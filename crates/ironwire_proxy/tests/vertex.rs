//! Conformance for the Vertex envelope, before the existing Messages pipeline.
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode, Uri};
use ironwire_core::config::Config;
use ironwire_core::protocol::BackendId;
use ironwire_creds::ConsentLedger;
use ironwire_proxy::server::app;
use ironwire_proxy::state::{AppState, BackendRegistry};
use ironwire_upstream::anthropic::AnthropicBackend;
use secrecy::SecretString;
use tower::ServiceExt;

const PREFIX: &str =
    "/vertex/projects/private-project/locations/private-region/publishers/anthropic/models/";
const JSON: &str = r#"{"id":"msg_vertex","type":"message","role":"assistant","model":"served-model","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}}"#;
const SSE: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_vertex","type":"message","role":"assistant","model":"served-model","content":[],"usage":{"input_tokens":2,"output_tokens":0}}}"#,
    "\n\nevent: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
    "\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);
type Seen = Arc<Mutex<Vec<(Uri, HeaderMap, Bytes)>>>;

async fn setup() -> (AppState, Seen) {
    let seen: Seen = Arc::default();
    let received = seen.clone();
    let upstream =
        axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
            let received = received.clone();
            async move {
                let streaming =
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap()["stream"] == true;
                received.lock().unwrap().push((uri, headers, body));
                (
                    [(
                        "content-type",
                        if streaming {
                            "text/event-stream"
                        } else {
                            "application/json"
                        },
                    )],
                    if streaming { SSE } else { JSON },
                )
            }
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let mut registry = BackendRegistry::new();
    registry.push(Arc::new(
        AnthropicBackend::api_key(SecretString::from("host-key"), Some(base), 5).unwrap(),
    ));
    let state = AppState::new(
        registry,
        Config::default(),
        ConsentLedger::default(),
        "control-token".into(),
    )
    .with_ledger(Some(ironwire_ledger::Ledger::in_memory().unwrap()));
    state.policy.lock().unwrap().set_pin(
        Some(BackendId::from("anthropic-key")),
        Some("served-model".into()),
    );
    (state, seen)
}

