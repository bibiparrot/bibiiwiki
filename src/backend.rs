use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use litellm_core::messages::types::MessagesRequest;
use litellm_core::router::Deployment;
use reqwest::{Client, StatusCode};
use serde_json::Value;

use crate::protocol::{
    Completion, ResponsesRequest, ToolCall, ToolKind, Usage, provider_and_model, to_anthropic_body,
    to_chat_completions_body, tool_kinds,
};

const OLLAMA_MAX_OUTPUT_TOKENS: u64 = 2048;
const OLLAMA_CONTEXT_TOKENS: u64 = 32_768;
const OLLAMA_KEEP_ALIVE: &str = "15m";

#[derive(Clone, Debug)]
pub struct BackendClient {
    http: Client,
    timeout: Duration,
    default_max_output_tokens: u64,
}

impl BackendClient {
    /// Builds the shared asynchronous provider client.
    ///
    /// # Errors
    ///
    /// Returns an error when the TLS/HTTP client cannot be constructed.
    pub fn new(timeout: Duration, default_max_output_tokens: u64) -> Result<Self> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(timeout)
            .build()
            .context("failed to build upstream HTTP client")?;
        Ok(Self {
            http,
            timeout,
            default_max_output_tokens,
        })
    }

    /// Executes one normalized completion against the selected deployment.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid translations, provider authentication,
    /// network failures, non-success responses, or malformed provider output.
    pub async fn complete(
        &self,
        deployment: &Deployment,
        request: &ResponsesRequest,
    ) -> Result<Completion> {
        let (provider, provider_model) = provider_and_model(&deployment.litellm_params.model);
        match provider {
            Some("anthropic" | "azure_ai") => {
                self.complete_litellm_messages(deployment, request, provider, provider_model)
                    .await
            }
            Some("ollama") => {
                self.complete_ollama_native(deployment, request, provider_model)
                    .await
            }
            _ => {
                self.complete_openai_compatible(deployment, request, provider, provider_model)
                    .await
            }
        }
    }

    async fn complete_litellm_messages(
        &self,
        deployment: &Deployment,
        request: &ResponsesRequest,
        provider: Option<&str>,
        provider_model: &str,
    ) -> Result<Completion> {
        let body = to_anthropic_body(request, provider_model, self.default_max_output_tokens)?;
        let response = litellm_core::messages::messages(MessagesRequest {
            model: &deployment.litellm_params.model,
            body,
            api_key: deployment.litellm_params.api_key.as_deref(),
            api_base: deployment.litellm_params.api_base.as_deref(),
            custom_llm_provider: provider,
            extra_headers: None,
            timeout: Some(self.timeout),
        })
        .await
        .map_err(|error| anyhow::anyhow!("LiteLLM Rust messages call failed: {error}"))?;

        let kinds = tool_kinds(&request.tools);
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for block in response.content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(value) = block.get("text").and_then(Value::as_str) {
                        text.push_str(value);
                    }
                }
                Some("tool_use") => {
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .context("Anthropic tool_use block is missing name")?;
                    let call_id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .context("Anthropic tool_use block is missing id")?;
                    let arguments = block.get("input").cloned().unwrap_or(Value::Null);
                    tool_calls.push(ToolCall {
                        call_id: call_id.to_string(),
                        name: name.to_string(),
                        arguments: arguments.to_string(),
                        kind: kinds.get(name).copied().unwrap_or(ToolKind::Function),
                    });
                }
                _ => {}
            }
        }

        Ok(Completion {
            model: response.model,
            text: (!text.is_empty()).then_some(text),
            tool_calls,
            usage: anthropic_usage(response.usage.as_ref()),
        })
    }

    async fn complete_ollama_native(
        &self,
        deployment: &Deployment,
        request: &ResponsesRequest,
        provider_model: &str,
    ) -> Result<Completion> {
        let url = ollama_native_chat_url(deployment.litellm_params.api_base.as_deref())?;
        let mut body =
            ollama_native_request_body(request, provider_model, self.default_max_output_tokens)?;
        let api_key = deployment.litellm_params.api_key.as_deref();
        let (mut status, mut response_body) = self.send_ollama_native(&url, &body, api_key).await?;
        if !status.is_success() && downgrade_ollama_native_schema(&mut body, &response_body) {
            tracing::warn!(
                model = provider_model,
                "Ollama rejected the strict JSON schema; retrying with JSON-object grammar"
            );
            (status, response_body) = self.send_ollama_native(&url, &body, api_key).await?;
        }
        if !status.is_success() {
            bail!(
                "Ollama native upstream returned {status}: {}",
                truncate(&response_body, 4096)
            );
        }
        let value: Value = serde_json::from_str(&response_body)
            .context("Ollama native upstream returned invalid JSON")?;
        parse_ollama_chat_response(&value, provider_model, &tool_kinds(&request.tools))
    }

    async fn complete_openai_compatible(
        &self,
        deployment: &Deployment,
        request: &ResponsesRequest,
        provider: Option<&str>,
        provider_model: &str,
    ) -> Result<Completion> {
        let url = chat_completions_url(deployment.litellm_params.api_base.as_deref(), provider)?;
        let mut body = chat_request_body(
            request,
            provider_model,
            self.default_max_output_tokens,
            provider,
        )?;
        let api_key = deployment.litellm_params.api_key.as_deref();
        let (mut status, mut response_body) =
            self.send_openai_compatible(&url, &body, api_key).await?;
        if provider == Some("ollama")
            && !status.is_success()
            && downgrade_ollama_json_schema(&mut body, &response_body)
        {
            tracing::warn!(
                model = provider_model,
                "Ollama rejected the strict JSON schema; retrying with JSON-object grammar"
            );
            (status, response_body) = self.send_openai_compatible(&url, &body, api_key).await?;
        }
        if !status.is_success() {
            bail!(
                "OpenAI-compatible upstream returned {status}: {}",
                truncate(&response_body, 4096)
            );
        }
        let value: Value = serde_json::from_str(&response_body)
            .context("OpenAI-compatible upstream returned invalid JSON")?;
        parse_chat_completion(&value, provider_model, &tool_kinds(&request.tools))
    }

    async fn send_openai_compatible(
        &self,
        url: &str,
        body: &Value,
        api_key: Option<&str>,
    ) -> Result<(StatusCode, String)> {
        let mut builder = self.http.post(url).json(body);
        if let Some(api_key) = api_key {
            builder = builder.bearer_auth(api_key);
        }
        let response = builder
            .send()
            .await
            .context("OpenAI-compatible chat completions request failed")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read chat completions response")?;
        Ok((status, body))
    }

    async fn send_ollama_native(
        &self,
        url: &str,
        body: &Value,
        api_key: Option<&str>,
    ) -> Result<(StatusCode, String)> {
        let mut builder = self.http.post(url).json(body);
        if let Some(api_key) = api_key {
            builder = builder.bearer_auth(api_key);
        }
        let response = builder
            .send()
            .await
            .context("Ollama native chat request failed")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read Ollama native chat response")?;
        Ok((status, body))
    }
}

