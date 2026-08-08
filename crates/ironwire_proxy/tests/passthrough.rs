//! Conformance: the native lane forwards bytes, not meanings.
//!
//! `docs/PROTOCOL.md` §7.2 — replay a request through the proxy against a
//! recording mock and assert the bytes the upstream received differ from the
//! original *only* in the mutations §2 enumerates, and that the bytes the
//! client received are byte-identical to what the upstream sent.
//!
//! This is the test that makes the fidelity claim real. Everything else in the
//! design rests on it.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ironwire_core::config::Config;
use ironwire_creds::ConsentLedger;
use ironwire_proxy::server::app;
use ironwire_proxy::state::{AppState, BackendRegistry};
use ironwire_upstream::anthropic::AnthropicBackend;
use secrecy::SecretString;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

/// What the mock upstream saw.
#[derive(Debug, Default, Clone)]
struct Received {
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// The exact SSE bytes the mock sends back, including framing.
const UPSTREAM_SSE: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-opus-4-6","usage":{"input_tokens":11,"cache_read_input_tokens":98000,"output_tokens":1}}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":137}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

/// Minimal HTTP/1.1 upstream that records one request and replies with SSE.
///
/// Hand-rolled rather than mounted on a framework so the response bytes are
/// exactly what this test wrote — a framework could re-frame them, which would
/// make a byte-identity assertion meaningless.
async fn spawn_mock() -> (String, Arc<Mutex<Option<Received>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let received = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&received);

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        // Read until the body is complete, using content-length from the head.
        loop {
            let Ok(n) = socket.read(&mut chunk).await else {
                return;
            };
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(split) = find_head_end(&buf) {
                let head = String::from_utf8_lossy(&buf[..split]).to_string();
                let length = content_length(&head).unwrap_or(0);
                if buf.len() - split >= length {
                    let mut lines = head.lines();
                    let request_line = lines.next().unwrap_or_default().to_string();
                    let headers = lines
                        .filter_map(|line| line.split_once(": "))
                        .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
                        .collect();
                    *sink.lock().expect("lock") = Some(Received {
                        request_line,
                        headers,
                        body: String::from_utf8_lossy(&buf[split..split + length]).to_string(),
                    });
                    break;
                }
            }
        }

        let head = format!(
            "HTTP/1.1 200 OK\r\n\
             content-type: text/event-stream\r\n\
             anthropic-ratelimit-unified-limit: 1000\r\n\
             anthropic-ratelimit-unified-remaining: 180\r\n\
             content-length: {}\r\n\r\n",
            UPSTREAM_SSE.len()
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(UPSTREAM_SSE.as_bytes()).await;
        let _ = socket.flush().await;
    });

    (format!("http://{addr}"), received)
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn content_length(head: &str) -> Option<usize> {
    head.lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
}

/// State with a single API-key Anthropic backend pointed at the mock.
///
/// The API-key backend rather than the subscription one, so this test does not
/// depend on a Claude Code login existing on the machine running it.
fn state_for(base_url: &str) -> AppState {
    let mut registry = BackendRegistry::new();
    registry.push(Arc::new(
        AnthropicBackend::api_key(
            SecretString::from("sk-ant-test-key"),
            Some(base_url.to_string()),
            30,
        )
        .expect("client builds"),
    ));
    AppState::new(
        registry,
        Config::default(),
        ConsentLedger::default(),
        "test-token".to_string(),
    )
}

/// A request body carrying every shape the native lane must not disturb:
/// cache breakpoints, a signed thinking block, tools, and a field we have
/// never heard of.
const CLIENT_BODY: &str = concat!(
    r#"{"model":"claude-opus-4-6","stream":true,"#,
    r#""system":[{"type":"text","text":"You are Claude Code","cache_control":{"type":"ephemeral"}}],"#,
    r#""tools":[{"name":"Read","input_schema":{"type":"object"}}],"#,
    r#""messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"...","signature":"sig-abc"}]},"#,
    r#"{"role":"user","content":"hi"}],"#,
    r#""a_field_from_the_future":{"nested":[1,2,3]},"#,
    r#""thinking":{"type":"enabled","budget_tokens":1024}}"#,
);

