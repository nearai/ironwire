//! A handler panic reaches the client as an error, not a dropped socket.
//!
//! Without this, a panic inside a façade handler unwinds to the connection
//! task, hyper drops the socket, and the client sees `ECONNRESET` with no
//! status, no body and no ledger row. Coding agents treat a reset as a
//! transport fault and retry the same request indefinitely, so one request
//! that always panics leaves the session stuck with nothing on screen but
//! the reset.
//!
//! Only the handler future is guarded: a panic while a streamed body is
//! being polled, after the status line has gone out, still ends the
//! connection. And only an unwinding build can catch anything here; under
//! `panic = "abort"` (this workspace's own release profile) the process ends
//! either way. An application embedding the proxy builds it under its own
//! profile, which is where this matters.

use std::panic::AssertUnwindSafe;

use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::FutureExt;

use crate::facade::error::FacadeError;

/// Which façade's error envelope a caught panic is rendered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Envelope {
    /// `{"type":"error","error":{...}}`.
    Anthropic,
    /// `{"error":{...}}`.
    OpenAi,
}

/// The message a client is shown. It never includes the panic's own text,
/// which can quote request content (a slicing panic prints the string it
/// was slicing).
pub(crate) const PANIC_MESSAGE: &str = "IronWire failed internally while handling this request. \
     This is an IronWire bug, not a provider error; repeating the same request is \
     unlikely to succeed.";

/// `router`, with every handler's panic turned into a 500 in `envelope`.
pub(crate) fn catch_panics<S>(router: Router<S>, envelope: Envelope) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(axum::middleware::from_fn(
        move |request: Request, next: Next| guard(envelope, request, next),
    ))
}

fn caught(envelope: Envelope) -> Response {
    let error = FacadeError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "api_error",
        PANIC_MESSAGE,
    );
    match envelope {
        Envelope::Anthropic => error.into_response(),
        Envelope::OpenAi => error.on_openai_facade(false).into_response(),
    }
}

async fn guard(envelope: Envelope, request: Request, next: Next) -> Response {
    // Before this guard, the connection task caught the same unwind and the
    // daemon carried on serving other requests; this changes only what the
    // client is told, not whether the daemon continues.
    match AssertUnwindSafe(next.run(request)).catch_unwind().await {
        Ok(response) => response,
        Err(_payload) => {
            // The payload is dropped unread: its text can quote the request.
            // The panic hook has already reported the location.
            tracing::error!("a façade handler panicked; answered with a 500");
            caught(envelope)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::routing::post;
    use tower::ServiceExt;

    /// The text a handler panics with: it stands for request content, which
    /// must not reach the client or the log.
    const SECRET: &str = "prompt text that must not leak";

    async fn boom() -> StatusCode {
        panic!("{SECRET}")
    }

    fn panicking() -> Router {
        Router::new()
            .route("/boom", post(boom))
            .route("/fine", post(|| async { (StatusCode::CREATED, "served") }))
    }

    async fn call(router: Router, path: &str) -> (StatusCode, String) {
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("served");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn a_panic_on_the_openai_facade_is_an_openai_shaped_500() {
        let (status, body) = call(catch_panics(panicking(), Envelope::OpenAi), "/boom").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let value: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"]["type"], "api_error");
        assert_eq!(value["error"]["message"], PANIC_MESSAGE);
        assert!(
            value.get("type").is_none(),
            "Anthropic envelope on OpenAI: {body}"
        );
    }

    #[tokio::test]
    async fn a_panic_on_the_anthropic_facade_is_an_anthropic_shaped_500() {
        let (status, body) = call(catch_panics(panicking(), Envelope::Anthropic), "/boom").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let value: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["type"], "api_error");
        assert_eq!(value["error"]["message"], PANIC_MESSAGE);
    }

    #[tokio::test]
    async fn the_panic_text_never_reaches_the_client() {
        for envelope in [Envelope::OpenAi, Envelope::Anthropic] {
            let (_, body) = call(catch_panics(panicking(), envelope), "/boom").await;
            assert!(!body.contains(SECRET), "panic text leaked: {body}");
        }
    }

    #[tokio::test]
    async fn a_handler_that_does_not_panic_is_untouched() {
        let (status, body) = call(catch_panics(panicking(), Envelope::OpenAi), "/fine").await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body, "served");
    }

    #[tokio::test]
    async fn the_router_keeps_serving_after_a_panic() {
        let router = catch_panics(panicking(), Envelope::OpenAi);
        let (first, _) = call(router.clone(), "/boom").await;
        assert_eq!(first, StatusCode::INTERNAL_SERVER_ERROR);
        let (second, _) = call(router, "/fine").await;
        assert_eq!(second, StatusCode::CREATED);
    }
}