fn ollama_native_request_body(
    request: &ResponsesRequest,
    provider_model: &str,
    default_max_output_tokens: u64,
) -> Result<Value> {
    let mut body = to_chat_completions_body(request, provider_model, default_max_output_tokens)?;
    let object = body
        .as_object_mut()
        .expect("chat request body is always an object");
    if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            let content = chat_response_text(message.get("content")).unwrap_or_default();
            if let Some(message) = message.as_object_mut() {
                message.insert("content".to_string(), Value::String(content));
            }
        }
    }
    let requested_output = object
        .remove("max_tokens")
        .and_then(|value| value.as_u64())
        .unwrap_or(default_max_output_tokens)
        .min(OLLAMA_MAX_OUTPUT_TOKENS);
    let mut options = serde_json::Map::from_iter([
        ("num_ctx".to_string(), Value::from(OLLAMA_CONTEXT_TOKENS)),
        ("num_predict".to_string(), Value::from(requested_output)),
    ]);
    for name in ["temperature", "top_p"] {
        if let Some(value) = object.remove(name) {
            options.insert(name.to_string(), value);
        }
    }
    object.insert("options".to_string(), Value::Object(options));
    object.insert(
        "keep_alive".to_string(),
        Value::String(OLLAMA_KEEP_ALIVE.to_string()),
    );
    if !ollama_thinking_requested(request) {
        object.insert("think".to_string(), Value::Bool(false));
    }
    if let Some(response_format) = object.remove("response_format") {
        let format = match response_format.get("type").and_then(Value::as_str) {
            Some("json_schema") => response_format
                .get("json_schema")
                .and_then(|value| value.get("schema"))
                .cloned()
                .unwrap_or_else(|| Value::String("json".to_string())),
            Some("json_object") => Value::String("json".to_string()),
            _ => Value::Null,
        };
        if !format.is_null() {
            object.insert("format".to_string(), format);
        }
    }
    object.remove("reasoning_effort");
    object.remove("parallel_tool_calls");
    object.remove("tool_choice");
    Ok(body)
}

