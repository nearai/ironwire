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
    let statuses = state.backends.statuses().await;
    let consent = state.consent_snapshot();
    let candidates = state.backends.candidates(&statuses, &consent);

    let mut rejected: Vec<(String, String)> = Vec::new();
    let mut attempts = 0usize;
    let mut last_error: Option<UpstreamError> = None;

    // Bounded: each iteration marks the failed backend unavailable, so the
    // candidate set strictly shrinks. The cap is belt and braces against a
    // backend that reports itself healthy after failing.
    let max_attempts = candidates.len().max(1);
    let mut excluded: Vec<ironwire_core::protocol::BackendId> = Vec::new();

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
            match policy.decide(key.clone(), inbound, peek, &available, Utc::now()) {
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
        let request = UpstreamRequest {
            path: path.to_string(),
            body: apply_model(&body, decision.model.as_deref()),
            headers: headers.clone(),
            stream: peek.stream,
        };

        match backend.send(request).await {
            Ok(response) => {
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

    match last_error {
        Some(last) => Err(PipelineError::AllFailed { last, rejected }),
        None => Err(PipelineError::NoRoute(NoRoute::AllExhausted)),
    }
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
