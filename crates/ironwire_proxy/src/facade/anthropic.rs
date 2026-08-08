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
use crate::resilience;
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
    let started_at = chrono::Utc::now();
    // Parse once, for the peek only. The bytes we forward are the bytes we
    // received unless policy changes the model (`docs/PROTOCOL.md` §2).
    let parsed: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| FacadeError::invalid_request(format!("body is not valid JSON: {e}")))?;
    let peek = RequestPeek::inspect_with(PROTOCOL, &parsed, body.len(), &state.identity_markers());
    let key = conversation_key(&parsed);
    let conversation = key.0;

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

    let (response, routed) = pipeline::dispatch(
        &state,
        PROTOCOL,
        path,
        &peek,
        key.clone(),
        body.clone(),
        forwarded.clone(),
    )
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

    // The observation closure runs when the stream ends *or is dropped*, so
    // both quota accounting and the ledger entry survive a cancelled request.
    let ledger = state.ledger.clone();
    let entry = pipeline::LedgerContext {
        started_at,
        started: std::time::Instant::now(),
        facade: "anthropic",
        path: path.to_string(),
        conversation: conversation.to_string(),
        backend: routed.decision.backend.to_string(),
        requested_model: peek.requested_model.clone(),
        rung: format!("{:?}", routed.decision.rung).to_lowercase(),
        attempts: routed.attempts,
        status: response.status.as_u16(),
    };
    let observed = pipeline::observe_boxed(response.body, dialect_for(PROTOCOL), move |obs| {
        pipeline::record(&backend, &obs);
        if let Some(ledger) = ledger.as_ref() {
            entry.write(ledger, &obs);
        }
    });

    // Keep a quiet stream alive, end a dead one honestly, and restart one that
    // failed before producing content — the three shapes of "Response stalled
    // mid-stream" (`docs/PROTOCOL.md` §5, `resilience`).
    let body: Body = if peek.stream {
        let reconnect_state = state.clone();
        let reconnect_path = path.to_string();
        let reconnect_peek = peek.clone();
        let resilience = resilience::ResilienceConfig::from(&state.config.resilience);
        let reconnect: resilience::Reconnect = Box::new(move || {
            let state = reconnect_state.clone();
            let path = reconnect_path.clone();
            let peek = reconnect_peek.clone();
            let key = key.clone();
            let body = body.clone();
            let headers = forwarded.clone();
            Box::pin(async move {
                match pipeline::dispatch(&state, PROTOCOL, &path, &peek, key, body, headers).await {
                    Ok((response, routed)) => {
                        tracing::info!(backend = %routed.decision.backend, "stream restarted");
                        Some(pipeline::observe_boxed(
                            response.body,
                            dialect_for(PROTOCOL),
                            |_| {},
                        ))
                    }
                    Err(error) => {
                        tracing::warn!(%error, "no capacity left to restart the stream");
                        None
                    }
                }
            })
        });
        Body::from_stream(resilience::guard(observed, reconnect, resilience))
    } else {
        Body::from_stream(observed)
    };

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
        .body(body)
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
