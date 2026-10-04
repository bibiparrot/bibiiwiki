use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};

static ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default = "empty_array")]
    pub input: Value,
    #[serde(default)]
    pub text: Option<Value>,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub reasoning: Option<Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolKind {
    Function,
    Custom,
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    pub kind: ToolKind,
}

#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

impl Usage {
    #[must_use]
    pub const fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

#[derive(Clone, Debug)]
pub struct Completion {
    pub model: String,
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

#[must_use]
pub fn provider_and_model(model: &str) -> (Option<&str>, &str) {
    model
        .split_once('/')
        .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
        .map_or((None, model), |(provider, model)| (Some(provider), model))
}

#[must_use]
pub fn tool_kinds(tools: &[Value]) -> HashMap<String, ToolKind> {
    tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name").and_then(Value::as_str)?;
            let kind = match tool.get("type").and_then(Value::as_str) {
                Some("custom") => ToolKind::Custom,
                Some("function") => ToolKind::Function,
                _ => return None,
            };
            Some((name.to_string(), kind))
        })
        .collect()
}

/// Converts a Responses request into an Anthropic Messages request body.
///
/// # Errors
///
/// Returns an error when an input item lacks fields required to preserve tool
/// call semantics or when `input` has an unsupported top-level shape.
#[allow(clippy::too_many_lines)]
pub fn to_anthropic_body(
    request: &ResponsesRequest,
    provider_model: &str,
    default_max_output_tokens: u64,
) -> Result<Value> {
    let mut system = Vec::new();
    if let Some(instructions) = request
        .instructions
        .as_deref()
        .filter(|instructions| !instructions.trim().is_empty())
    {
        system.push(json!({"type": "text", "text": instructions}));
    }

    let mut messages = Vec::new();
    for item in input_items(&request.input)? {
        if let Some(text) = item.as_str() {
            push_anthropic_message(
                &mut messages,
                "user",
                vec![json!({"type": "text", "text": text})],
            );
            continue;
        }
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        match item_type {
            "message" => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                let blocks = anthropic_content_blocks(item.get("content").unwrap_or(&Value::Null));
                if role == "developer" || role == "system" {
                    system.extend(blocks);
                } else if !blocks.is_empty() {
                    push_anthropic_message(&mut messages, role, blocks);
                }
            }
            "function_call" => {
                let name = required_string(item, "name")?;
                let call_id = required_string(item, "call_id")?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                let input = serde_json::from_str(arguments)
                    .unwrap_or_else(|_| json!({"raw_arguments": arguments}));
                push_anthropic_message(
                    &mut messages,
                    "assistant",
                    vec![json!({
                        "type": "tool_use",
                        "id": call_id,
                        "name": name,
                        "input": input,
                    })],
                );
            }
            "custom_tool_call" => {
                let name = required_string(item, "name")?;
                let call_id = required_string(item, "call_id")?;
                let input = item
                    .get("input")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                push_anthropic_message(
                    &mut messages,
                    "assistant",
                    vec![json!({
                        "type": "tool_use",
                        "id": call_id,
                        "name": name,
                        "input": {"input": input},
                    })],
                );
            }
            "function_call_output" | "custom_tool_call_output" => {
                let call_id = required_string(item, "call_id")?;
                let output = output_to_text(item.get("output").unwrap_or(&Value::Null));
                push_anthropic_message(
                    &mut messages,
                    "user",
                    vec![json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": output,
                    })],
                );
            }
            "reasoning" | "compaction" => {}
            other => push_anthropic_message(
                &mut messages,
                "user",
                vec![json!({
                    "type": "text",
                    "text": format!("[Unsupported Responses input item {other}: {item}]")
                })],
            ),
        }
    }
    if messages.is_empty() {
        push_anthropic_message(
            &mut messages,
            "user",
            vec![json!({"type": "text", "text": ""})],
        );
    }

    let mut body = Map::new();
    body.insert("model".to_string(), json!(provider_model));
    body.insert("messages".to_string(), Value::Array(messages));
    body.insert(
        "max_tokens".to_string(),
        json!(
            request
                .max_output_tokens
                .unwrap_or(default_max_output_tokens)
        ),
    );
    body.insert("stream".to_string(), Value::Bool(false));
    if !system.is_empty() {
        body.insert("system".to_string(), Value::Array(system));
    }
    let tools = anthropic_tools(&request.tools);
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(tool_choice) = request.tool_choice.as_ref().and_then(anthropic_tool_choice) {
        body.insert("tool_choice".to_string(), tool_choice);
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        body.insert("top_p".to_string(), json!(top_p));
    }
    Ok(Value::Object(body))
}