#[tokio::test]
async fn the_request_body_reaches_the_provider_byte_identical() {
    let (base_url, received) = spawn_mock().await;
    let response = app(state_for(&base_url))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/anthropic/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .body(Body::from(CLIENT_BODY))
                .expect("request builds"),
        )
        .await
        .expect("served");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(response.into_body(), 1 << 20).await;

    let got = received.lock().expect("lock").clone().expect("mock saw it");
    assert_eq!(
        got.body, CLIENT_BODY,
        "the native lane must not re-encode the body"
    );
    assert!(got.request_line.starts_with("POST /v1/messages "));
}

#[tokio::test]
async fn only_the_enumerated_headers_are_mutated() {
    let (base_url, received) = spawn_mock().await;
    let response = app(state_for(&base_url))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/anthropic/v1/messages")
                .header("content-type", "application/json")
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", "interleaved-thinking-2025-05-14")
                .header("x-stainless-lang", "js")
                .header("x-api-key", "CLIENT-KEY-MUST-NOT-LEAK")
                .header("authorization", "Bearer CLIENT-TOKEN-MUST-NOT-LEAK")
                .body(Body::from(CLIENT_BODY))
                .expect("request builds"),
        )
        .await
        .expect("served");
    let _ = axum::body::to_bytes(response.into_body(), 1 << 20).await;

    let got = received.lock().expect("lock").clone().expect("mock saw it");
    let header = |name: &str| {
        got.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };

    // Preserved: provider headers, including ones we do not model.
    assert_eq!(header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(header("x-stainless-lang"), Some("js"));
    assert_eq!(
        header("anthropic-beta"),
        Some("interleaved-thinking-2025-05-14"),
        "the client's own beta flags must survive"
    );
    assert_eq!(header("content-type"), Some("application/json"));

    // Replaced: the credential is ours, never the client's.
    assert_eq!(header("x-api-key"), Some("sk-ant-test-key"));
    assert!(
        !got.headers.iter().any(|(_, v)| v.contains("MUST-NOT-LEAK")),
        "a credential the client sent must never reach the provider"
    );
}

#[tokio::test]
async fn the_response_stream_reaches_the_client_byte_identical() {
    let (base_url, _received) = spawn_mock().await;
    let response = app(state_for(&base_url))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/anthropic/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(CLIENT_BODY))
                .expect("request builds"),
        )
        .await
        .expect("served");

    assert_eq!(response.status(), StatusCode::OK);
    // Rate-limit headers must survive: observation depends on them.
    assert_eq!(
        response
            .headers()
            .get("anthropic-ratelimit-unified-remaining")
            .and_then(|v| v.to_str().ok()),
        Some("180")
    );
    // Streaming responses must not be buffered anywhere in our path.
    assert_eq!(
        response
            .headers()
            .get("x-accel-buffering")
            .and_then(|v| v.to_str().ok()),
        Some("no")
    );

    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    assert_eq!(
        String::from_utf8_lossy(&body),
        UPSTREAM_SSE,
        "SSE must be forwarded frame-for-frame"
    );
}

#[tokio::test]
async fn usage_is_observed_from_the_stream_without_altering_it() {
    let (base_url, _received) = spawn_mock().await;
    let state = state_for(&base_url);
    let backend = state
        .backends
        .all()
        .first()
        .cloned()
        .expect("one backend registered");

    let response = app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/anthropic/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(CLIENT_BODY))
                .expect("request builds"),
        )
        .await
        .expect("served");
    let _ = axum::body::to_bytes(response.into_body(), 1 << 20).await;

    // The mock reported 180 of 1000 remaining, i.e. 82% used. We must report
    // exactly that, from the provider's own headers.
    match backend.quota().primary {
        ironwire_core::quota::Headroom::Observed { used_pct, .. } => {
            assert!(
                (used_pct - 82.0).abs() < 0.01,
                "expected 82% used, got {used_pct}"
            );
        }
        other => panic!("expected an observed headroom, got {other:?}"),
    }
}
