//! The request pipeline: peek, decide, send, fail over, stream.
//!
//! Two rules govern this file and both come from `docs/PROTOCOL.md`:
//!
//! 1. **Mutate only what §2 enumerates.** URL, auth headers, hop-by-hop
//!    headers, and — only when policy chose a different model — the `model`
//!    key. Nothing else is touched, which is why provider features we have
//!    never heard of keep working.
//! 2. **Failover ends at the first byte.** Once a byte of the response has
//!    reached the client, replaying the request would duplicate content the
//!    client has already committed to its transcript. After that point an
//!    upstream failure is surfaced on the open stream, never retried (§5).

use std::sync::Arc;

use bytes::Bytes;
use chrono::Utc;
use futures_util::{Stream, StreamExt};
use ironwire_core::peek::RequestPeek;
use ironwire_core::policy::{ConversationKey, NoRoute, RouteDecision};
use ironwire_core::protocol::Protocol;
use ironwire_ledger::{Exchange, Ledger};
use ironwire_translate::ChatToAnthropicStream;
use ironwire_upstream::backend::{Backend, UpstreamError, UpstreamRequest, UpstreamResponse};
use ironwire_upstream::observe::Observation;
use ironwire_upstream::sse::{Dialect, SseObserver};

use crate::state::AppState;

/// The upshot of routing one request.
#[derive(Debug)]
pub struct Routed {
    /// What the router decided.
    pub decision: RouteDecision,
    /// How many backends were tried before this one succeeded.
    pub attempts: usize,
    /// Errors from backends tried and rejected, for the log and for the error
    /// we return if everything fails.
    pub rejected: Vec<(String, String)>,
}

/// Failure of the whole pipeline.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// The router found nowhere to send this.
    #[error("no route: {0:?}")]
    NoRoute(NoRoute),
    /// Every candidate backend failed.
    #[error("all backends failed")]
    AllFailed {
        /// The last error, which is the most informative one to surface.
        last: UpstreamError,
        /// Everything tried, for diagnostics.
        rejected: Vec<(String, String)>,
    },
    /// The upstream returned a non-retryable error we pass through.
    #[error(transparent)]
    Upstream(UpstreamError),
}

/// A per-request route override from the `X-IronWire-Route` header.
///
/// Distinct from `ironwire pin`, which is a daemon-wide mode a user turns on
/// and forgets. This is one request, named by the caller — the escape hatch for
/// a script that wants a specific backend without disturbing anyone else's
/// session (`docs/DESIGN.md` §3).
///
/// Like a pin, it overrides *preference* but never *eligibility*: a route that
/// would corrupt the request is still refused. Obeying a caller into producing
/// a broken response is not obedience, it is a bug.
pub const ROUTE_OVERRIDE_HEADER: &str = "x-ironwire-route";

/// Read and remove the route override from the forwarded header list.
///
/// Removed, not just read: it is IronWire's own header and forwarding it to a
/// provider would leak our routing vocabulary into someone else's API.
#[must_use]
pub fn take_route_override(
    headers: &mut Vec<(String, String)>,
) -> Option<(String, Option<String>)> {
    let index = headers
        .iter()
        .position(|(name, _)| name.eq_ignore_ascii_case(ROUTE_OVERRIDE_HEADER))?;
    let (_, value) = headers.remove(index);
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    // `backend` or `backend:model`.
    Some(match value.split_once(':') {
        Some((backend, model)) => (backend.trim().to_string(), Some(model.trim().to_string())),
        None => (value.to_string(), None),
    })
}

/// Route and dispatch one request, failing over while it is still safe to.
///
/// # Errors
///
/// [`PipelineError`] when no backend could serve the request.
pub async fn dispatch(
    state: &AppState,
    inbound: Protocol,
    path: &str,
    peek: &RequestPeek,
    key: ConversationKey,
    body: Bytes,
    headers: Vec<(String, String)>,
) -> Result<(UpstreamResponse, Routed), PipelineError> {
    let conversation = key.0.to_string();
    let result = dispatch_inner(state, inbound, path, peek, key, body, headers).await;
    // Every failure exit publishes, not just the one at the bottom: `dispatch`
    // returns early from inside the failover loop too, and a channel that
    // reports only some failures is worse than one that reports none — a user
    // would learn to read silence as success.
    if let Err(error) = &result {
        state.events.publish(crate::events::Event::Failed {
            at: Utc::now(),
            conversation,
            detail: error.to_string(),
        });
    }
    result
}