fn ollama_native_chat_url(api_base: Option<&str>) -> Result<String> {
    let base = api_base
        .map(str::trim)
        .filter(|base| !base.is_empty())
        .context("litellm_params.api_base is required for the Ollama provider")?
        .trim_end_matches('/');
    let base = base
        .strip_suffix("/v1/chat/completions")
        .or_else(|| base.strip_suffix("/v1"))
        .or_else(|| base.strip_suffix("/api/chat"))
        .or_else(|| base.strip_suffix("/api"))
        .unwrap_or(base);
    Ok(format!("{base}/api/chat"))
}

fn parse_ollama_chat_response(
    response: &Value,
    fallback_model: &str,
    kinds: &HashMap<String, ToolKind>,
) -> Result<Completion> {
    let message = response
        .get("message")
        .context("Ollama native response is missing message")?;
    let prompt_tokens = response
        .get("prompt_eval_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = response
        .get("eval_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    parse_chat_completion(
        &serde_json::json!({
            "model": response.get("model").and_then(Value::as_str).unwrap_or(fallback_model),
            "choices": [{"message": message}],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens
            }
        }),
        fallback_model,
        kinds,
    )
}

fn downgrade_ollama_native_schema(body: &mut Value, error: &str) -> bool {
    if !error
        .to_ascii_lowercase()
        .contains("failed to parse grammar")
    {
        return false;
    }
    let Some(format) = body.get_mut("format") else {
        return false;
    };
    if !format.is_object() {
        return false;
    }
    *format = Value::String("json".to_string());
    true
}

fn downgrade_ollama_json_schema(body: &mut Value, error: &str) -> bool {
    if !error
        .to_ascii_lowercase()
        .contains("failed to parse grammar")
    {
        return false;
    }
    let Some(response_format) = body.get_mut("response_format") else {
        return false;
    };
    if response_format.get("type").and_then(Value::as_str) != Some("json_schema") {
        return false;
    }
    *response_format = serde_json::json!({"type": "json_object"});
    true
}

fn chat_request_body(
    request: &ResponsesRequest,
    provider_model: &str,
    default_max_output_tokens: u64,
    provider: Option<&str>,
) -> Result<Value> {
    let mut body = to_chat_completions_body(request, provider_model, default_max_output_tokens)?;
    if provider == Some("ollama") {
        let object = body
            .as_object_mut()
            .expect("chat request body is always an object");
        let requested = object
            .get("max_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(default_max_output_tokens);
        object.insert(
            "max_tokens".to_string(),
            Value::from(requested.min(OLLAMA_MAX_OUTPUT_TOKENS)),
        );
        if !ollama_thinking_requested(request) {
            // Ollama's OpenAI-compatible endpoint currently honors
            // `reasoning_effort: none`; the native `think: false` extension is
            // ignored on this route by reasoning-capable models such as
            // ornith. Send both so native-compatible proxies also behave.
            object.insert(
                "reasoning_effort".to_string(),
                Value::String("none".to_string()),
            );
            object.insert("think".to_string(), Value::Bool(false));
        }
    }
    Ok(body)
}

fn ollama_thinking_requested(request: &ResponsesRequest) -> bool {
    request
        .reasoning
        .as_ref()
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
        .is_some_and(|effort| effort != "none")
}

fn chat_completions_url(api_base: Option<&str>, provider: Option<&str>) -> Result<String> {
    let base = match api_base.map(str::trim).filter(|base| !base.is_empty()) {
        Some(base) => base,
        None if provider == Some("openai") => "https://api.openai.com/v1",
        None => bail!(
            "litellm_params.api_base is required for provider {}; point it at an OpenAI-compatible /v1 endpoint",
            provider.unwrap_or("without a prefix")
        ),
    };
    let base = base.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        Ok(base.to_string())
    } else if base.ends_with("/v1") {
        Ok(format!("{base}/chat/completions"))
    } else {
        Ok(format!("{base}/v1/chat/completions"))
    }
}

