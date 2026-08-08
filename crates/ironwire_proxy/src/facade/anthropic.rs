//! The Anthropic façade — what `ANTHROPIC_BASE_URL` points at.
//!
//! Mounted at `/anthropic`; Claude Code appends `/v1/...` itself.
//! `docs/PROTOCOL.md` §1 lists what has to exist here and why —
//! `count_tokens` in particular is not optional, because Claude Code drives its
//! context budget and compaction trigger off it.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ironwire_core::peek::RequestPeek;
use ironwire_core::policy::ConversationKey;
use ironwire_core::protocol::Protocol;
use ironwire_upstream::headers::{forward_request_header, forward_response_header};

use crate::facade::error::FacadeError;
use crate::pipeline::{self, dialect_for};
use crate::state::AppState;

const PROTOCOL: Protocol = Protocol::AnthropicMessages;

/// Routes for the Anthropic façade.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/models", get(models))
}

async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, FacadeError> {
    forward(state, headers, body, "/v1/messages").await
}

/// Claude Code calls this before every turn to decide whether to compact.
/// It goes through the same routing so the count comes from the model that
/// will actually serve the turn.
async fn count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, FacadeError> {
    forward(state, headers, body, "/v1/messages/count_tokens").await
}

/// Synthesized from the backends that are actually eligible, so a picker never
/// offers a model IronWire cannot currently serve.
async fn models(State(state): State<AppState>) -> Response {
    let statuses = state.backends.statuses().await;
    let consent = state.consent_snapshot();
    let mut data = Vec::new();
    for status in &statuses {
        if !status.authenticated {
            continue;
        }
        if status.kind.requires_consent() && !consent.is_granted(status.id.as_str()) {
            continue;
        }
        for (model, _) in &status.models {
            if data.iter().any(|m: &serde_json::Value| m["id"] == *model) {
                continue;
            }
            data.push(serde_json::json!({
                "type": "model",
                "id": model,
                "display_name": model,
            }));
        }
    }
    axum::Json(serde_json::json!({ "data": data, "has_more": false })).into_response()
}

async fn forward(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    path: &str,
) -> Result<Response, FacadeError> {
    // Parse once, for the peek only. The bytes we forward are the bytes we
    // received unless policy changes the model (`docs/PROTOCOL.md` §2).
    let parsed: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| FacadeError::invalid_request(format!("body is not valid JSON: {e}")))?;
    let peek = RequestPeek::inspect(PROTOCOL, &parsed, body.len());
    let key = conversation_key(&parsed);

    let forwarded: Vec<(String, String)> = headers
        .iter()
        .filter(|(name, _)| forward_request_header(name.as_str()))
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_ascii_lowercase(), v.to_string()))
        })
        .collect();

    let (response, routed) =
        pipeline::dispatch(&state, PROTOCOL, path, &peek, key, body, forwarded)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "no route for request");
                FacadeError::from_pipeline(&e)
            })?;

    tracing::info!(
        backend = %routed.decision.backend,
        model = routed.decision.model.as_deref().unwrap_or("<client's>"),
        rung = ?routed.decision.rung,
        attempts = routed.attempts,
        reason = %routed.decision.reason,
        "routed"
    );

    let backend = state
        .backends
        .get(&routed.decision.backend)
        .cloned()
        .expect("dispatch returned a registered backend");

    let observed = pipeline::observe_boxed(response.body, dialect_for(PROTOCOL), move |obs| {
        pipeline::record(&backend, &obs);
    });

    let mut builder = Response::builder()
        .status(StatusCode::from_u16(response.status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
    for (name, value) in &response.headers {
        if forward_response_header(name) {
            builder = builder.header(name, value);
        }
    }
    // Tell every intermediary — and any buffering layer of our own — to leave
    // an event stream alone. A buffered SSE stream is a hung coding agent.
    if peek.stream {
        builder = builder
            .header("cache-control", "no-cache")
            .header("x-accel-buffering", "no");
    }
    builder = builder.header("x-ironwire-backend", routed.decision.backend.to_string());

    builder
        .body(Body::from_stream(observed))
        .map_err(|e| FacadeError::invalid_request(format!("could not build response: {e}")))
}

/// Derive the conversation key from the parts that stay fixed as a
/// conversation grows (`docs/DESIGN.md` §3).
fn conversation_key(body: &serde_json::Value) -> ConversationKey {
    let system = match body.get("system") {
        Some(serde_json::Value::String(s)) => s.as_str(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .first()
            .and_then(|b| b.get("text"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
        _ => "",
    };
    let tools: Vec<&str> = body
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t.get("name").and_then(serde_json::Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    ConversationKey::derive(PROTOCOL, system, &tools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_key_is_stable_as_turns_accumulate() {
        let turn_1 = json!({
            "system": "You are Claude Code",
            "tools": [{"name": "Read"}, {"name": "Bash"}],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let turn_40 = json!({
            "system": "You are Claude Code",
            "tools": [{"name": "Read"}, {"name": "Bash"}],
            "messages": (0..80).map(|i| json!({"role": "user", "content": format!("m{i}")})).collect::<Vec<_>>(),
        });
        assert_eq!(conversation_key(&turn_1), conversation_key(&turn_40));
    }

    #[test]
    fn a_different_session_gets_a_different_key() {
        let a = json!({"system": "You are Claude Code", "tools": [{"name": "Read"}]});
        let b = json!({"system": "You are Aider", "tools": [{"name": "Read"}]});
        assert_ne!(conversation_key(&a), conversation_key(&b));
    }

    #[test]
    fn a_body_with_no_system_or_tools_still_keys() {
        let _ = conversation_key(&json!({"messages": []}));
    }
}
