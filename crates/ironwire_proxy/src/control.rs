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
use chrono::{DateTime, Utc};
use ironwire_core::protocol::BackendId;
use ironwire_core::quota::Headroom;
use ironwire_creds::ConsentLedger;
use ironwire_ledger::{Exchange, Summary};
use ironwire_update::UpdateStatus;
use ironwire_usage::UsageReport;
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
    /// Circuit state, so a backend that is being skipped says so rather than
    /// looking idle.
    pub health: HealthView,
    /// Models offered.
    pub models: Vec<String>,
}

/// A backend's circuit state, flattened for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthView {
    /// `closed` / `open` / `half_open`.
    pub circuit: String,
    /// Consecutive failures counted against this backend's health.
    pub consecutive_failures: u32,
    /// Seconds until an open circuit will next allow a probe.
    pub retry_in_secs: Option<i64>,
}

impl Default for HealthView {
    fn default() -> Self {
        Self {
            circuit: "closed".to_string(),
            consecutive_failures: 0,
            retry_in_secs: None,
        }
    }
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
    /// Stopped by a spend cap the user set, not by the provider.
    CapReached {
        /// Spent against the cap in this window.
        spent_usd: f64,
        /// The cap.
        cap_usd: f64,
        /// Seconds until the window rolls over.
        resets_in_secs: i64,
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
            Headroom::CapReached {
                spent_usd,
                cap_usd,
                resets_at,
            } => Self::CapReached {
                spent_usd: *spent_usd,
                cap_usd: *cap_usd,
                resets_in_secs: (*resets_at - now).num_seconds().max(0),
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
    /// Every pool, seen as one balance.
    pub balance: BalanceView,
    /// What the privacy filter is *doing*, never what the user is safe from
    /// (`docs/TRUST.md` I7). `None` when it is off.
    pub privacy: Option<String>,
    /// Serial of the signed quirks document in force; `0` means the values this
    /// binary shipped with (`docs/UPDATES.md`).
    pub quirks_serial: u64,
    /// What the last update check concluded. IronWire never applies an update
    /// itself — this is notification, not action.
    pub update: UpdateStatus,
    /// The most recent route this daemon took, for a status line to display.
    ///
    /// Defaulted on the way in so a newer CLI can read an older daemon's status
    /// instead of refusing it.
    #[serde(default)]
    pub last_route: Option<LastRouteView>,
    /// How fast capacity is going, measured from the local ledger.
    ///
    /// Not quota. [`HeadroomView`] is still the only thing on this screen that
    /// claims to describe a *provider's* remaining capacity, and it still says
    /// `unknown` when the provider has not spoken. This is a measurement of
    /// IronWire's own traffic and every figure in it carries the basis it was
    /// derived from (`ironwire_usage`).
    ///
    /// Empty when capture is off, when `usage.enabled = false`, or when there
    /// is no open window. Defaulted so an older daemon still parses.
    #[serde(default)]
    pub usage: UsageReport,
}

/// The most recent routing decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastRouteView {
    /// Backend chosen.
    pub backend: String,
    /// Model sent upstream, when policy named one. `None` means the client's
    /// own choice went through untouched, which is the ordinary case.
    pub model: Option<String>,
    /// Backend the conversation was on before, when this was a change. This is
    /// the field a status line exists for: a fallback that nobody notices is
    /// one the user cannot act on.
    pub from: Option<String>,
    /// When it happened.
    pub at: DateTime<Utc>,
}

/// Every pool, seen as one balance.
///
/// Deliberately a *count* of pools rather than a single merged number. The
/// pools are not commensurable: a Claude Max five-hour window, a ChatGPT weekly
/// window, and a dollar balance have no shared unit, and averaging their
/// percentages would produce a figure that looks authoritative and means
/// nothing. What a user actually needs to know is how many places they can still
/// go, whether any of them is free, and when the closed ones come back —
/// all of which are answerable without inventing a unit.
///
/// Every field here is derived from an *observation* or from a recorded cost.
/// Nothing is estimated (`docs/CRITIQUE.md` §4).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BalanceView {
    /// Pools that could serve a request right now.
    pub available: usize,
    /// Available pools whose marginal cost is zero — a subscription or credits
    /// already paid for. The number that decides whether a user should worry.
    pub free_available: usize,
    /// Pools that are authenticated and consented but have not yet reported
    /// their capacity. Named rather than folded into `available`, because "we
    /// do not know" is not "there is room".
    pub unknown: usize,
    /// Pools that are exhausted, or whose circuit is open.
    pub unavailable: usize,
    /// When the first unavailable pool is expected back.
    pub next_available_at: Option<DateTime<Utc>>,
    /// Spend on *metered* backends in the last 24 hours. `None` when the
    /// ledger is off — which means unmeasured, not zero.
    ///
    /// Subscription and credit traffic is deliberately not in here. The ledger
    /// prices every exchange, including the ones served by a subscription, and
    /// summing all of it produced a "spend" figure for a day on which nothing
    /// was billed — the opposite of what this proxy exists to tell you.
    pub spend_today_usd: Option<f64>,
    /// The configured daily spend cap and what has gone against it, when the
    /// user set one. A permanent line rather than a startup message, following
    /// the privacy filter's precedent: a limit you cannot see is one you cannot
    /// trust.
    #[serde(default)]
    pub spend_cap: Option<SpendCapView>,
    /// What each subscription has used of its own window, as the provider
    /// reported it. The unit that matters for capacity already paid for: a
    /// percentage, not a price.
    ///
    /// Defaulted on the way in so a newer CLI against an older daemon renders
    /// what it can instead of refusing the whole status screen — the daemon
    /// outlives the shell that talks to it, so the two versions *will* differ.
    #[serde(default)]
    pub subscription_used: Vec<SubscriptionUse>,
}