fn request(suffix: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("{PREFIX}{suffix}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

#[tokio::test]
async fn vertex_uses_messages_routing_and_retains_requested_model_separately_from_pin() {
    for stream in [false, true] {
        let (state, seen) = setup().await;
        let action = if stream {
            "streamRawPredict"
        } else {
            "rawPredict"
        };
        let mut request = request(
            &format!("claude-client-alias@20261001:{action}"),
            r#"{"anthropic_version":"vertex-2023-10-16","max_tokens":16,"messages":[{"role":"user","content":"hello"}],"future":{"preserve":true}}"#,
        );
        request.headers_mut().insert(
            "authorization",
            "Bearer vertex-client-secret".parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("x-goog-api-key", "google-secret".parse().unwrap());
        request.headers_mut().insert(
            "x-goog-request-params",
            "projects=private-project&locations=private-region"
                .parse()
                .unwrap(),
        );
        request.headers_mut().insert(
            "x-claude-code-session-id",
            "desktop-session".parse().unwrap(),
        );
        let response = app(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(
            response.as_ref(),
            if stream {
                SSE.as_bytes()
            } else {
                JSON.as_bytes()
            }
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0.path(), "/v1/messages");
        let sent: serde_json::Value = serde_json::from_slice(&seen[0].2).unwrap();
        assert_eq!(sent["model"], "served-model");
        assert_eq!(sent["stream"], stream);
        assert_eq!(sent["future"], serde_json::json!({"preserve":true}));
        assert!(sent.get("anthropic_version").is_none());
        assert!(
            sent.get("system").is_none(),
            "a facade must never manufacture client identity"
        );
        let rendered = format!("{:?}", seen[0]);
        for secret in [
            "vertex-client-secret",
            "google-secret",
            "private-project",
            "private-region",
        ] {
            assert!(!rendered.contains(secret), "leaked {secret}");
        }
        assert_eq!(seen[0].1["x-api-key"], "host-key");
        let rows = state.ledger.as_ref().unwrap().recent(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "/v1/messages");
        assert_eq!(
            rows[0].requested_model.as_deref(),
            Some("claude-client-alias@20261001")
        );
        assert_eq!(rows[0].served_model.as_deref(), Some("served-model"));
        assert_eq!(
            rows[0].client_session_id.as_deref(),
            Some("desktop-session")
        );
    }
}

#[tokio::test]
async fn vertex_counting_is_terminal_without_an_upstream_request_or_fabricated_count() {
    let (state, seen) = setup().await;
    let response = app(state.clone())
        .oneshot(request(
            "count-tokens:rawPredict",
            r#"{"anthropic_version":"vertex-2023-10-16","messages":[]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response.headers().get("retry-after").is_none());
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body.get("input_tokens").is_none());
    assert!(seen.lock().unwrap().is_empty());
    let rows = state.ledger.as_ref().unwrap().recent(10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, "/v1/messages/count_tokens");
}

#[tokio::test]
async fn malformed_vertex_requests_are_refused_without_echoing_path_or_body_values() {
    for (suffix, body) in [
        ("claude:rawPredict", "not JSON private-project"),
        ("claude:rawPredict", "[]"),
        ("claude:rawPredict", "null"),
        ("claude:rawPredict", r#"{"model":"other"}"#),
        ("claude:rawPredict", r#"{"stream":true}"#),
        ("claude:streamRawPredict", r#"{"stream":false}"#),
        ("claude:streamRawPredict", r#"{"stream":"true"}"#),
        (
            "claude:rawPredict",
            r#"{"anthropic_version":"unsupported"}"#,
        ),
        ("claude:rawPredict", r#"{"model":"other","model":"claude"}"#),
        ("claude:rawPredict", r#"{"stream":true,"stream":false}"#),
        ("claude:unknownAction", "{}"),
        ("claude%2Fescape:rawPredict", "{}"),
        ("claude:rawPredict/extra", "{}"),
        ("count-tokens:streamRawPredict", "{}"),
    ] {
        let (state, seen) = setup().await;
        let response = app(state.clone())
            .oneshot(request(suffix, body))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{suffix} {body}"
        );
        let error = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let error: serde_json::Value = serde_json::from_slice(&error).unwrap();
        assert_eq!(error["error"]["type"], "invalid_request_error");
        assert!(!error.to_string().contains("private-project"));
        assert!(seen.lock().unwrap().is_empty());
        let rows = state.ledger.as_ref().unwrap().recent(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].path == "/v1/messages" || rows[0].path == "/v1/messages/count_tokens");
        assert!(rows[0].requested_model.is_none());
    }
}

#[tokio::test]
async fn a_vertex_route_override_cannot_escape_the_existing_pin() {
    let (state, seen) = setup().await;
    let mut request = request("claude:rawPredict", r#"{"messages":[]}"#);
    request
        .headers_mut()
        .insert("x-ironwire-route", "other:escape".parse().unwrap());
    let response = app(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(
        state.ledger.as_ref().unwrap().recent(1).unwrap()[0].path,
        "/v1/messages"
    );
}

#[tokio::test]
async fn oversized_vertex_uploads_keep_anthropic_errors_and_canonical_ledger_paths() {
    let (state, seen) = setup().await;
    let response = app(state.clone())
        .oneshot(request(
            "claude:rawPredict",
            &"x".repeat(ironwire_proxy::facade::MAX_REQUEST_BODY_BYTES + 1),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"]["type"],
        "invalid_request_error"
    );
    assert!(seen.lock().unwrap().is_empty());
    let rows = state.ledger.as_ref().unwrap().recent(1).unwrap();
    assert_eq!(rows[0].path, "/v1/messages");
    assert_eq!(rows[0].failure.as_deref(), Some("body_rejected"));
}
