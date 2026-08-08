//! The `Backend` trait and its request/response shapes.
//!
//! Note what these types are *not*: there is no `CompletionRequest`, no
//! `ChatMessage`, no `ToolCall`. `ironclaw_llm` has all of those and they are
//! right for an agent that owns its prompts — but they are a
//! chat-completions-shaped common denominator, and re-encoding through one is
//! exactly the fidelity loss the native lane exists to avoid
//! (`docs/DESIGN.md` §7). A backend here moves bytes.

use bytes::Bytes;
use futures_util::stream::BoxStream;
use ironwire_core::capability::Capabilities;
use ironwire_core::protocol::{BackendId, BackendKind, ModelTier};
use ironwire_core::quota::QuotaSnapshot;

use crate::observe::Observation;

/// A request as it leaves IronWire.
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    /// Path suffix beneath the backend's base URL, e.g. `/v1/messages`.
    pub path: String,
    /// Body bytes, already carrying any policy-driven `model` edit.
    pub body: Bytes,
    /// Inbound headers worth forwarding, already stripped of hop-by-hop and
    /// auth headers by [`crate::headers`].
    pub headers: Vec<(String, String)>,
    /// Whether the client asked for SSE.
    pub stream: bool,
}

/// A response on its way back to the client.
pub struct UpstreamResponse {
    /// Upstream status.
    pub status: http::StatusCode,
    /// Response headers, stripped of hop-by-hop headers.
    pub headers: Vec<(String, String)>,
    /// Body, streamed. Forwarded verbatim.
    pub body: BoxStream<'static, Result<Bytes, UpstreamError>>,
}

impl std::fmt::Debug for UpstreamResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamResponse")
            .field("status", &self.status)
            .field("headers", &self.headers.len())
            .finish_non_exhaustive()
    }
}

/// Failure talking to a backend.
///
/// The distinction that matters for routing is [`UpstreamError::is_retryable`]:
/// a retryable error before the first byte descends the ladder, and after the
/// first byte nothing is retryable at all (`docs/PROTOCOL.md` §5).
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    /// The backend has no usable credential.
    #[error("{backend} is not authenticated: {detail}")]
    NeedsAuth {
        /// Which backend.
        backend: BackendId,
        /// What went wrong.
        detail: String,
    },

    /// Rate limited. Carries the provider's own `retry-after` where given —
    /// never a value of our own invention.
    #[error("{backend} is rate limited")]
    RateLimited {
        /// Which backend.
        backend: BackendId,
        /// Seconds the provider asked us to wait.
        retry_after_secs: Option<u64>,
    },

    /// Transport failure: connect, TLS, timeout, reset.
    #[error("{backend} transport failure: {detail}")]
    Transport {
        /// Which backend.
        backend: BackendId,
        /// What went wrong.
        detail: String,
    },

    /// The upstream returned a non-success status we pass through unchanged.
    #[error("{backend} returned {status}")]
    Upstream {
        /// Which backend.
        backend: BackendId,
        /// Status returned.
        status: http::StatusCode,
        /// Body, for the client.
        body: Bytes,
    },

    /// A credential was about to be attached to a host that did not issue it.
    /// This is a bug, and `docs/TRUST.md` I2 says it must be impossible rather
    /// than merely unlikely.
    #[error("refusing to send a {issuer} credential to {attempted}")]
    CredentialHostMismatch {
        /// Host that issued the credential.
        issuer: &'static str,
        /// Host we were about to send it to.
        attempted: String,
    },
}

impl UpstreamError {
    /// Whether trying a different backend is worthwhile.
    ///
    /// Only meaningful before the first response byte has reached the client.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Transport { .. } | Self::NeedsAuth { .. } => true,
            Self::Upstream { status, .. } => status.is_server_error(),
            // A host mismatch is our bug. Retrying elsewhere would hide it.
            Self::CredentialHostMismatch { .. } => false,
        }
    }

    /// Whether this failure says the *backend* is unhealthy.
    ///
    /// Distinct from [`Self::is_retryable`], which asks whether some other
    /// backend should be tried. The two differ where they must:
    ///
    /// - A missing or rejected credential is worth failing over, but it is a
    ///   configuration problem, not an outage. Counting it would open a circuit
    ///   and replace "re-run `claude login`" with "temporarily unavailable".
    /// - A rate limit means the backend is working exactly as designed, and its
    ///   own quota accounting already steers us away (`docs/CRITIQUE.md` §4).
    ///   Counting it too would take a backend out twice for one event.
    #[must_use]
    pub fn indicates_unhealthy_backend(&self) -> bool {
        match self {
            Self::Transport { .. } => true,
            Self::Upstream { status, .. } => status.is_server_error(),
            Self::RateLimited { .. }
            | Self::NeedsAuth { .. }
            | Self::CredentialHostMismatch { .. } => false,
        }
    }

    /// Provider-supplied retry delay, where there is one.
    #[must_use]
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::RateLimited {
                retry_after_secs, ..
            } => *retry_after_secs,
            _ => None,
        }
    }
}