fn chat_response_format(text: Option<&Value>) -> Option<Value> {
    let format = text?.get("format")?;
    match format.get("type").and_then(Value::as_str)? {
        "json_schema" => {
            let mut schema = format.as_object()?.clone();
            schema.remove("type");
            Some(json!({"type": "json_schema", "json_schema": schema}))
        }
        "json_object" => Some(json!({"type": "json_object"})),
        _ => None,
    }
}

/// Converts a Responses request into an `OpenAI` chat-completions request body.
///
/// # Errors
///
/// Returns an error when an input item lacks fields required to preserve tool
/// call semantics or when `input` has an unsupported top-level shape.
#[allow(clippy::too_many_lines)]
pub fn to_chat_completions_body(
    request: &ResponsesRequest,
    provider_model: &str,
    default_max_output_tokens: u64,
) -> Result<Value> {
    let mut messages = Vec::new();
    if let Some(instructions) = request
        .instructions
        .as_deref()
        .filter(|instructions| !instructions.trim().is_empty())
    {
        messages.push(json!({"role": "system", "content": instructions}));
    }

    for item in input_items(&request.input)? {
        if let Some(text) = item.as_str() {
            messages.push(json!({"role": "user", "content": text}));
            continue;
        }
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        match item_type {
            "message" => {
                let mut role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                if role == "developer" {
                    role = "system";
                }
                let content = chat_content(item.get("content").unwrap_or(&Value::Null));
                messages.push(json!({"role": role, "content": content}));
            }
            "function_call" | "custom_tool_call" => {
                let name = required_string(item, "name")?;
                let call_id = required_string(item, "call_id")?;
                let arguments = if item_type == "custom_tool_call" {
                    json!({"input": item.get("input").and_then(Value::as_str).unwrap_or_default()})
                        .to_string()
                } else {
                    item.get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}")
                        .to_string()
                };
                messages.push(json!({
                    "role": "assistant",
                    "content": Value::Null,
                    "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments}
                    }]
                }));
            }
            "function_call_output" | "custom_tool_call_output" => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": required_string(item, "call_id")?,
                    "content": output_to_text(item.get("output").unwrap_or(&Value::Null)),
                }));
            }
            "reasoning" | "compaction" => {}
            other => messages.push(json!({
                "role": "user",
                "content": format!("[Unsupported Responses input item {other}: {item}]")
            })),
        }
    }
    if messages.is_empty() {
        messages.push(json!({"role": "user", "content": ""}));
    }

    let mut body = Map::new();
    body.insert("model".to_string(), json!(provider_model));
    body.insert("messages".to_string(), Value::Array(messages));
    body.insert("stream".to_string(), Value::Bool(false));
    body.insert(
        "max_tokens".to_string(),
        json!(
            request
                .max_output_tokens
                .unwrap_or(default_max_output_tokens)
        ),
    );
    let tools = chat_tools(&request.tools);
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(tool_choice) = request.tool_choice.as_ref().and_then(chat_tool_choice) {
        body.insert("tool_choice".to_string(), tool_choice);
    }
    if let Some(parallel) = request.parallel_tool_calls {
        body.insert("parallel_tool_calls".to_string(), Value::Bool(parallel));
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        body.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(response_format) = chat_response_format(request.text.as_ref()) {
        body.insert("response_format".to_string(), response_format);
    }
    if let Some(effort) = request
        .reasoning
        .as_ref()
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
    {
        body.insert("reasoning_effort".to_string(), json!(effort));
    }
    Ok(Value::Object(body))
}

#[must_use]
pub fn output_items(completion: &Completion, response_id: &str) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some(text) = completion.text.as_deref().filter(|text| !text.is_empty()) {
        items.push(json!({
            "id": next_id("msg"),
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
        }));
    }
    for call in &completion.tool_calls {
        let id = next_id(match call.kind {
            ToolKind::Function => "fc",
            ToolKind::Custom => "ctc",
        });
        let item = match call.kind {
            ToolKind::Function => json!({
                "id": id,
                "type": "function_call",
                "name": call.name,
                "arguments": call.arguments,
                "call_id": call.call_id,
            }),
            ToolKind::Custom => {
                let input = serde_json::from_str::<Value>(&call.arguments)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("input")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| call.arguments.clone());
                json!({
                    "id": id,
                    "type": "custom_tool_call",
                    "name": call.name,
                    "input": input,
                    "call_id": call.call_id,
                })
            }
        };
        items.push(item);
    }
    let _ = response_id;
    items
}

