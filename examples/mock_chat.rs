use axum::Json;
use axum::routing::post;
use serde_json::{Value, json};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    let listener = TcpListener::bind("127.0.0.1:4010")
        .await
        .expect("failed to bind mock provider");
    eprintln!("mock chat-completions provider listening on 127.0.0.1:4010");
    let app = axum::Router::new().route("/v1/chat/completions", post(complete));
    axum::serve(listener, app)
        .await
        .expect("mock provider failed");
}

async fn complete(Json(request): Json<Value>) -> Json<Value> {
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("mock-coder");
    Json(json!({
        "id": "chatcmpl_bibiiwiki_smoke",
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "BIBIIWIKI_CODEX_SMOKE_OK"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 4,
            "total_tokens": 14
        }
    }))
}