fn parse_chat_completion(
    response: &Value,
    fallback_model: &str,
    kinds: &HashMap<String, ToolKind>,
) -> Result<Completion> {
    let message = response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .context("chat completions response is missing choices[0].message")?;
    let text = chat_response_text(message.get("content"));
    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (index, call) in calls.iter().enumerate() {
            let function = call.get("function").unwrap_or(call);
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .context("chat tool call is missing function.name")?;
            let arguments = function.get("arguments").map_or_else(
                || "{}".to_string(),
                |value| {
                    value
                        .as_str()
                        .map_or_else(|| value.to_string(), str::to_string)
                },
            );
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .map_or_else(|| format!("call_{index}"), str::to_string);
            tool_calls.push(ToolCall {
                call_id,
                name: name.to_string(),
                arguments,
                kind: kinds.get(name).copied().unwrap_or(ToolKind::Function),
            });
        }
    } else if let Some(function) = message.get("function_call") {
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .context("chat function_call is missing name")?;
        tool_calls.push(ToolCall {
            call_id: "call_0".to_string(),
            name: name.to_string(),
            arguments: function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_string(),
            kind: kinds.get(name).copied().unwrap_or(ToolKind::Function),
        });
    }
    if text.is_none() && tool_calls.is_empty() {
        bail!("chat completions response contains neither text nor tool calls");
    }

    Ok(Completion {
        model: response
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(fallback_model)
            .to_string(),
        text,
        tool_calls,
        usage: chat_usage(response.get("usage")),
    })
}

