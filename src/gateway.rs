use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use litellm_core::router::{Deployment, Router as ModelRouter};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::backend::BackendClient;
use crate::config::Config;
use crate::protocol::{
    ResponsesRequest, created_event, failed_event, new_response_id, output_items,
    response_document, stream_events,
};

const SEMANTIC_MAX_OUTPUT_TOKENS: u64 = 2048;

#[derive(Clone, Debug)]
pub struct AppState {
    router: Arc<ModelRouter>,
    deployments: Arc<Vec<Deployment>>,
    backend: BackendClient,
    inbound_api_key: Option<Arc<str>>,
    strip_tools: bool,
    semantic_output_limit: Option<u64>,
}

impl AppState {
    /// Builds immutable gateway state and the shared provider client.
    ///
    /// # Errors
    ///
    /// Returns an error when inbound authentication or the HTTP client cannot
    /// be initialized.
    pub fn from_config(config: &Config) -> Result<Self> {
        let inbound_api_key = config.inbound_api_key()?.map(Arc::from);
        let deployments = Arc::new(config.model_list.clone());
        Ok(Self {
            router: Arc::new(ModelRouter::new(config.model_list.clone())),
            deployments,
            backend: BackendClient::new(
                config.request_timeout(),
                config.server.default_max_output_tokens,
            )?,
            inbound_api_key,
            strip_tools: false,
            semantic_output_limit: None,
        })
    }

    #[must_use]
    pub fn for_semantic_prompts(mut self) -> Self {
        self.strip_tools = true;
        self.semantic_output_limit = Some(SEMANTIC_MAX_OUTPUT_TOKENS);
        self
    }

    fn deployment(&self, model: &str) -> Result<Deployment, ApiError> {
        self.router
            .get_available_deployment(model)
            .cloned()
            .ok_or_else(|| {
                ApiError::not_found(format!("no deployment configured for model {model:?}"))
            })
    }
}

pub fn app(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/models", get(models))
        .route("/v1/responses", post(responses))
        .route("/responses", post(responses))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok", "service": "bibiiwiki"}))
}

async fn models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    tracing::debug!("serving configured model list");
    let data: Vec<Value> = state
        .deployments
        .iter()
        .map(|deployment| {
            json!({
                "id": deployment.model_name,
                "object": "model",
                "owned_by": "bibiiwiki"
            })
        })
        .collect();
    Ok(Json(json!({"object": "list", "data": data})))
}

async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request): Json<ResponsesRequest>,
) -> Result<Response, ApiError> {
    authorize(&state, &headers)?;
    if request.model.trim().is_empty() {
        return Err(ApiError::bad_request("model must not be empty"));
    }
    if state.strip_tools {
        request.tools.clear();
        request.tool_choice = None;
        request.parallel_tool_calls = None;
    }
    if let Some(limit) = state.semantic_output_limit {
        request.max_output_tokens = Some(
            request
                .max_output_tokens
                .map_or(limit, |requested| requested.min(limit)),
        );
    }
    tracing::debug!(model = %request.model, stream = request.stream, "received Responses request");
    let deployment = state.deployment(&request.model)?;
    if request.stream {
        Ok(streaming_response(state, deployment, request))
    } else {
        let response_id = new_response_id();
        let completion = state
            .backend
            .complete(&deployment, &request)
            .await
            .map_err(ApiError::upstream)?;
        let items = output_items(&completion, &response_id);
        Ok(Json(response_document(
            &request.model,
            &response_id,
            &completion,
            &items,
        ))
        .into_response())
    }
}

fn streaming_response(
    state: AppState,
    deployment: Deployment,
    request: ResponsesRequest,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(16);
    tokio::spawn(async move {
        let response_id = new_response_id();
        if tx
            .send(Ok(sse_event(created_event(&request.model, &response_id))))
            .await
            .is_err()
        {
            return;
        }
        match state.backend.complete(&deployment, &request).await {
            Ok(completion) => {
                for value in stream_events(&request.model, &response_id, &completion)
                    .into_iter()
                    .skip(1)
                {
                    if tx.send(Ok(sse_event(value))).await.is_err() {
                        break;
                    }
                }
            }
            Err(error) => {
                let detail = format!("{error:#}");
                tracing::error!(model = %request.model, error = %detail, "provider call failed");
                let value = failed_event(&response_id, &detail);
                let _ = tx.send(Ok(sse_event(value))).await;
            }
        }
    });
    Sse::new(ReceiverStream::new(rx))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(10))
                .text("keep-alive"),
        )
        .into_response()
}