/// Current state of a backend, as `ironwire status` shows it.
#[derive(Debug, Clone)]
pub struct BackendStatus {
    /// Stable id.
    pub id: BackendId,
    /// Display name.
    pub name: String,
    /// Capacity kind.
    pub kind: BackendKind,
    /// Whether a credential was found and looks usable.
    pub authenticated: bool,
    /// Why not, when `authenticated` is false.
    pub detail: Option<String>,
    /// Observed capacity. Never estimated (`docs/CRITIQUE.md` §4).
    pub quota: QuotaSnapshot,
    /// Models offered, best first.
    pub models: Vec<(String, ModelTier)>,
}

/// A place requests can go.
#[async_trait::async_trait]
pub trait Backend: Send + Sync {
    /// Stable id.
    fn id(&self) -> &BackendId;

    /// Display name for `ironwire status`.
    fn name(&self) -> &str;

    /// Capacity kind.
    fn kind(&self) -> BackendKind;

    /// What this backend can preserve.
    fn capabilities(&self) -> &Capabilities;

    /// Models offered, best first.
    ///
    /// Owned rather than borrowed, because for some backends this is not a
    /// fixed list: the ChatGPT backend gates models behind the client version
    /// it is told about, so the real catalogue is something we *ask* for and
    /// then remember (`crate::codex_version`).
    fn models(&self) -> Vec<(String, ModelTier)>;

    /// Whether this backend requires the inbound request to carry the
    /// originating product's own client identity (`docs/TRUST.md` §3).
    fn requires_client_identity(&self) -> bool {
        false
    }

    /// Current status, including a fresh credential check.
    async fn status(&self) -> BackendStatus;

    /// Send a request and return the response, streaming.
    ///
    /// # Errors
    ///
    /// Any [`UpstreamError`]. Implementations must not retry internally: retry
    /// and failover are the router's decision, because only it knows whether a
    /// byte has already reached the client.
    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError>;

    /// Fold an observation drawn off the wire into this backend's quota state.
    fn record(&self, observation: &Observation);

    /// Latest observed quota.
    fn quota(&self) -> QuotaSnapshot;

    /// Verify this backend actually works, right now, over the network.
    ///
    /// `ironwire doctor` calls this because a config that parses and a
    /// credential that exists prove nothing: the failures that matter — an
    /// expired token, a beta flag the provider stopped honouring, an account
    /// not entitled to a model — only appear on the wire.
    ///
    /// Implementations must probe **without** claiming another product's
    /// identity (`docs/TRUST.md` §3). For subscription backends that rules out
    /// a synthetic inference call and leaves an auth-only check.
    ///
    /// # Errors
    ///
    /// Any [`UpstreamError`] the probe surfaces.
    async fn probe(&self) -> Result<(), UpstreamError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> BackendId {
        BackendId::from("test")
    }

    #[test]
    fn transient_failures_are_worth_another_backend() {
        assert!(
            UpstreamError::RateLimited {
                backend: id(),
                retry_after_secs: Some(30)
            }
            .is_retryable()
        );
        assert!(
            UpstreamError::Transport {
                backend: id(),
                detail: "reset".into()
            }
            .is_retryable()
        );
    }

    #[test]
    fn client_errors_are_not_retried_elsewhere() {
        // A 400 is the client's request being wrong; another backend will
        // reject it identically, and hiding that helps nobody.
        let err = UpstreamError::Upstream {
            backend: id(),
            status: http::StatusCode::BAD_REQUEST,
            body: Bytes::new(),
        };
        assert!(!err.is_retryable());

        let err = UpstreamError::Upstream {
            backend: id(),
            status: http::StatusCode::BAD_GATEWAY,
            body: Bytes::new(),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn a_credential_host_mismatch_is_never_retried_away() {
        // TRUST.md I2 — this is our bug, and failing loudly is the point.
        let err = UpstreamError::CredentialHostMismatch {
            issuer: "api.anthropic.com",
            attempted: "evil.example".into(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn retry_after_comes_from_the_provider_or_not_at_all() {
        assert_eq!(
            UpstreamError::RateLimited {
                backend: id(),
                retry_after_secs: Some(42)
            }
            .retry_after_secs(),
            Some(42)
        );
        assert_eq!(
            UpstreamError::RateLimited {
                backend: id(),
                retry_after_secs: None
            }
            .retry_after_secs(),
            None,
            "we must not invent a delay the provider did not give"
        );
    }
}