async fn dispatch_inner(
    state: &AppState,
    inbound: Protocol,
    path: &str,
    peek: &RequestPeek,
    key: ConversationKey,
    body: Bytes,
    headers: Vec<(String, String)>,
) -> Result<(UpstreamResponse, Routed), PipelineError> {
    let mut headers = headers;
    let route_override = take_route_override(&mut headers).map(|(backend, model)| {
        (
            ironwire_core::protocol::BackendId::from(backend.as_str()),
            model,
        )
    });

    let statuses = state.backends.statuses().await;
    let consent = state.consent_snapshot();
    let candidates = state.backends.candidates(&statuses, &consent);

    // Skip backends whose circuit is open, so a known-dead one is not
    // rediscovered — at the cost of a round trip — on every single turn.
    //
    // Unless that would leave nothing. A breaker exists to spend less time on a
    // backend that is failing, not to turn a degraded proxy into a dead one:
    // when every circuit is open, the honest move is to try anyway and report
    // the real upstream error rather than a 503 of our own invention.
    let now = Utc::now();
    let healthy: Vec<_> = candidates
        .iter()
        .filter(|c| state.breakers.allows(&c.id, now))
        .cloned()
        .collect();
    let candidates = if healthy.is_empty() {
        if !candidates.is_empty() {
            tracing::warn!("every backend's circuit is open; trying anyway");
        }
        candidates
    } else {
        healthy
    };

    // Where this conversation was before we decided anything, so a genuine
    // route change can be told apart from a request that stayed put.
    let previous = match state.policy.lock() {
        Ok(policy) => policy.current_backend(&key),
        Err(poisoned) => poisoned.into_inner().current_backend(&key),
    };

    let mut rejected: Vec<(String, String)> = Vec::new();
    let mut attempts = 0usize;
    let mut last_error: Option<UpstreamError> = None;

    // Bounded: each iteration marks the failed backend unavailable, so the
    // candidate set strictly shrinks. The cap is belt and braces against a
    // backend that reports itself healthy after failing.
    let max_attempts = candidates.len().max(1) + MAX_SAME_BACKEND_RETRIES;
    let mut excluded: Vec<ironwire_core::protocol::BackendId> = Vec::new();
    let mut same_backend_attempts = 0usize;

    while attempts < max_attempts {
        let available: Vec<_> = candidates
            .iter()
            .filter(|c| !excluded.contains(&c.id))
            .cloned()
            .collect();

        let decision = {
            let mut policy = match state.policy.lock() {
                Ok(p) => p,
                Err(poisoned) => poisoned.into_inner(),
            };
            match policy.decide_with_override(
                key.clone(),
                inbound,
                peek,
                &available,
                Utc::now(),
                route_override.clone(),
            ) {
                Ok(decision) => decision,
                Err(no_route) => {
                    return Err(last_error.map_or(PipelineError::NoRoute(no_route), |last| {
                        PipelineError::AllFailed { last, rejected }
                    }));
                }
            }
        };

        let Some(backend) = state.backends.get(&decision.backend) else {
            excluded.push(decision.backend.clone());
            continue;
        };

        attempts += 1;
        let request = if decision.translated {
            match translate_request(&body, path, &decision, peek) {
                Ok(request) => request,
                Err(reason) => {
                    // Refusing beats sending a body the target cannot parse.
                    rejected.push((decision.backend.to_string(), reason.clone()));
                    excluded.push(decision.backend.clone());
                    if let Ok(mut policy) = state.policy.lock() {
                        policy.forget(&key);
                    }
                    continue;
                }
            }
        } else {
            UpstreamRequest {
                path: path.to_string(),
                body: apply_model(&body, decision.model.as_deref()),
                headers: headers.clone(),
                stream: peek.stream,
            }
        };

        match backend.send(request).await {
            Ok(response) => {
                let response = if decision.translated {
                    translate_response(
                        response,
                        peek.requested_model.as_deref().unwrap_or("unknown"),
                        peek.stream,
                    )
                } else {
                    response
                };
                // Headers are in hand, so the backend answered. Whether the
                // *stream* then stalls is a different failure, handled on the
                // open stream by `resilience` — a breaker cannot help there,
                // because by then the client is already committed.
                state.breakers.record_success(&decision.backend);
                // Announce only a real change. A sticky conversation producing
                // one "routed" line per turn would bury the one line that
                // means something (`crate::events`).
                if previous.as_ref() != Some(&decision.backend) {
                    state.events.publish(crate::events::Event::Routed {
                        at: Utc::now(),
                        conversation: key.0.to_string(),
                        from: previous.as_ref().map(ToString::to_string),
                        to: decision.backend.to_string(),
                        rung: decision.rung,
                        translated: decision.translated,
                        reason: decision.reason.clone(),
                    });
                }
                return Ok((
                    response,
                    Routed {
                        decision,
                        attempts,
                        rejected,
                    },
                ));
            }
            Err(error) => {
                tracing::warn!(
                    backend = %decision.backend,
                    error = %error,
                    retryable = error.is_retryable(),
                    "backend failed before first byte"
                );
                rejected.push((decision.backend.to_string(), error.to_string()));
                state
                    .breakers
                    .record_failure(&decision.backend, &error, Utc::now());

                // A rate limit teaches us something; fold it in so the next
                // decision does not pick the same backend again.
                if let Some(secs) = error.retry_after_secs() {
                    backend.record(&Observation {
                        retry_after_secs: Some(secs),
                        ..Observation::default()
                    });
                }

                if !error.is_retryable() {
                    return Err(PipelineError::Upstream(error));
                }

                // A 529/overload or a dropped connection is usually momentary.
                // Descending the ladder on one would move a warm conversation
                // off its subscription — and onto a metered key the user pays
                // for — over a blip. Try the same backend again first.
                if should_retry_same_backend(&error)
                    && same_backend_attempts < MAX_SAME_BACKEND_RETRIES
                {
                    same_backend_attempts += 1;
                    let backoff = backoff_for(same_backend_attempts);
                    tracing::info!(
                        backend = %decision.backend,
                        attempt = same_backend_attempts,
                        ?backoff,
                        "transient upstream failure; retrying the same backend before descending"
                    );
                    tokio::time::sleep(backoff).await;
                    last_error = Some(error);
                    continue;
                }

                same_backend_attempts = 0;
                // Drop this conversation's affinity: it is pinned to a backend
                // that just failed, and keeping it would re-select it.
                if let Ok(mut policy) = state.policy.lock() {
                    policy.forget(&key);
                }
                excluded.push(decision.backend.clone());
                last_error = Some(error);
            }
        }
    }

    Err(match last_error {
        Some(last) => PipelineError::AllFailed { last, rejected },
        None => PipelineError::NoRoute(NoRoute::AllExhausted),
    })
}

