use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bibiiwiki::config::Config;
use bibiiwiki::gateway::{AppState, app};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

#[tokio::test]
#[ignore = "requires Ollama with ornith-1.5:9b installed and running"]
async fn default_ornith_model_answers_through_the_rust_responses_proxy() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config = Config::load(&root.join("bibiiwiki.yaml")).expect("default configuration");
    let response = app(AppState::from_config(&config).expect("gateway state"))
        .oneshot(
            Request::post("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "ornith-1.5:9b",
                        "input": "Reply with exactly ORNITH_PROXY_OK and nothing else.",
                        "max_output_tokens": 256,
                        "stream": false
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("gateway response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("Responses JSON");
    assert_eq!(body["model"], "ornith-1.5:9b");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["output"][0]["content"][0]["text"], "ORNITH_PROXY_OK");
}
