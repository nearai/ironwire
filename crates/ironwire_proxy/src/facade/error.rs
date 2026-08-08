//! Turning IronWire failures into errors the client already knows how to
//! handle.
//!
//! `docs/DESIGN.md` §9: a request that cannot be served returns a
//! protocol-correct error with the provider's own `retry-after` preserved.
//! Coding agents have well-tested paths for a 429 or a 529; they have no path
//! at all for an IronWire-shaped error, and inventing one turns a recoverable
//! stall into a crashed session.

use axum::Json;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use ironwire_core::policy::NoRoute;
use ironwire_upstream::backend::UpstreamError;
use serde_json::json;

use crate::pipeline::PipelineError;

/// An error rendered in a provider's own error shape.
#[derive(Debug)]
pub struct FacadeError {
    status: StatusCode,
    kind: &'static str,
    message: String,
    retry_after_secs: Option<u64>,
}

impl FacadeError {
    /// Build directly.
    #[must_use]
    pub fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    /// A malformed request body.
    #[must_use]
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request_error", message)
    }

    /// Render a pipeline failure.
    #[must_use]
    pub fn from_pipeline(error: &PipelineError) -> Self {
        match error {
            PipelineError::NoRoute(no_route) => Self::from_no_route(no_route),
            PipelineError::Upstream(upstream) | PipelineError::AllFailed { last: upstream, .. } => {
                Self::from_upstream(upstream)
            }
        }
    }

    fn from_no_route(no_route: &NoRoute) -> Self {
        match no_route {
            NoRoute::NoBackendsConfigured => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "IronWire has no backends configured. Run `ironwire connect claude`.",
            ),
            NoRoute::AllExhausted => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "Every connected backend is rate limited or unavailable. \
                 Run `ironwire status` to see when capacity returns.",
            ),
            NoRoute::AllIneligible { reasons } => {
                let detail = reasons
                    .iter()
                    .map(|(id, why)| format!("{id}: {why:?}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "api_error",
                    format!(
                        "No connected backend can serve this request without losing \
                         semantics ({detail}). IronWire refuses rather than silently \
                         degrading; see `ironwire status`."
                    ),
                )
            }
            NoRoute::RequiresClientIdentity => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "The only available capacity is a subscription reserved for its own \
                 client. IronWire does not present one product's identity to unlock \
                 another's subscription. Connect an API key with \
                 `ironwire connect anthropic-api`.",
            ),
        }
    }

    fn from_upstream(error: &UpstreamError) -> Self {
        match error {
            UpstreamError::RateLimited {
                retry_after_secs, ..
            } => Self {
                status: StatusCode::TOO_MANY_REQUESTS,
                kind: "rate_limit_error",
                message: error.to_string(),
                // The provider's own delay, never one of ours.
                retry_after_secs: *retry_after_secs,
            },
            UpstreamError::NeedsAuth { .. } => Self::new(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                error.to_string(),
            ),
            UpstreamError::Transport { .. } => {
                Self::new(StatusCode::BAD_GATEWAY, "api_error", error.to_string())
            }
            UpstreamError::Upstream { status, .. } => Self::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                "api_error",
                error.to_string(),
            ),
            UpstreamError::CredentialHostMismatch { .. } => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                error.to_string(),
            ),
        }
    }
}

impl IntoResponse for FacadeError {
    fn into_response(self) -> Response {
        let mut headers = HeaderMap::new();
        if let Some(secs) = self.retry_after_secs
            && let Ok(value) = HeaderValue::from_str(&secs.to_string())
        {
            headers.insert("retry-after", value);
        }
        // Anthropic's error envelope. Claude Code parses this shape, so an
        // IronWire failure reaches it as a failure it already handles.
        let body = json!({
            "type": "error",
            "error": { "type": self.kind, "message": self.message },
        });
        (self.status, headers, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironwire_core::capability::Ineligible;
    use ironwire_core::protocol::BackendId;

    #[test]
    fn a_rate_limit_reaches_the_client_as_a_rate_limit() {
        // Claude Code backs off correctly on a 429; it has no handling at all
        // for a bespoke IronWire status.
        let err =
            FacadeError::from_pipeline(&PipelineError::Upstream(UpstreamError::RateLimited {
                backend: BackendId::from("claude-sub"),
                retry_after_secs: Some(30),
            }));
        assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(err.retry_after_secs, Some(30));
        assert_eq!(err.kind, "rate_limit_error");
    }

    #[test]
    fn we_never_invent_a_retry_after() {
        let err =
            FacadeError::from_pipeline(&PipelineError::Upstream(UpstreamError::RateLimited {
                backend: BackendId::from("claude-sub"),
                retry_after_secs: None,
            }));
        assert_eq!(err.retry_after_secs, None);
    }

    #[test]
    fn an_ineligible_route_explains_itself_rather_than_degrading() {
        let err = FacadeError::from_pipeline(&PipelineError::NoRoute(NoRoute::AllIneligible {
            reasons: vec![(BackendId::from("nearai"), Ineligible::MidToolLoop)],
        }));
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(err.message.contains("nearai"));
        assert!(err.message.contains("MidToolLoop"));
    }

    #[test]
    fn the_identity_refusal_tells_the_user_what_to_do_about_it() {
        let err =
            FacadeError::from_pipeline(&PipelineError::NoRoute(NoRoute::RequiresClientIdentity));
        assert!(err.message.contains("ironwire connect anthropic-api"));
    }

    #[test]
    fn an_unconfigured_daemon_says_so_instead_of_timing_out() {
        let err =
            FacadeError::from_pipeline(&PipelineError::NoRoute(NoRoute::NoBackendsConfigured));
        assert!(err.message.contains("ironwire connect claude"));
    }
}