/// How many times to retry the *same* backend on a transient failure before
/// descending the fidelity ladder. Small on purpose: the point is to ride out a
/// blip, not to wait out a real outage.
pub const MAX_SAME_BACKEND_RETRIES: usize = 2;

/// Whether this failure is worth another go at the same backend.
///
/// A 429 the provider attached a real wait to is *not* — it told us how long,
/// and sleeping through it would stall the agent when other capacity exists.
/// Everything else transient is: an overload or a reset usually clears.
#[must_use]
pub fn should_retry_same_backend(error: &UpstreamError) -> bool {
    match error {
        UpstreamError::RateLimited {
            retry_after_secs, ..
        } => retry_after_secs.is_none_or(|secs| secs <= 2),
        UpstreamError::Transport { .. } => true,
        UpstreamError::Upstream { status, .. } => status.is_server_error(),
        // A missing credential will not fix itself, and a host mismatch is our
        // own bug — retrying either just delays the real answer.
        UpstreamError::NeedsAuth { .. } | UpstreamError::CredentialHostMismatch { .. } => false,
    }
}

/// Exponential backoff for same-backend retries.
#[must_use]
pub fn backoff_for(attempt: usize) -> std::time::Duration {
    std::time::Duration::from_millis(250 * (1 << attempt.min(6)) as u64)
}