/// A configured spend cap, and progress against it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpendCapView {
    /// Spent against the cap in this window.
    pub spent_usd: f64,
    /// The cap.
    pub cap_usd: f64,
}

/// One subscription's consumption of its own window.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscriptionUse {
    /// Display name, e.g. `Claude subscription`.
    pub name: String,
    /// Percent of the window consumed, as the provider reported it. `None`
    /// means the provider has not said — never a guess.
    pub used_pct: Option<f32>,
    /// Exchanges served in the last 24 hours, from the local ledger.
    pub exchanges: i64,
}

/// Body of `POST /_ironwire/pin`.
#[derive(Debug, Clone, Deserialize)]
pub struct PinRequest {
    /// Backend to pin to. `None` clears the pin.
    pub backend: Option<String>,
    /// Model to force. Ignored when `backend` is `None`.
    pub model: Option<String>,
}

/// One backend's live-probe verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeView {
    /// Stable id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Whether the probe succeeded.
    pub ok: bool,
    /// Round-trip milliseconds.
    pub latency_ms: u64,
    /// What went wrong, when it did.
    pub error: Option<String>,
}

/// Query for `GET /_ironwire/log`.
#[derive(Debug, Clone, Deserialize)]
pub struct LogQuery {
    /// How many exchanges to return, newest first.
    #[serde(default = "default_log_limit")]
    pub limit: usize,
}

fn default_log_limit() -> usize {
    20
}

/// What `GET /_ironwire/log` returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogView {
    /// Whether local capture is on at all.
    pub enabled: bool,
    /// Recent exchanges, newest first.
    pub exchanges: Vec<Exchange>,
    /// Aggregate over the last 24 hours.
    pub last_24h: Summary,
}

/// Routes for the control API.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/backends", get(status))
        .route("/pin", post(pin))
        .route("/probe", post(probe))
        .route("/log", get(log))
        .route("/events", get(events))
        .route("/health", get(health))
}

/// Unauthenticated: it reveals nothing and is what a service manager probes.
async fn health() -> Response {
    (StatusCode::OK, "ok").into_response()
}

/// Live route and health events, as SSE.
///
/// What `ironwire watch` and the menu bar app both read. IronWire cannot put a
/// line in a coding agent's transcript — the only writable channel is the
/// response stream itself, and injecting there would put words in the model's
/// mouth — so this side channel is how a user finds out that their family
/// changed (`crate::events`).
async fn events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }

    let mut rx = state.events.subscribe();
    let stream = async_stream::stream! {
        // A comment frame immediately, so a client knows it is connected before
        // anything has happened — otherwise `ironwire watch` looks hung on a
        // quiet system, which is the normal state.
        yield Ok::<_, std::convert::Infallible>(
            axum::body::Bytes::from_static(b": connected\n\n"),
        );
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let payload = serde_json::to_string(&event)
                        .unwrap_or_else(|_| "{}".to_string());
                    yield Ok(axum::body::Bytes::from(format!("data: {payload}\n\n")));
                }
                // The bus is lossy on purpose: a subscriber that fell behind is
                // told so rather than silently shown a gap.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    yield Ok(axum::body::Bytes::from(format!(
                        ": lagged {n}\n\n"
                    )));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(axum::body::Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    let now = chrono::Utc::now();
    let statuses = state.backends.statuses().await;
    let consent = state.consent_snapshot();

    let health = state.breakers.statuses();
    let backends: Vec<BackendView> = statuses
        .iter()
        .map(|s| BackendView {
            id: s.id.to_string(),
            name: s.name.clone(),
            // Serde's `snake_case` rename, not `Debug` lowercased: the latter
            // renders `ApiKey` as `apikey`, which `status` then prints as the
            // backend's kind. Nobody saw it until an API-key backend could
            // actually be configured.
            kind: serde_json::to_value(s.kind)
                .ok()
                .and_then(|value| value.as_str().map(ToString::to_string))
                .unwrap_or_else(|| format!("{:?}", s.kind).to_lowercase()),
            authenticated: s.authenticated,
            consented: !s.kind.requires_consent() || consent.is_granted(s.id.as_str()),
            detail: s.detail.clone(),
            headroom: HeadroomView::from(&s.quota.primary, now),
            health: health.iter().find(|h| h.backend == s.id).map_or_else(
                HealthView::default,
                |h| HealthView {
                    circuit: format!("{:?}", h.state).to_lowercase(),
                    consecutive_failures: h.consecutive_failures,
                    retry_in_secs: h.retry_at.map(|at| (at - now).num_seconds().max(0)),
                },
            ),
            models: s.models.iter().map(|(m, _)| m.clone()).collect(),
        })
        .collect();

    let balance = balance(
        &statuses,
        &consent,
        &health,
        state.ledger.as_ref(),
        &state.config.limits,
        now,
    );

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
        balance,
        privacy: state
            .privacy
            .as_ref()
            .map(|filter| filter.summary().to_string()),
        quirks_serial: state.quirks().serial(),
        update: state.update_status(),
        last_route: state.last_route().map(|route| LastRouteView {
            backend: route.backend,
            model: route.model,
            from: route.from,
            at: route.at,
        }),
        usage: usage(&state, now),
    })
    .into_response()
}