#[must_use]
pub fn response_document(
    request_model: &str,
    response_id: &str,
    completion: &Completion,
    items: &[Value],
) -> Value {
    json!({
        "id": response_id,
        "object": "response",
        "created_at": unix_seconds(),
        "status": "completed",
        "model": request_model,
        "output": items,
        "usage": {
            "input_tokens": completion.usage.input_tokens,
            "input_tokens_details": {
                "cached_tokens": completion.usage.cached_input_tokens
            },
            "output_tokens": completion.usage.output_tokens,
            "output_tokens_details": {
                "reasoning_tokens": completion.usage.reasoning_output_tokens
            },
            "total_tokens": completion.usage.total_tokens()
        }
    })
}

#[must_use]
pub fn stream_events(
    request_model: &str,
    response_id: &str,
    completion: &Completion,
) -> Vec<Value> {
    let items = output_items(completion, response_id);
    let mut events = vec![created_event(request_model, response_id)];
    events.extend(items.iter().map(|item| {
        json!({
            "type": "response.output_item.done",
            "item": item
        })
    }));
    events.push(json!({
        "type": "response.completed",
        "response": response_document(request_model, response_id, completion, &items)
    }));
    events
}

#[must_use]
pub fn created_event(request_model: &str, response_id: &str) -> Value {
    json!({
        "type": "response.created",
        "response": {
            "id": response_id,
            "object": "response",
            "created_at": unix_seconds(),
            "status": "in_progress",
            "model": request_model,
            "output": []
        }
    })
}

#[must_use]
pub fn failed_event(response_id: &str, message: &str) -> Value {
    json!({
        "type": "response.failed",
        "response": {
            "id": response_id,
            "status": "failed",
            "error": {
                "type": "server_error",
                "code": "bibiiwiki_upstream_error",
                "message": message
            }
        }
    })
}

#[must_use]
pub fn new_response_id() -> String {
    next_id("resp")
}

fn input_items(input: &Value) -> Result<Vec<&Value>> {
    match input {
        Value::Array(items) => Ok(items.iter().collect()),
        Value::String(_) => Ok(vec![input]),
        Value::Null => Ok(Vec::new()),
        _ => bail!("Responses input must be a string or array"),
    }
}

fn required_string<'a>(object: &'a Value, field: &str) -> Result<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("Responses item is missing string field {field:?}"))
}

fn anthropic_content_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) => vec![json!({"type": "text", "text": text})],
        Value::Array(parts) => parts.iter().filter_map(anthropic_content_block).collect(),
        Value::Null => Vec::new(),
        other => vec![json!({"type": "text", "text": other.to_string()})],
    }
}

fn anthropic_content_block(part: &Value) -> Option<Value> {
    match part.get("type").and_then(Value::as_str) {
        Some("input_text" | "output_text" | "text") => part
            .get("text")
            .and_then(Value::as_str)
            .map(|text| json!({"type": "text", "text": text})),
        Some("input_image") => {
            let image_url = part.get("image_url").and_then(Value::as_str)?;
            if let Some((metadata, data)) = image_url.split_once(',')
                && let Some(media_type) = metadata
                    .strip_prefix("data:")
                    .and_then(|value| value.strip_suffix(";base64"))
            {
                Some(json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": media_type, "data": data}
                }))
            } else {
                Some(json!({
                    "type": "image",
                    "source": {"type": "url", "url": image_url}
                }))
            }
        }
        _ => None,
    }
}

fn push_anthropic_message(messages: &mut Vec<Value>, role: &str, mut blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.append(&mut blocks);
        return;
    }
    messages.push(json!({"role": role, "content": blocks}));
}