/// Build a Chat Completions request out of an Anthropic Messages body.
///
/// Only the messages endpoint is translatable. `count_tokens` has no
/// Chat Completions equivalent, and answering it with a guess would corrupt the
/// client's context accounting — so it is refused rather than approximated.
///
/// # Errors
///
/// A human-readable reason when this path cannot be translated.
fn translate_request(
    body: &Bytes,
    path: &str,
    decision: &RouteDecision,
    peek: &RequestPeek,
) -> Result<UpstreamRequest, String> {
    if !path.ends_with("/v1/messages") {
        return Err(format!("{path} has no cross-family equivalent"));
    }
    let parsed: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| format!("could not parse the request body for translation: {e}"))?;
    let model = decision
        .model
        .as_deref()
        .or(peek.requested_model.as_deref())
        .unwrap_or("default");

    let (translated, dropped) =
        ironwire_translate::anthropic_to_chat_completions(&parsed, model, peek.stream);
    if !dropped.is_empty() {
        tracing::info!(
            backend = %decision.backend,
            thinking_blocks = dropped.thinking_blocks,
            cache_breakpoints = dropped.cache_breakpoints,
            images = dropped.images,
            "translated across API families; these did not survive"
        );
    }
    let body = serde_json::to_vec(&translated)
        .map_err(|e| format!("could not serialise the translated request: {e}"))?;

    Ok(UpstreamRequest {
        path: "/v1/chat/completions".to_string(),
        body: Bytes::from(body),
        // The inbound headers describe the Anthropic protocol; none of them
        // mean anything to a Chat Completions endpoint.
        headers: Vec::new(),
        stream: peek.stream,
    })
}

/// Map a Chat Completions response back into the Anthropic shape the client is
/// waiting for.
fn translate_response(
    response: UpstreamResponse,
    requested_model: &str,
    stream: bool,
) -> UpstreamResponse {
    let mut headers: Vec<(String, String)> = response
        .headers
        .into_iter()
        // Length and encoding describe the pre-translation body.
        .filter(|(name, _)| name != "content-length" && name != "content-encoding")
        .collect();
    headers.retain(|(name, _)| name != "content-type");
    headers.push((
        "content-type".to_string(),
        if stream {
            "text/event-stream".to_string()
        } else {
            "application/json".to_string()
        },
    ));

    let body = if stream {
        translated_stream(response.body, requested_model.to_string()).boxed()
    } else {
        translated_body(response.body, requested_model.to_string()).boxed()
    };

    UpstreamResponse {
        status: response.status,
        headers,
        body,
    }
}

/// Buffer a non-streaming response and re-emit it in the Anthropic shape.
fn translated_body(
    inner: futures_util::stream::BoxStream<'static, Result<Bytes, UpstreamError>>,
    requested_model: String,
) -> impl Stream<Item = Result<Bytes, UpstreamError>> + Send {
    futures_util::stream::once(async move {
        let collected: Vec<Bytes> = inner.filter_map(|c| async move { c.ok() }).collect().await;
        let mut raw = Vec::new();
        for chunk in collected {
            raw.extend_from_slice(&chunk);
        }
        let parsed: serde_json::Value =
            serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null);
        let translated =
            ironwire_translate::chat_completion_to_anthropic(&parsed, &requested_model);
        Ok(Bytes::from(
            serde_json::to_vec(&translated).unwrap_or_else(|_| b"{}".to_vec()),
        ))
    })
}