fn sse_event(value: Value) -> Event {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
        .to_string();
    Event::default()
        .event(kind)
        .json_data(value)
        .expect("serde_json::Value is always serializable")
}

fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = state.inbound_api_key.as_deref() else {
        return Ok(());
    };
    let supplied = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if supplied == Some(expected) {
        Ok(())
    } else {
        Err(ApiError::unauthorized("missing or invalid bearer token"))
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request_error",
            message: message.into(),
        }
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "authentication_error",
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "model_not_found",
            message: message.into(),
        }
    }

    fn upstream(error: anyhow::Error) -> Self {
        let message = format!("{:#}", error.context("provider call failed"));
        Self {
            status: StatusCode::BAD_GATEWAY,
            code: "upstream_error",
            message,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "type": self.code,
                    "code": self.code,
                    "message": self.message
                }
            })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use litellm_core::router::LiteLLMParams;
    use serde_json::json;
    use tower::ServiceExt;

    use super::*;
    use crate::config::{CodexConfig, ServerConfig};

    #[test]
    fn upstream_errors_preserve_the_complete_cause_chain() {
        let error = anyhow::anyhow!("request timed out after 600 seconds")
            .context("OpenAI-compatible chat completions request failed");

        let response = ApiError::upstream(error);

        assert!(response.message.contains("provider call failed"));
        assert!(
            response
                .message
                .contains("OpenAI-compatible chat completions request failed")
        );
        assert!(
            response
                .message
                .contains("request timed out after 600 seconds")
        );
    }

    async fn mock_server(response: Value, route: &'static str) -> SocketAddr {
        async fn respond(State(response): State<Value>, Json(_body): Json<Value>) -> Json<Value> {
            Json(response)
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock listener");
        let address = listener.local_addr().expect("mock address");
        let router = axum::Router::new()
            .route(route, post(respond))
            .with_state(response);
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("mock server");
        });
        address
    }

    async fn recording_mock_server(
        response: Value,
        route: &'static str,
    ) -> (SocketAddr, tokio::sync::mpsc::Receiver<Value>) {
        async fn respond(
            State((response, sender)): State<(Value, tokio::sync::mpsc::Sender<Value>)>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            sender.send(body).await.expect("request receiver");
            Json(response)
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock listener");
        let address = listener.local_addr().expect("mock address");
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let router = axum::Router::new()
            .route(route, post(respond))
            .with_state((response, sender));
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("mock server");
        });
        (address, receiver)
    }

    fn test_config(model: &str, provider_model: &str, api_base: String) -> Config {
        Config {
            server: ServerConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                api_key_env: None,
                request_timeout_seconds: 10,
                default_max_output_tokens: 1024,
            },
            model_list: vec![Deployment {
                model_name: model.to_string(),
                litellm_params: LiteLLMParams {
                    model: provider_model.to_string(),
                    api_key: Some("test-key".to_string()),
                    api_base: Some(api_base),
                },
            }],
            chunking: crate::config::ChunkingConfig::default(),
            codex: CodexConfig {
                binary: "codex".to_string(),
                model: Some(model.to_string()),
                reasoning_effort: None,
            },
        }
    }

    async fn response_json(response: Response) -> Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("response body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("JSON response")
    }

    #[tokio::test]
    async fn generic_chat_backend_emits_responses_sse() {
        let upstream = mock_server(
            json!({
                "model": "local-model",
                "choices": [{"message": {"role": "assistant", "content": "hello"}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2}
            }),
            "/v1/chat/completions",
        )
        .await;
        let config = test_config(
            "local",
            "openai/local-model",
            format!("http://{upstream}/v1"),
        );
        let service = app(AppState::from_config(&config).unwrap());
        let response = service
            .oneshot(
                Request::post("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model": "local", "input": "hi", "stream": true}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("event: response.created"));
        assert!(body.contains("event: response.output_item.done"));
        assert!(body.contains("event: response.completed"));
        assert!(body.contains("hello"));
    }

    #[tokio::test]
    async fn ollama_ornith_request_traverses_the_responses_proxy() {
        let (upstream, mut requests) = recording_mock_server(
            json!({
                "model": "ornith-1.5:9b",
                "message": {"role": "assistant", "content": "ornith ok"},
                "prompt_eval_count": 3,
                "eval_count": 2
            }),
            "/api/chat",
        )
        .await;
        let config = test_config(
            "ornith-1.5:9b",
            "ollama/ornith-1.5:9b",
            format!("http://{upstream}/v1"),
        );
        let response = app(AppState::from_config(&config).unwrap())
            .oneshot(
                Request::post("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model": "ornith-1.5:9b", "input": "ping"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let upstream_request = requests.recv().await.expect("upstream request");
        assert_eq!(upstream_request["model"], "ornith-1.5:9b");
        assert_eq!(upstream_request["options"]["num_ctx"], 32_768);
        assert_eq!(
            response_json(response).await["output"][0]["content"][0]["text"],
            "ornith ok"
        );
    }

    #[tokio::test]
    async fn semantic_prompt_gateway_removes_codex_tools() {
        let (upstream, mut requests) = recording_mock_server(
            json!({
                "model": "ornith-1.5:9b",
                "message": {"role": "assistant", "content": "{}"},
                "prompt_eval_count": 3,
                "eval_count": 2
            }),
            "/api/chat",
        )
        .await;
        let config = test_config(
            "ornith-1.5:9b",
            "ollama/ornith-1.5:9b",
            format!("http://{upstream}/v1"),
        );
        let mut config = config;
        config.server.default_max_output_tokens = 4096;
        let state = AppState::from_config(&config)
            .unwrap()
            .for_semantic_prompts();
        let response = app(state)
            .oneshot(
                Request::post("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "ornith-1.5:9b",
                            "input": "analyze",
                            "tools": [{
                                "type": "function",
                                "name": "shell",
                                "parameters": {"type": "object"}
                            }],
                            "tool_choice": "auto",
                            "parallel_tool_calls": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let upstream_request = requests.recv().await.expect("upstream request");
        assert!(upstream_request.get("tools").is_none());
        assert!(upstream_request.get("tool_choice").is_none());
        assert!(upstream_request.get("parallel_tool_calls").is_none());
        assert_eq!(upstream_request["options"]["num_predict"], 2048);
        assert_eq!(upstream_request["options"]["num_ctx"], 32_768);
    }

    #[tokio::test]
    async fn native_litellm_anthropic_backend_preserves_tool_calls() {
        let upstream = mock_server(
            json!({
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "model": "claude-test",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "shell",
                    "input": {"cmd": "pwd"}
                }],
                "stop_reason": "tool_use",
                "stop_sequence": null,
                "usage": {"input_tokens": 4, "output_tokens": 3}
            }),
            "/v1/messages",
        )
        .await;
        let config = test_config(
            "claude",
            "anthropic/claude-test",
            format!("http://{upstream}"),
        );
        let service = app(AppState::from_config(&config).unwrap());
        let response = service
            .oneshot(
                Request::post("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "claude",
                            "input": "where am I?",
                            "tools": [{
                                "type": "function",
                                "name": "shell",
                                "parameters": {"type": "object"}
                            }]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["output"][0]["type"], "function_call");
        assert_eq!(body["output"][0]["call_id"], "toolu_1");
        assert_eq!(body["output"][0]["name"], "shell");
        assert_eq!(body["usage"]["total_tokens"], 7);
    }

    #[tokio::test]
    async fn unknown_model_returns_openai_shaped_error() {
        let config = test_config("known", "openai/known", "http://127.0.0.1:1/v1".to_string());
        let response = app(AppState::from_config(&config).unwrap())
            .oneshot(
                Request::post("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model": "missing", "input": "hi"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], "model_not_found");
    }
}