fn chat_response_text(content: Option<&Value>) -> Option<String> {
    match content {
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        Some(Value::Array(parts)) => {
            let text = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.get("content").and_then(Value::as_str))
                })
                .collect::<String>();
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn anthropic_usage(usage: Option<&Value>) -> Usage {
    let field = |name| {
        usage
            .and_then(|value| value.get(name))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Usage {
        input_tokens: field("input_tokens")
            + field("cache_creation_input_tokens")
            + field("cache_read_input_tokens"),
        cached_input_tokens: field("cache_read_input_tokens"),
        output_tokens: field("output_tokens"),
        reasoning_output_tokens: 0,
    }
}

fn chat_usage(usage: Option<&Value>) -> Usage {
    let field = |name| {
        usage
            .and_then(|value| value.get(name))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Usage {
        input_tokens: field("prompt_tokens"),
        cached_input_tokens: usage
            .and_then(|value| value.get("prompt_tokens_details"))
            .and_then(|value| value.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: field("completion_tokens"),
        reasoning_output_tokens: usage
            .and_then(|value| value.get("completion_tokens_details"))
            .and_then(|value| value.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_expected_chat_completion_urls() {
        assert_eq!(
            chat_completions_url(Some("http://localhost:4000"), Some("litellm")).unwrap(),
            "http://localhost:4000/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url(Some("http://localhost:11434/v1"), Some("ollama")).unwrap(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn native_ollama_request_bounds_context_and_generation() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "ornith-1.5:9b",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Return JSON"}]
            }],
            "max_output_tokens": 512,
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "answer",
                    "schema": {
                        "type": "object",
                        "properties": {"answer": {"type": "string"}},
                        "required": ["answer"],
                        "additionalProperties": false
                    }
                }
            }
        }))
        .unwrap();

        let body = ollama_native_request_body(&request, "ornith-1.5:9b", 4096).unwrap();

        assert_eq!(body["options"]["num_ctx"], OLLAMA_CONTEXT_TOKENS);
        assert_eq!(body["options"]["num_predict"], 512);
        assert_eq!(body["messages"][0]["content"], "Return JSON");
        assert_eq!(body["format"]["type"], "object");
        assert_eq!(body["think"], false);
        assert_eq!(body["keep_alive"], OLLAMA_KEEP_ALIVE);
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("response_format").is_none());
    }

    #[test]
    fn native_ollama_url_replaces_the_openai_v1_suffix() {
        assert_eq!(
            ollama_native_chat_url(Some("http://127.0.0.1:11434/v1")).unwrap(),
            "http://127.0.0.1:11434/api/chat"
        );
    }

    #[test]
    fn native_ollama_response_preserves_text_tools_and_usage() {
        let response = json!({
            "model": "ornith-1.5:9b",
            "message": {
                "role": "assistant",
                "content": "done",
                "tool_calls": [{"function": {"name": "shell", "arguments": {"cmd": "pwd"}}}]
            },
            "prompt_eval_count": 10,
            "eval_count": 3
        });

        let parsed = parse_ollama_chat_response(
            &response,
            "ornith-1.5:9b",
            &HashMap::from([("shell".to_string(), ToolKind::Function)]),
        )
        .unwrap();

        assert_eq!(parsed.text.as_deref(), Some("done"));
        assert_eq!(parsed.tool_calls[0].arguments, r#"{"cmd":"pwd"}"#);
        assert_eq!(parsed.usage.total_tokens(), 13);
    }

    #[test]
    fn parses_chat_tool_call() {
        let response = json!({
            "model": "local",
            "choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1", "type": "function", "function": {"name": "shell", "arguments": "{\"cmd\":\"pwd\"}"}
            }]}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3}
        });
        let parsed = parse_chat_completion(&response, "fallback", &HashMap::new()).unwrap();
        assert_eq!(parsed.tool_calls[0].name, "shell");
        assert_eq!(parsed.usage.total_tokens(), 13);
    }

    #[test]
    fn disables_ollama_thinking_when_reasoning_is_not_requested() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "ornith-1.5:9b",
            "input": "Return a short answer"
        }))
        .unwrap();

        let body = chat_request_body(&request, "ornith-1.5:9b", 4096, Some("ollama")).unwrap();

        assert_eq!(body["think"], false);
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["max_tokens"], OLLAMA_MAX_OUTPUT_TOKENS);
    }

    #[test]
    fn preserves_smaller_requested_ollama_output_limit() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "ornith-1.5:9b",
            "input": "Return JSON",
            "max_output_tokens": 512
        }))
        .unwrap();

        let body = chat_request_body(&request, "ornith-1.5:9b", 4096, Some("ollama")).unwrap();

        assert_eq!(body["max_tokens"], 512);
    }

    #[test]
    fn disables_ollama_thinking_for_explicit_none_effort() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "ornith-1.5:9b",
            "input": "Return a short answer",
            "reasoning": {"effort": "none"}
        }))
        .unwrap();

        let body = chat_request_body(&request, "ornith-1.5:9b", 4096, Some("ollama")).unwrap();

        assert_eq!(body["think"], false);
        assert_eq!(body["reasoning_effort"], "none");
    }

    #[test]
    fn preserves_requested_ollama_thinking() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "reasoning-local",
            "input": "Think carefully",
            "reasoning": {"effort": "high"}
        }))
        .unwrap();

        let body = chat_request_body(&request, "reasoning-local", 4096, Some("ollama")).unwrap();

        assert!(body.get("think").is_none());
    }

    #[test]
    fn downgrades_only_ollama_grammar_failures_to_json_object_mode() {
        let mut body = json!({
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": "analysis", "schema": {"type": "object"}}
            }
        });

        assert!(downgrade_ollama_json_schema(
            &mut body,
            "Failed to initialize samplers: failed to parse grammar"
        ));
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
        assert!(!downgrade_ollama_json_schema(
            &mut body,
            "some other bad request"
        ));
    }
}