/// Translate the event stream chunk by chunk, so text still arrives live.
fn translated_stream(
    inner: futures_util::stream::BoxStream<'static, Result<Bytes, UpstreamError>>,
    requested_model: String,
) -> impl Stream<Item = Result<Bytes, UpstreamError>> + Send {
    let state = (inner, Some(ChatToAnthropicStream::new(requested_model)));
    futures_util::stream::unfold(state, |(mut inner, mut translator)| async move {
        let mut active = translator.take()?;
        loop {
            match inner.next().await {
                Some(Ok(chunk)) => {
                    let out = active.push(&chunk);
                    if out.is_empty() {
                        // Nothing to forward yet — keep reading rather than
                        // emitting an empty frame.
                        continue;
                    }
                    translator = Some(active);
                    return Some((Ok(Bytes::from(out)), (inner, translator)));
                }
                // The upstream failed mid-stream. We are past the point of no
                // return (PROTOCOL.md §5), so close the client's stream
                // properly instead of leaving it hanging.
                Some(Err(_)) | None => {
                    let out = active.finish();
                    return Some((Ok(Bytes::from(out)), (inner, None)));
                }
            }
        }
    })
}

/// Rewrite the `model` key, and only that key.
///
/// Returns the original bytes untouched when policy did not change the model —
/// which is the common case, and the safest possible request: no parse, no
/// re-encode, no chance of a fidelity bug. `serde_json`'s `preserve_order`
/// feature keeps field order stable when an edit *is* needed.
#[must_use]
pub fn apply_model(body: &Bytes, model: Option<&str>) -> Bytes {
    let Some(model) = model else {
        return body.clone();
    };
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.clone();
    };
    let Some(object) = value.as_object_mut() else {
        return body.clone();
    };
    object.insert(
        "model".to_string(),
        serde_json::Value::String(model.to_string()),
    );
    serde_json::to_vec(&value).map_or_else(|_| body.clone(), Bytes::from)
}

/// SSE dialect matching an inbound protocol.
#[must_use]
pub fn dialect_for(protocol: Protocol) -> Dialect {
    match protocol {
        Protocol::AnthropicMessages => Dialect::Anthropic,
        Protocol::OpenAiResponses => Dialect::OpenAiResponses,
        Protocol::OpenAiChat => Dialect::OpenAiChat,
    }
}

/// Wrap a response body so it is observed on the way past.
///
/// The observer reads a copy and can only ever learn less; it has no way to
/// stall, alter or fail the forwarded bytes. `on_finish` runs when the stream
/// ends — including when the client disconnects early, so an abandoned request
/// still records what it consumed.
pub fn observe_body<S>(
    inner: S,
    dialect: Dialect,
    on_finish: impl FnOnce(Observation) + Send + 'static,
) -> impl Stream<Item = Result<Bytes, UpstreamError>> + Send
where
    S: Stream<Item = Result<Bytes, UpstreamError>> + Send + Unpin + 'static,
{
    // The callback is boxed rather than generic so `Tee` is unconditionally
    // `Unpin`: a `Drop` impl cannot carry bounds the struct does not, and we
    // need `Drop` to be the thing that guarantees the flush.
    struct Tee<S> {
        inner: S,
        observer: Option<SseObserver>,
        on_finish: Option<Box<dyn FnOnce(Observation) + Send>>,
    }

    impl<S> Tee<S> {
        fn flush(&mut self) {
            if let (Some(observer), Some(on_finish)) = (self.observer.take(), self.on_finish.take())
            {
                on_finish(observer.finish());
            }
        }
    }

    // Firing on Drop rather than only on stream end is what makes cancellation
    // accounting correct: a client that hits Esc mid-response still leaves us
    // with whatever the provider had already reported.
    impl<S> Drop for Tee<S> {
        fn drop(&mut self) {
            self.flush();
        }
    }

    impl<S> Stream for Tee<S>
    where
        S: Stream<Item = Result<Bytes, UpstreamError>> + Unpin,
    {
        type Item = Result<Bytes, UpstreamError>;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            let this = self.get_mut();
            let polled = std::pin::Pin::new(&mut this.inner).poll_next(cx);
            match &polled {
                std::task::Poll::Ready(Some(Ok(chunk))) => {
                    if let Some(observer) = this.observer.as_mut() {
                        observer.push(chunk);
                    }
                }
                std::task::Poll::Ready(_) => this.flush(),
                std::task::Poll::Pending => {}
            }
            polled
        }
    }

    Tee {
        inner,
        observer: Some(SseObserver::new(dialect)),
        on_finish: Some(Box::new(on_finish)),
    }
}

/// Fold an observation into a backend's quota state.
pub fn record(backend: &Arc<dyn Backend>, observation: &Observation) {
    if !observation.is_empty() {
        backend.record(observation);
    }
}

