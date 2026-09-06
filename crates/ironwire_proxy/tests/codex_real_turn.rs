//! A real Codex turn, and what IronWire says when the provider refuses it.
//!
//! Codex 0.153 has dropped `wire_api = "chat"`, so the Responses API is the
//! only wire it speaks and this is the whole of IronWire's Codex support.
//! Pointed at NEAR AI it failed with `response.failed` and "the upstream
//! closed without producing a response, and no other capacity could serve
//! it" — while the ledger recorded status 400 and an empty `error` column.
//!
//! Neither sentence was true, and between them they cost a live bisect. The
//! provider *had* produced a response: a 400 with a JSON body naming what it
//! objected to. IronWire treated every status below 500 as a success, handed
//! the JSON error body to the SSE resilience guard, which found no frames in
//! it and reported the stream as having closed empty. The provider's own
//! sentence was read by nobody and written nowhere.
//!
//! The fixture is a real captured Codex request, trimmed: identifiers are
//! zeroed and every prompt string — `instructions`, the `input` messages, and
//! every tool `description` — is replaced by filler of the same length. What
//! is kept is the shape that matters, because the shape is what was hard to
//! reproduce: 297 KB on the wire, 23 tools including `namespace` groups and a
//! built-in `web_search`, `include: ["reasoning.encrypted_content"]`, and the
//! Codex-proprietary `client_metadata` object.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ironwire_core::config::Config;
use ironwire_core::protocol::ModelTier;
use ironwire_creds::ConsentLedger;
use ironwire_ledger::Ledger;
use ironwire_proxy::server::app;
use ironwire_proxy::state::{AppState, BackendRegistry};
use ironwire_upstream::openai_chat::ChatCompletionsBackend;
use secrecy::SecretString;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

const FIXTURE: &str = include_str!("fixtures/codex_responses_request.json");

/// A provider refusal in the shape an OpenAI-compatible server sends one.
const REFUSAL: &str = r#"{"error":{"message":"tools[22]: 'name' is a required property","type":"invalid_request_error","param":"tools"}}"#;

#[derive(Debug, Default, Clone)]
struct Received {
    head: String,
    body: String,
}

/// A NEAR AI stand-in that refuses whatever it is sent, with a reason.
async fn spawn_refusing_nearai() -> (String, Arc<Mutex<Option<Received>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let received = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&received);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let sink = Arc::clone(&sink);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
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
                            *sink.lock().expect("lock") = Some(Received {
                                head,
                                body: String::from_utf8_lossy(&buf[split..split + length])
                                    .to_string(),
                            });
                            break;
                        }
                    }
                }
                let head = format!(
                    "HTTP/1.1 400 Bad Request\r\n\
                     content-type: application/json\r\n\
                     content-length: {}\r\n\r\n",
                    REFUSAL.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(REFUSAL.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });
    (format!("http://{addr}/v1"), received)
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

fn state_for(base_url: &str, ledger: Ledger) -> AppState {
    let mut registry = BackendRegistry::new();
    registry.push(Arc::new(
        ChatCompletionsBackend::nearai(
            Some(SecretString::from("near-key")),
            Some(base_url.to_string()),
            vec![("Qwen/Qwen3.6-35B-A3B-FP8".to_string(), ModelTier::Frontier)],
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
    .with_ledger(Some(ledger))
}

fn codex_request() -> Request<Body> {
    // Re-serialised compactly, which is how Codex sends it; the fixture is
    // stored indented only so it can be read in review.
    let body: serde_json::Value = serde_json::from_str(FIXTURE).expect("fixture is valid JSON");
    Request::builder()
        .method("POST")
        .uri("/openai/v1/responses")
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-the-clients-own-key")
        .header("originator", "codex_cli_rs")
        .body(Body::from(serde_json::to_vec(&body).expect("serialises")))
        .expect("request builds")
}

/// The fixture is the request that failed, not a reduction of it.
#[test]
fn the_fixture_still_has_the_shape_that_broke() {
    let body: serde_json::Value = serde_json::from_str(FIXTURE).expect("valid JSON");
    let compact = serde_json::to_vec(&body).expect("serialises");
    assert!(
        compact.len() > 250_000,
        "the size class is part of the reproduction, got {} bytes",
        compact.len()
    );
    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 23);
    assert!(
        tools.iter().any(|t| t["type"] == "namespace"),
        "the nested MCP tool groups are part of the shape"
    );
    assert!(
        tools.iter().any(|t| t["type"] == "web_search"),
        "the built-in tool with no `name` is part of the shape"
    );
    assert!(
        body.get("client_metadata").is_some(),
        "the Codex-proprietary field is part of the shape"
    );
    // Trimmed on purpose: no real identifiers, no real prompt wording.
    assert_eq!(
        body["client_metadata"]["session_id"],
        serde_json::Value::String("00000000-0000-0000-0000-000000000000".to_string())
    );
}

#[tokio::test]
async fn a_refused_turn_tells_the_client_and_the_ledger_why() {
    let ledger = Ledger::in_memory().expect("ledger opens");
    let (base, received) = spawn_refusing_nearai().await;

    let response = app(state_for(&base, ledger.clone()))
        .oneshot(codex_request())
        .await
        .expect("the proxy answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body reads");
    let text = String::from_utf8_lossy(&body).to_string();

    // The request itself went out untouched, on its own wire. That part was
    // never the bug, and a change that "fixed" this by reshaping the body
    // would break the native lane instead.
    let seen = received
        .lock()
        .expect("lock")
        .clone()
        .expect("upstream saw a request");
    assert!(
        seen.head.starts_with("POST /v1/responses HTTP/1.1"),
        "{}",
        seen.head
    );
    assert!(seen.body.contains("client_metadata"), "body was reshaped");

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        text.contains("'name' is a required property"),
        "the provider's reason never reached the client: {text}"
    );
    assert!(
        !text.contains("closed without producing a response"),
        "a refusal was still being reported as an empty stream: {text}"
    );

    let rows = ledger.recent(10).expect("ledger reads");
    let row = rows.first().expect("the refusal was recorded");
    assert_eq!(row.status, 400);
    let recorded = row.error.as_deref().unwrap_or_default();
    assert!(
        recorded.contains("'name' is a required property"),
        "the ledger's error column is still empty of the reason: {recorded:?}"
    );
}