/// Measure IronWire's own traffic from the ledger.
///
/// A ledger read failure yields an empty report rather than a failed status
/// call: the capacity numbers above are the reason someone ran this command,
/// and losing them because a burn-rate query could not run would be the wrong
/// trade every time.
/// Memoised by [`AppState::usage_report`], keyed on the ledger's write token:
/// `ironwire statusline` calls this endpoint on every render of somebody's
/// editor, and the scan behind it reads eight days of ledger rows. The token
/// is what keeps that from ever showing a report built before the traffic the
/// caller is asking about.
fn usage(state: &AppState, now: DateTime<Utc>) -> UsageReport {
    let config = &state.config.usage;
    if !config.enabled {
        return UsageReport::default();
    }
    let Some(ledger) = state.ledger.as_ref() else {
        return UsageReport::default();
    };

    state.usage_report(now, ledger.writes(), || build_usage(config, ledger, now))
}

fn build_usage(
    config: &ironwire_core::config::UsageConfig,
    ledger: &ironwire_ledger::Ledger,
    now: DateTime<Utc>,
) -> UsageReport {
    let options = ironwire_usage::Options {
        session_hours: i64::from(config.session_hours.max(1)),
        history_hours: i64::from(config.history_hours.max(1)),
        // An unparseable plan is dropped rather than guessed at, and said out
        // loud: silently falling back to a default would put a limit on the
        // user's screen that they never declared.
        plan: config.plan.as_deref().and_then(|name| {
            let parsed = ironwire_usage::Plan::parse(name);
            if parsed.is_none() {
                tracing::warn!(
                    plan = name,
                    "unknown usage.plan; comparing against your own history instead. \
                     Known plans: pro, max5, max20, team"
                );
            }
            parsed
        }),
        ..ironwire_usage::Options::default()
    };

    match ledger.since(now - options.history()) {
        Ok(exchanges) => ironwire_usage::report(&exchanges, now, &options),
        Err(error) => {
            tracing::debug!(%error, "could not read the ledger for usage estimates");
            UsageReport::default()
        }
    }
}

