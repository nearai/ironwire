//! The local control API.
//!
//! `ironwire status` and the macOS menu bar app are both clients of this — the
//! daemon is the only place routing logic lives, so CLI and GUI cannot drift
//! (`docs/DESIGN.md` §6).
//!
//! Loopback is not sufficient authorisation on a shared machine: this surface
//! exposes the ledger and can change where a user's requests go, so it also
//! requires the token in `$IRONWIRE_HOME/control.token` (mode 0600).

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ironwire_core::protocol::BackendId;
use ironwire_core::quota::Headroom;
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// One backend, as `ironwire status` renders it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendView {
    /// Stable id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// `subscription` / `api_key` / `credits` / `local`.
    pub kind: String,
    /// Whether a credential was found.
    pub authenticated: bool,
    /// Whether consent has been recorded, where it is required.
    pub consented: bool,
    /// Why not authenticated, when applicable.
    pub detail: Option<String>,
    /// Observed capacity — or `unknown`. Never a guess (`docs/CRITIQUE.md` §4).
    pub headroom: HeadroomView,
    /// Models offered.
    pub models: Vec<String>,
}

/// Observed capacity, flattened for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HeadroomView {
    /// The provider reported a number, this long ago.
    Observed {
        /// Percent of the window consumed.
        used_pct: f32,
        /// Seconds since we read it.
        observed_secs_ago: i64,
        /// Seconds until the window resets, when stated.
        resets_in_secs: Option<i64>,
    },
    /// Inside a `retry-after` window.
    Exhausted {
        /// Seconds until it is worth retrying.
        retry_in_secs: i64,
    },
    /// The provider has told us nothing. Displayed as `unknown`, because
    /// showing a plausible number we made up is how the whole status surface
    /// stops being believed.
    Unknown,
}

impl HeadroomView {
    fn from(headroom: &Headroom, now: chrono::DateTime<chrono::Utc>) -> Self {
        match headroom {
            Headroom::Observed {
                used_pct,
                resets_at,
                observed_at,
            } => Self::Observed {
                used_pct: *used_pct,
                observed_secs_ago: (now - *observed_at).num_seconds(),
                resets_in_secs: resets_at.map(|r| (r - now).num_seconds()),
            },
            Headroom::Exhausted { until } => Self::Exhausted {
                retry_in_secs: (*until - now).num_seconds().max(0),
            },
            Headroom::Unknown => Self::Unknown,
        }
    }
}

/// Full daemon state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusView {
    /// IronWire version.
    pub version: String,
    /// Port in use.
    pub port: u16,
    /// Conversations with a sticky route.
    pub tracked_conversations: usize,
    /// Active pin, if any.
    pub pin: Option<String>,
    /// Every configured backend.
    pub backends: Vec<BackendView>,
}

/// Body of `POST /_ironwire/pin`.
#[derive(Debug, Clone, Deserialize)]
pub struct PinRequest {
    /// Backend to pin to. `None` clears the pin.
    pub backend: Option<String>,
    /// Model to force. Ignored when `backend` is `None`.
    pub model: Option<String>,
}

/// Routes for the control API.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/backends", get(status))
        .route("/pin", post(pin))
        .route("/health", get(health))
}

/// Unauthenticated: it reveals nothing and is what a service manager probes.
async fn health() -> Response {
    (StatusCode::OK, "ok").into_response()
}

async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    let now = chrono::Utc::now();
    let statuses = state.backends.statuses().await;
    let consent = state.consent_snapshot();

    let backends = statuses
        .iter()
        .map(|s| BackendView {
            id: s.id.to_string(),
            name: s.name.clone(),
            kind: format!("{:?}", s.kind).to_lowercase(),
            authenticated: s.authenticated,
            consented: !s.kind.requires_consent() || consent.is_granted(s.id.as_str()),
            detail: s.detail.clone(),
            headroom: HeadroomView::from(&s.quota.primary, now),
            models: s.models.iter().map(|(m, _)| m.clone()).collect(),
        })
        .collect();

    let (tracked, pin) = {
        let policy = match state.policy.lock() {
            Ok(p) => p,
            Err(poisoned) => poisoned.into_inner(),
        };
        (
            policy.tracked_conversations(),
            policy.pin().map(|(b, m)| match m {
                Some(model) => format!("{b}:{model}"),
                None => b.to_string(),
            }),
        )
    };

    axum::Json(StatusView {
        version: env!("CARGO_PKG_VERSION").to_string(),
        port: state.port,
        tracked_conversations: tracked,
        pin,
        backends,
    })
    .into_response()
}

async fn pin(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<PinRequest>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    let mut policy = match state.policy.lock() {
        Ok(p) => p,
        Err(poisoned) => poisoned.into_inner(),
    };
    policy.set_pin(
        request.backend.as_deref().map(BackendId::from),
        request.model,
    );
    (StatusCode::OK, axum::Json(serde_json::json!({"ok": true}))).into_response()
}

/// Constant-time-ish token check. The token is a local file, so this is a guard
/// against another *user* on the box, not against a network attacker.
fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if presented.as_bytes().ct_eq(state.control_token.as_bytes()) {
        return Ok(());
    }
    Err(Box::new(
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({
                "error": "control API requires the token from $IRONWIRE_HOME/control.token"
            })),
        )
            .into_response(),
    ))
}

/// Length-independent byte comparison, so a wrong token's *length* is not a
/// side channel either.
trait ConstantTimeEq {
    fn ct_eq(&self, other: &[u8]) -> bool;
}

impl ConstantTimeEq for [u8] {
    fn ct_eq(&self, other: &[u8]) -> bool {
        if self.len() != other.len() {
            return false;
        }
        self.iter()
            .zip(other)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    #[test]
    fn an_unobserved_backend_reports_unknown_not_a_number() {
        let view = HeadroomView::from(&Headroom::Unknown, Utc::now());
        assert!(matches!(view, HeadroomView::Unknown));
    }

    #[test]
    fn an_observation_carries_its_own_age() {
        let now = Utc::now();
        let view = HeadroomView::from(
            &Headroom::Observed {
                used_pct: 82.0,
                resets_at: Some(now + Duration::minutes(30)),
                observed_at: now - Duration::seconds(40),
            },
            now,
        );
        match view {
            HeadroomView::Observed {
                used_pct,
                observed_secs_ago,
                resets_in_secs,
            } => {
                assert!((used_pct - 82.0).abs() < 0.01);
                assert_eq!(observed_secs_ago, 40);
                assert_eq!(resets_in_secs, Some(1800));
            }
            other => panic!("expected an observation, got {other:?}"),
        }
    }

    #[test]
    fn an_elapsed_retry_window_never_reports_negative_time() {
        let now = Utc::now();
        let view = HeadroomView::from(
            &Headroom::Exhausted {
                until: now - Duration::seconds(10),
            },
            now,
        );
        match view {
            HeadroomView::Exhausted { retry_in_secs } => assert_eq!(retry_in_secs, 0),
            other => panic!("expected exhaustion, got {other:?}"),
        }
    }

    #[test]
    fn token_comparison_rejects_wrong_length_and_wrong_bytes() {
        assert!(b"abcdef".ct_eq(b"abcdef"));
        assert!(!b"abcdef".ct_eq(b"abcdeg"));
        assert!(!b"abcdef".ct_eq(b"abcde"));
        assert!(!b"".ct_eq(b"x"));
    }
}