/// Everything the ledger needs that the observation itself does not carry.
///
/// Assembled before the response streams and consumed when it ends — including
/// when the client disconnects early, so an abandoned request is still on the
/// record with whatever the provider had already reported.
#[derive(Debug, Clone)]
pub struct LedgerContext {
    /// When the request arrived.
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Monotonic start, for durations.
    pub started: std::time::Instant,
    /// Façade that received it.
    pub facade: &'static str,
    /// Path beneath the façade.
    pub path: String,
    /// Opaque conversation key.
    pub conversation: String,
    /// Backend that served it.
    pub backend: String,
    /// Model the client asked for.
    pub requested_model: Option<String>,
    /// Fidelity rung, lowercased.
    pub rung: String,
    /// Backends tried before this one succeeded.
    pub attempts: usize,
    /// Status returned to the client.
    pub status: u16,
}

impl LedgerContext {
    /// Write one exchange.
    ///
    /// Never propagates: a ledger problem must not fail a user's inference
    /// request, and by the time this runs the response has already been
    /// delivered anyway.
    pub fn write(self, ledger: &Ledger, observation: &Observation) {
        let usage = observation.usage;
        // Priced against whichever model actually served it, so a fallback to a
        // cheaper model shows up as a cheaper turn.
        let cost_usd = ironwire_ledger::price(
            observation
                .served_model
                .as_deref()
                .or(self.requested_model.as_deref())
                .unwrap_or("unknown"),
            usage.and_then(|u| {
                Some((
                    u32::try_from(u.input_tokens).ok()?,
                    u32::try_from(u.output_tokens).ok()?,
                    u32::try_from(u.cache_read_tokens).ok()?,
                    u32::try_from(u.cache_creation_tokens).ok()?,
                ))
            }),
        );
        let exchange = Exchange {
            started_at: self.started_at,
            ttfb_ms: None,
            total_ms: i64::try_from(self.started.elapsed().as_millis()).ok(),
            facade: self.facade.to_string(),
            path: self.path,
            conversation: self.conversation,
            backend: self.backend,
            requested_model: self.requested_model,
            served_model: observation.served_model.clone(),
            rung: self.rung,
            attempts: i64::try_from(self.attempts).unwrap_or(i64::MAX),
            // `None`, not `0`, when the provider reported nothing: a fabricated
            // zero would silently understate the user's spend.
            input_tokens: usage.and_then(|u| i64::try_from(u.input_tokens).ok()),
            cache_read_tokens: usage.and_then(|u| i64::try_from(u.cache_read_tokens).ok()),
            cache_write_tokens: usage.and_then(|u| i64::try_from(u.cache_creation_tokens).ok()),
            output_tokens: usage.and_then(|u| i64::try_from(u.output_tokens).ok()),
            cost_usd,
            status: i64::from(self.status),
            error: None,
        };
        if let Err(error) = ledger.record(&exchange) {
            tracing::debug!(%error, "could not write the trace ledger entry");
        }
    }
}