/// Collapse every pool into one balance — see [`BalanceView`] for why this
/// counts pools instead of summing them.
fn balance(
    statuses: &[ironwire_upstream::backend::BackendStatus],
    consent: &ConsentLedger,
    health: &[ironwire_upstream::breaker::BreakerStatus],
    ledger: Option<&ironwire_ledger::Ledger>,
    limits: &ironwire_core::config::LimitsConfig,
    now: DateTime<Utc>,
) -> BalanceView {
    use ironwire_upstream::breaker::CircuitState;

    // Local midnight, the same window a spend cap is measured over. A rolling
    // 24 hours and a calendar day disagreeing about "today" on the one screen
    // that reports both would be indefensible.
    let summary = ledger.and_then(|l| l.summary(crate::spend::window_start(now)).ok());
    // Which backend is metered is knowable only here, where the registry is —
    // the ledger stores an id, not a kind. Everything else is priced but not
    // billed, and must not be added up as though it were.
    let metered: std::collections::HashSet<&str> = statuses
        .iter()
        .filter(|s| s.kind.is_metered())
        .map(|s| s.id.as_str())
        .collect();
    let mut view = BalanceView {
        spend_today_usd: summary.as_ref().map(|s| {
            s.cost_by_backend
                .iter()
                .filter(|(backend, _)| metered.contains(backend.as_str()))
                .map(|(_, cost)| cost)
                .sum()
        }),
        spend_cap: limits
            .daily_spend_usd
            .filter(|cap| *cap > 0.0)
            .map(|cap| SpendCapView {
                spent_usd: summary.as_ref().map_or(0.0, |s| {
                    s.cost_by_backend
                        .iter()
                        .filter(|(backend, _)| metered.contains(backend.as_str()))
                        .map(|(_, cost)| cost)
                        .sum()
                }),
                cap_usd: cap,
            }),
        subscription_used: statuses
            .iter()
            .filter(|s| {
                s.kind.requires_consent() && s.authenticated && consent.is_granted(s.id.as_str())
            })
            .map(|s| SubscriptionUse {
                name: s.name.clone(),
                used_pct: match s.quota.primary {
                    Headroom::Observed { used_pct, .. } => Some(used_pct),
                    _ => None,
                },
                exchanges: summary
                    .as_ref()
                    .and_then(|sum| {
                        sum.by_backend
                            .iter()
                            .find(|(backend, _)| backend == s.id.as_str())
                    })
                    .map_or(0, |(_, count)| *count),
            })
            .collect(),
        ..BalanceView::default()
    };
    let mut resets: Vec<DateTime<Utc>> = Vec::new();

    for status in statuses {
        // A backend the user has not logged into, or not consented to, is not a
        // pool that is *down* — it is one they have not opted into. Counting it
        // as unavailable would make a working setup look half-broken.
        if !status.authenticated
            || (status.kind.requires_consent() && !consent.is_granted(status.id.as_str()))
        {
            continue;
        }

        let circuit_open = health
            .iter()
            .find(|h| h.backend == status.id)
            .is_some_and(|h| h.state == CircuitState::Open);
        if circuit_open {
            view.unavailable += 1;
            if let Some(at) = health
                .iter()
                .find(|h| h.backend == status.id)
                .and_then(|h| h.retry_at)
            {
                resets.push(at);
            }
            continue;
        }

        match status.quota.primary {
            Headroom::Exhausted { until } => {
                view.unavailable += 1;
                resets.push(until);
            }
            // A cap is not "no capacity left" — it is capacity the user
            // declined to spend, so it counts as unavailable rather than
            // unknown, and `next_available_at` points at the rollover.
            Headroom::CapReached { resets_at, .. } => {
                view.unavailable += 1;
                resets.push(resets_at);
            }
            Headroom::Unknown => view.unknown += 1,
            Headroom::Observed { .. } => {
                view.available += 1;
                if !status.kind.is_metered() {
                    view.free_available += 1;
                }
            }
        }
    }

    view.next_available_at = resets.into_iter().min();
    view
}

async fn pin(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<PinRequest>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    // Validate before storing. An unknown backend used to be accepted, and then
    // every request silently ignored the pin and routed normally — while
    // `ironwire status` reported "Pinned to <whatever>". The user believes all
    // their traffic is on one backend and it is not, which is the same failure
    // `X-IronWire-Route` was fixed for, made worse by persisting.
    //
    // Rejected here rather than per-request, because here is where the backend
    // list is visible and the answer can name what actually exists.
    if let Some(requested) = &request.backend {
        let known: Vec<String> = state
            .backends
            .all()
            .iter()
            .map(|backend| backend.id().to_string())
            .collect();
        if !known.iter().any(|id| id == requested) {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "error": format!("`{requested}` is not a connected backend"),
                    "available": known,
                })),
            )
                .into_response();
        }
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

/// Hit every backend for real. This is what makes `ironwire doctor` worth
/// running: a credential that parses proves nothing.
async fn probe(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    let mut views = Vec::new();
    for backend in state.backends.all() {
        let started = std::time::Instant::now();
        let outcome = backend.probe().await;
        let latency = started.elapsed();
        views.push(ProbeView {
            id: backend.id().to_string(),
            name: backend.name().to_string(),
            ok: outcome.is_ok(),
            latency_ms: u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
            error: outcome.err().map(|e| e.to_string()),
        });
    }
    axum::Json(views).into_response()
}

async fn log(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<LogQuery>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    let Some(ledger) = state.ledger.as_ref() else {
        return axum::Json(LogView {
            enabled: false,
            exchanges: Vec::new(),
            last_24h: Summary::default(),
        })
        .into_response();
    };

    let since = chrono::Utc::now() - chrono::Duration::hours(24);
    let exchanges = ledger.recent(query.limit.min(1000)).unwrap_or_default();
    let last_24h = ledger.summary(since).unwrap_or_default();
    axum::Json(LogView {
        enabled: true,
        exchanges,
        last_24h,
    })
    .into_response()
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
