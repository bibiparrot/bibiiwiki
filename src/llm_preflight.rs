use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::backend::BackendClient;
use crate::codex::{self, CodexPromptRequest};
use crate::config::Config;
use crate::gateway::{AppState, app as gateway_app};
use crate::protocol::ResponsesRequest;

const TEST_PROMPT: &str = "Who are you?";

/// Calls the configured provider deployment directly, without the local
/// Responses proxy or Codex adapter.
///
/// # Errors
///
/// Returns an error when the deployment is missing, unreachable, rejects the
/// request, or returns no text.
pub async fn probe_direct_llm(config: &Config) -> Result<String> {
    let model_name = config.codex_model();
    let deployment = config
        .model_list
        .iter()
        .find(|deployment| deployment.model_name == model_name)
        .with_context(|| format!("configured model {model_name:?} was not found"))?;
    let client = BackendClient::new(
        config.request_timeout(),
        config.server.default_max_output_tokens,
    )?;
    let completion = client
        .complete(deployment, &probe_request(config, model_name))
        .await
        .context("configured LLM did not answer")?;
    completion
        .text
        .filter(|text| !text.trim().is_empty())
        .context("configured LLM returned no text")
}

/// Starts the configured local Responses proxy and verifies that a request can
/// traverse it to the configured provider.
///
/// # Errors
///
/// Returns an error when the proxy cannot start, the provider call fails, or
/// the proxy returns invalid or empty output.
pub async fn probe_responses_proxy(config: &Config) -> Result<String> {
    let model = config.codex_model();
    let state = AppState::from_config(config).context("could not create proxy state")?;
    let listener = tokio::net::TcpListener::bind(config.server.bind)
        .await
        .with_context(|| {
            format!(
                "could not bind the configured diagnostic proxy port {}",
                config.server.bind
            )
        })?;
    let address = listener
        .local_addr()
        .context("could not read the diagnostic proxy address")?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, gateway_app(state))
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
    });

    let probe_result = async {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(config.request_timeout())
            .build()
            .context("could not build the proxy test client")?;
        let mut request = client
            .post(format!("http://{address}/v1/responses"))
            .json(&proxy_request_body(config, model));
        if let Some(api_key) = config.inbound_api_key()? {
            request = request.bearer_auth(api_key);
        }
        let response = request
            .send()
            .await
            .context("Responses proxy request failed")?;
        let status = response.status();
        let response_text = response
            .text()
            .await
            .context("could not read the Responses proxy reply")?;
        if !status.is_success() {
            bail!(
                "Responses proxy returned {status}: {}",
                truncate_reply(&response_text)
            );
        }
        let response: Value = serde_json::from_str(&response_text)
            .context("Responses proxy returned invalid JSON")?;
        responses_output_text(&response).context("Responses proxy returned no output text")
    }
    .await;

    let _ = shutdown_tx.send(());
    let server_result = match server.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(anyhow!("diagnostic proxy stopped with an error: {error}")),
        Err(error) => Err(anyhow!("diagnostic proxy task failed: {error}")),
    };
    let reply = probe_result?;
    server_result?;
    Ok(reply)
}

/// Verifies the Codex CLI adapter through its ephemeral local Responses proxy.
///
/// # Errors
///
/// Returns an error when Codex cannot launch, cannot call the proxy/provider,
/// or returns an empty final response.
pub async fn probe_codex_agent(config: Config, workspace: &Path) -> Result<String> {
    let result = codex::prompt(
        config,
        CodexPromptRequest {
            prompt: TEST_PROMPT.to_owned(),
            workspace: workspace.to_path_buf(),
            state_path: None,
            output_schema: None,
        },
    )
    .await
    .context("Codex adapter could not call the LLM through the Responses proxy")?;
    let reply = result.response.trim();
    if reply.is_empty() {
        bail!("Codex adapter returned no text through the Responses proxy");
    }
    Ok(reply.to_owned())
}

fn probe_request(config: &Config, model: &str) -> ResponsesRequest {
    ResponsesRequest {
        model: model.to_owned(),
        instructions: None,
        input: Value::String(TEST_PROMPT.to_owned()),
        text: None,
        tools: Vec::new(),
        tool_choice: None,
        parallel_tool_calls: None,
        stream: false,
        max_output_tokens: Some(256),
        temperature: None,
        top_p: None,
        reasoning: config
            .codex
            .reasoning_effort
            .as_deref()
            .map(|effort| json!({"effort": effort})),
    }
}

fn proxy_request_body(config: &Config, model: &str) -> Value {
    let mut body = json!({
        "model": model,
        "input": TEST_PROMPT,
        "stream": false,
        "max_output_tokens": 256
    });
    if let Some(effort) = config.codex.reasoning_effort.as_deref() {
        body["reasoning"] = json!({"effort": effort});
    }
    body
}

fn responses_output_text(response: &Value) -> Option<String> {
    let text = response
        .get("output")?
        .as_array()?
        .iter()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    (!text.trim().is_empty()).then_some(text)
}

fn truncate_reply(text: &str) -> String {
    const MAX_CHARS: usize = 800;
    let mut reply: String = text.trim().chars().take(MAX_CHARS).collect();
    if text.trim().chars().count() > MAX_CHARS {
        reply.push('…');
    }
    reply
}