/// Convenience wrapper so a boxed upstream body can be observed.
pub fn observe_boxed(
    body: futures_util::stream::BoxStream<'static, Result<Bytes, UpstreamError>>,
    dialect: Dialect,
    on_finish: impl FnOnce(Observation) + Send + 'static,
) -> futures_util::stream::BoxStream<'static, Result<Bytes, UpstreamError>> {
    observe_body(body, dialect, on_finish).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use std::sync::Mutex;

    #[test]
    fn an_unchanged_model_forwards_the_original_bytes_exactly() {
        // The safest request is the one we did not re-encode.
        let body = Bytes::from_static(br#"{"model":"claude-opus-4-6","weird":[1,2,3]}"#);
        let out = apply_model(&body, None);
        assert_eq!(out, body);
    }

    #[test]
    fn a_model_edit_changes_only_the_model() {
        let body = Bytes::from_static(
            br#"{"model":"claude-opus-4-6","stream":true,"future_field":{"a":1}}"#,
        );
        let out = apply_model(&body, Some("claude-sonnet-4-6"));
        let value: serde_json::Value = serde_json::from_slice(&out).expect("valid json");
        assert_eq!(value["model"], "claude-sonnet-4-6");
        assert_eq!(value["stream"], true);
        assert_eq!(
            value["future_field"]["a"], 1,
            "fields we do not model must survive an edit"
        );
    }

    #[test]
    fn a_model_edit_preserves_field_order() {
        let body = Bytes::from_static(br#"{"model":"a","zzz":1,"aaa":2}"#);
        let out = apply_model(&body, Some("b"));
        let text = String::from_utf8(out.to_vec()).expect("utf8");
        assert!(
            text.find("zzz").unwrap() < text.find("aaa").unwrap(),
            "preserve_order must keep the client's field order: {text}"
        );
    }

    #[test]
    fn an_unparseable_body_is_forwarded_rather_than_mangled() {
        // If we cannot understand it, passing it through unchanged is strictly
        // better than guessing.
        let body = Bytes::from_static(b"not json");
        assert_eq!(apply_model(&body, Some("x")), body);
    }

    #[tokio::test]
    async fn the_tee_forwards_bytes_verbatim_and_reports_usage() {
        let frames = vec![
            Ok(Bytes::from_static(
                br#"data: {"type":"message_start","message":{"model":"claude-opus-4-6","usage":{"input_tokens":7}}}"#,
            )),
            Ok(Bytes::from_static(b"\n\n")),
            Ok(Bytes::from_static(
                br#"data: {"type":"message_delta","usage":{"output_tokens":42}}"#,
            )),
            Ok(Bytes::from_static(b"\n\n")),
        ];
        let expected: Vec<u8> = frames
            .iter()
            .filter_map(|f| f.as_ref().ok())
            .flat_map(|b| b.to_vec())
            .collect();

        let seen = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&seen);
        let observed = observe_body(stream::iter(frames), Dialect::Anthropic, move |obs| {
            *sink.lock().expect("lock") = Some(obs);
        });

        let collected: Vec<u8> = observed
            .filter_map(|r| async move { r.ok() })
            .flat_map(|b| stream::iter(b.to_vec()))
            .collect()
            .await;

        assert_eq!(collected, expected, "forwarded bytes must be verbatim");
        let obs = seen.lock().expect("lock").clone().expect("observed");
        let usage = obs.usage.expect("usage");
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 42);
    }

    #[tokio::test]
    async fn abandoning_the_stream_still_records_what_was_consumed() {
        // A user hitting Esc must not lose the quota accounting for the tokens
        // the provider already generated.
        let frames = vec![
            Ok(Bytes::from_static(
                br#"data: {"type":"message_start","message":{"model":"m","usage":{"input_tokens":9}}}"#,
            )),
            Ok(Bytes::from_static(b"\n\n")),
            Ok(Bytes::from_static(b"data: never read\n\n")),
        ];
        let seen = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&seen);
        let mut observed = Box::pin(observe_body(
            stream::iter(frames),
            Dialect::Anthropic,
            move |obs| {
                *sink.lock().expect("lock") = Some(obs);
            },
        ));

        // Read two chunks, then drop mid-stream.
        let _ = observed.next().await;
        let _ = observed.next().await;
        drop(observed);

        let obs = seen
            .lock()
            .expect("lock")
            .clone()
            .expect("recorded on drop");
        assert_eq!(obs.usage.expect("usage").input_tokens, 9);
    }

    #[tokio::test]
    async fn a_stream_error_still_flushes_the_observation() {
        let frames = vec![
            Ok(Bytes::from_static(
                br#"data: {"type":"message_start","message":{"model":"m","usage":{"input_tokens":3}}}"#,
            )),
            Ok(Bytes::from_static(b"\n\n")),
            Err(UpstreamError::Transport {
                backend: ironwire_core::protocol::BackendId::from("x"),
                detail: "reset".into(),
            }),
        ];
        let seen = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&seen);
        let observed = observe_body(stream::iter(frames), Dialect::Anthropic, move |obs| {
            *sink.lock().expect("lock") = Some(obs);
        });
        let _: Vec<_> = observed.collect().await;
        assert!(seen.lock().expect("lock").is_some());
    }
}