fn chat_content(content: &Value) -> Value {
    match content {
        Value::String(text) => json!(text),
        Value::Array(parts) => {
            let mapped: Vec<Value> = parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => part
                        .get("text")
                        .and_then(Value::as_str)
                        .map(|text| json!({"type": "text", "text": text})),
                    Some("input_image") => part.get("image_url").and_then(Value::as_str).map(
                        |url| {
                            json!({
                                "type": "image_url",
                                "image_url": {"url": url, "detail": part.get("detail").and_then(Value::as_str).unwrap_or("auto")}
                            })
                        },
                    ),
                    _ => None,
                })
                .collect();
            Value::Array(mapped)
        }
        Value::Null => Value::String(String::new()),
        other => Value::String(other.to_string()),
    }
}

fn output_to_text(output: &Value) -> String {
    match output {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            if texts.is_empty() {
                output.to_string()
            } else {
                texts.join("\n")
            }
        }
        other => other.to_string(),
    }
}

fn anthropic_tools(tools: &[Value]) -> Vec<Value> {
    tools.iter().filter_map(|tool| {
        let kind = tool.get("type").and_then(Value::as_str)?;
        let name = tool.get("name").and_then(Value::as_str)?;
        let description = tool.get("description").and_then(Value::as_str).unwrap_or("");
        match kind {
            "function" => Some(json!({
                "name": name,
                "description": description,
                "input_schema": tool.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"}))
            })),
            "custom" => Some(json!({
                "name": name,
                "description": description,
                "input_schema": {
                    "type": "object",
                    "properties": {"input": {"type": "string"}},
                    "required": ["input"],
                    "additionalProperties": false
                }
            })),
            _ => None,
        }
    }).collect()
}

fn chat_tools(tools: &[Value]) -> Vec<Value> {
    anthropic_tools(tools)
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool["name"],
                    "description": tool["description"],
                    "parameters": tool["input_schema"]
                }
            })
        })
        .collect()
}

fn anthropic_tool_choice(choice: &Value) -> Option<Value> {
    match choice {
        Value::String(value) if value == "auto" => Some(json!({"type": "auto"})),
        Value::String(value) if value == "required" => Some(json!({"type": "any"})),
        Value::String(value) if value == "none" => None,
        Value::Object(object) => object
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({"type": "tool", "name": name})),
        _ => None,
    }
}

fn chat_tool_choice(choice: &Value) -> Option<Value> {
    match choice {
        Value::String(value) => Some(json!(value)),
        Value::Object(object) => object
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({"type": "function", "function": {"name": name}})),
        _ => None,
    }
}

fn next_id(prefix: &str) -> String {
    let sequence = ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{prefix}_{nanos:x}{sequence:x}")
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

const fn empty_array() -> Value {
    Value::Array(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ResponsesRequest {
        serde_json::from_value(json!({
            "model": "claude",
            "instructions": "Be useful.",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "List files"}]},
                {"type": "function_call", "name": "shell", "call_id": "call_1", "arguments": "{\"cmd\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "a.txt"}
            ],
            "tools": [{"type": "function", "name": "shell", "description": "Run a command", "parameters": {"type": "object"}}],
            "stream": true
        }))
        .expect("request should deserialize")
    }

    #[test]
    fn maps_responses_to_anthropic_messages() {
        let body = to_anthropic_body(&request(), "claude-test", 4096).expect("translation");
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["system"][0]["text"], "Be useful.");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["tools"][0]["name"], "shell");
    }

    #[test]
    fn maps_responses_to_chat_completions() {
        let mut request = request();
        request.text = Some(json!({"format": {
            "type": "json_schema",
            "name": "wiki_analysis",
            "strict": true,
            "schema": {"type": "object", "properties": {"summary": {"type": "string"}}}
        }}));
        let body = to_chat_completions_body(&request, "test-model", 4096).expect("translation");
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["name"],
            "shell"
        );
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(
            body["response_format"]["json_schema"]["name"],
            "wiki_analysis"
        );
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["properties"]["summary"]["type"],
            "string"
        );
    }

    #[test]
    fn emits_codex_consumable_terminal_events() {
        let completion = Completion {
            model: "test".to_string(),
            text: Some("done".to_string()),
            tool_calls: Vec::new(),
            usage: Usage::default(),
        };
        let events = stream_events("alias", "resp_1", &completion);
        assert_eq!(events[0]["type"], "response.created");
        assert_eq!(events[1]["type"], "response.output_item.done");
        assert_eq!(events[2]["type"], "response.completed");
        assert_eq!(events[2]["response"]["id"], "resp_1");
    }
}
