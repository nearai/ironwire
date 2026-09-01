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
use ironwire_core::config::PrivacyMode;
use ironwire_core::protocol::BackendId;
use ironwire_core::quota::Headroom;
use ironwire_creds::ConsentLedger;
use ironwire_creds::consent::ConsentPrompt;
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
    /// Serial of the signed catalog document in force; `0` means the values this
    /// binary shipped with (`docs/UPDATES.md`).
    pub catalog_serial: u64,
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
    /// How far down the ladder this route sits.
    ///
    /// Carried rather than left to the caller, because the alternative is a
    /// client inferring "is this degraded" from backend *names* — a second
    /// implementation of a routing question, in a language that cannot see the
    /// policy, drifting the moment the ladder changes. Defaulted on the way in
    /// so an older daemon still parses.
    #[serde(default)]
    pub rung: ironwire_core::policy::Rung,
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
    /// Cache reads as a fraction of every prompt token in the window, when
    /// anything reported usage. The prompt cache is the largest cost lever in a
    /// coding session and the thing this router's whole design protects; it was
    /// the one number nobody could see.
    #[serde(default)]
    pub cache_hit_rate: Option<f64>,
    /// Exchanges summarised for that rate, so the figure carries its basis.
    #[serde(default)]
    pub cache_exchanges: i64,
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

/// The settings a client may change, and everything it needs to render them
/// without deciding anything itself.
///
/// The menu bar app is the reason this exists. It could read `privacy` off
/// [`StatusView`] and offer four buttons — and it would be wrong, because
/// whether `full` is even selectable depends on `trusted_backends`, which is a
/// rule that lives in [`Config::validate`] and would then have a second,
/// drifting implementation in Swift. So the daemon says which options exist,
/// which are selectable, and why not (`docs/DESIGN.md` §6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsView {
    /// The privacy filter, and the modes it could be switched to.
    pub privacy: PrivacySettingsView,
    /// Everything a user can log into, and what it would take.
    pub services: Vec<ServiceView>,
    /// Every coding agent IronWire knows about, and whether it is pointed here.
    ///
    /// Separate from `services` because they answer different questions: a
    /// service is capacity IronWire can spend, a tool is something on this
    /// machine that may or may not be sending its traffic through us. A user
    /// with a working subscription and an unwired agent has everything
    /// configured and nothing routing, and only this list explains it.
    #[serde(default)]
    pub tools: Vec<ToolView>,
}

/// One coding agent, as a settings screen needs to describe it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolView {
    /// Stable id — what `ironwire disconnect <id>` accepts.
    pub id: String,
    /// What the user calls it.
    pub name: String,
    /// The file IronWire would edit, when one can be located.
    pub config_path: Option<String>,
    /// Whether the tool looks present on this machine.
    pub installed: bool,
    /// Whether its config currently routes through IronWire.
    pub wired: bool,
    /// What to run to point it here. A GUI cannot edit somebody's agent config
    /// on their behalf without the same "name the file first" ceremony the CLI
    /// does, so it names the command instead (`docs/TRUST.md` §5).
    pub connect_command: String,
}

/// The privacy filter as a settings screen sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacySettingsView {
    /// The mode in force, as it serialises: `off` / `credentials` / `pii` /
    /// `full`.
    pub mode: String,
    /// What the filter is *doing*, in the daemon's own words. Rendered
    /// verbatim or not at all — never restated as what the user is safe from
    /// (`docs/TRUST.md` I7).
    pub summary: String,
    /// Every mode, in ladder order.
    pub options: Vec<PrivacyOptionView>,
    /// Backends the user named as acceptable destinations under `full`.
    pub trusted_backends: Vec<String>,
}

/// One rung of the privacy ladder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyOptionView {
    /// The value to send back to `POST /_ironwire/privacy`.
    pub id: String,
    /// What this level substitutes, in one clause.
    pub describes: String,
    /// Whether switching to it right now would work.
    pub selectable: bool,
    /// Why it would not, when it would not. Present so a greyed-out option can
    /// say what to do about itself instead of just being greyed out.
    pub unavailable_because: Option<String>,
}

/// One thing a user can log into.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceView {
    /// Backend id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// `subscription` / `api_key` / `credits` / `local`.
    pub kind: String,
    /// Whether a credential was found.
    pub authenticated: bool,
    /// Why not, when it was not.
    pub detail: Option<String>,
    /// Whether this backend is gated behind recorded consent.
    pub requires_consent: bool,
    /// Whether that consent is currently recorded, at the current prompt
    /// version.
    pub consented: bool,
    /// The exact question that has to be answered to enable it.
    ///
    /// Carried rather than written into the client, because a second copy of a
    /// consent prompt is a second prompt — and the recorded version would go on
    /// claiming both users answered the same one (`docs/TRUST.md` §2).
    pub consent_prompt: Option<ConsentPrompt>,
    /// What to run for the part a GUI cannot do: pointing a coding agent at
    /// IronWire means editing the user's shell profile or their agent's config,
    /// which is theirs to own.
    pub connect_command: Option<String>,
}

/// Body of `POST /_ironwire/privacy`.
#[derive(Debug, Clone, Deserialize)]
pub struct PrivacyRequest {
    /// The mode to switch to.
    pub mode: String,
}

/// Body of `POST /_ironwire/consent`.
#[derive(Debug, Clone, Deserialize)]
pub struct ConsentRequest {
    /// Backend to grant or withdraw consent for.
    pub backend: String,
    /// `true` to record consent, `false` to withdraw it.
    pub granted: bool,
    /// The prompt version the user actually answered.
    ///
    /// Required, and checked against the current one. A client that has been
    /// running since before the wording changed would otherwise record consent
    /// to the new question on the strength of the old one having been shown —
    /// which is exactly what versioning the prompt exists to prevent.
    pub prompt_version: u32,
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
    /// How many exchanges to return.
    #[serde(default = "default_log_limit")]
    pub limit: usize,
    /// Return only exchanges at or after this instant, RFC 3339.
    ///
    /// Use the `Z` form (`2026-09-01T12:00:00Z`) or percent-encode the offset:
    /// a literal `+` in a query string is a space, so `+00:00` arrives as
    /// ` 00:00` and the request is rejected as malformed.
    ///
    /// Changes the order to **oldest first**, and pairs with [`Self::after_id`]
    /// to page forward through the window.
    #[serde(default)]
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    /// Return only exchanges with an `id` greater than this. Requires
    /// [`Self::since`].
    ///
    /// The cursor, and the reason `since` alone is not one. Pass the `id` of
    /// the last row you received; the next page starts after it. Keep going
    /// until a response is shorter than `limit`.
    ///
    /// Paging on the timestamp instead does not work, and fails quietly in two
    /// ways: `since` is inclusive against a column that is not unique, so the
    /// boundary row comes back on every request, and once `limit` exchanges
    /// share a timestamp the caller stops advancing at all. `id` is unique and
    /// assigned in insert order, so it is a total order over exactly the rows
    /// already seen.
    #[serde(default)]
    pub after_id: Option<i64>,
}

fn default_log_limit() -> usize {
    20
}

/// What `GET /_ironwire/log` returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogView {
    /// Whether local capture is on at all.
    pub enabled: bool,
    /// The exchanges that matched. Newest first, or oldest first when the
    /// request named a `since` -- see [`LogQuery::since`].
    ///
    /// Each carries its `id`. A caller paging a window passes the last one
    /// back as [`LogQuery::after_id`] and stops when a page is short.
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
        .route("/settings", get(settings))
        .route("/privacy", post(privacy))
        .route("/consent", post(consent))
        .route("/tools", post(tools))
        .route("/probe", post(probe))
        .route("/log", get(log))
        .route("/events", get(events))
        .route("/health", get(health))
}

/// Why `full` cannot be switched on right now, or `None` when it can.
///
/// **One rule, one place.** `GET /settings` greys the option out with this and
/// `POST /privacy` refuses with this; when they were written separately they
/// drifted immediately — the settings screen greyed `full` out while the
/// endpoint behind it accepted the very same change.
///
/// Naming a destination is not the same as having one. `trusted_backends`
/// defaults to `["nearai"]`, and that backend registers whether or not a key was
/// found, so "named" is true on machines where `full` can route nowhere.
/// Selecting it there fails every request as "rate limited", which is both wrong
/// and unactionable.
fn full_is_blocked(
    privacy: &ironwire_core::config::PrivacyConfig,
    statuses: &[ironwire_upstream::backend::BackendStatus],
) -> Option<String> {
    if privacy.trusted_backends.is_empty() {
        return Some(
            "`full` routes only to backends you have named as acceptable, and none are named. \
             Add `trusted_backends` under `[privacy]` in config.toml first — which operators \
             you trust with your data is not IronWire's call."
                .to_string(),
        );
    }
    let usable = privacy.trusted_backends.iter().any(|id| {
        statuses
            .iter()
            .any(|status| status.id.as_str() == id && status.authenticated)
    });
    (!usable).then(|| {
        format!(
            "`full` would route only to {}, and none of those has a credential yet — every \
             request would be refused. Connect one first, or name a backend you do have under \
             `trusted_backends` in config.toml.",
            privacy.trusted_backends.join(", ")
        )
    })
}

/// Body of `POST /_ironwire/tools`.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolRequest {
    /// Which tool.
    pub id: String,
    /// `true` to point it here, `false` to take it back off.
    pub connect: bool,
}

/// Point a coding agent at IronWire, or take it back off.
///
/// This is the one endpoint that writes to a file outside `$IRONWIRE_HOME`, and
/// it exists because the alternative — a menu that can see an unwired agent and
/// can only recite a command about it — is a worse answer to "why is nothing
/// routing" than doing the edit.
///
/// The ceremony survives the move. Every other path that touches somebody's
/// agent config works out the change, shows it, and only then writes; the CLI
/// prints it, and this hands `changes` and `occupied` back so a GUI can show
/// exactly the same thing. A slot the user is already using is still reported
/// and still left alone — a GUI is not a licence to take one.
async fn tools(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<ToolRequest>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }

    let catalog = state.catalog();
    let planned = if request.connect {
        ironwire_agents::tools::plan_connect(
            &request.id,
            state.config.server.port,
            catalog.current(),
        )
    } else {
        ironwire_agents::tools::plan_disconnect(&request.id, catalog.current())
    };

    let planned = match planned {
        Ok(planned) => planned,
        Err(error) => return bad_request(error.to_string()),
    };

    let mut backup = None;
    if !planned.is_noop() {
        match ironwire_agents::tools::commit(&planned) {
            Ok(written) => backup = written,
            Err(error) => {
                return server_error(format!(
                    "{} could not be written: {error}",
                    planned.path.display()
                ));
            }
        }
    }

    axum::Json(serde_json::json!({
        "ok": true,
        "path": planned.path.display().to_string(),
        "changes": planned.changes,
        "occupied": planned
            .occupied
            .iter()
            .map(|(slot, current)| serde_json::json!({"slot": slot, "current": current}))
            .collect::<Vec<_>>(),
        "backup": backup.map(|path| path.display().to_string()),
    }))
    .into_response()
}

/// What can be changed, and what it would take.
async fn settings(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    let statuses = state.backends.statuses().await;
    let consent = state.consent_snapshot();
    axum::Json(settings_view(&state, &statuses, &consent)).into_response()
}

fn settings_view(
    state: &AppState,
    statuses: &[ironwire_upstream::backend::BackendStatus],
    consent: &ConsentLedger,
) -> SettingsView {
    let privacy = state.privacy_config();

    let options = [
        PrivacyMode::Off,
        PrivacyMode::Credentials,
        PrivacyMode::Pii,
        PrivacyMode::Full,
    ]
    .into_iter()
    .map(|mode| {
        // The same rule `Config::validate` applies at startup, asked here so a
        // client can grey the option out instead of offering a switch that
        // would take every backend out of service.
        //
        // Naming a destination is not the same as having one. `trusted_backends`
        // defaults to `["nearai"]`, which is a backend that registers whether or
        // not a key was found — so "named" would be true on a machine where
        // `full` can route precisely nowhere. Selecting it there fails every
        // request as "rate limited", which is both wrong and unactionable, so
        // the usable check is part of the same question.
        let blocked = (mode == PrivacyMode::Full)
            .then(|| full_is_blocked(&privacy, statuses))
            .flatten();
        PrivacyOptionView {
            id: mode_name(mode).to_string(),
            describes: mode.describe().to_string(),
            selectable: blocked.is_none(),
            unavailable_because: blocked,
        }
    })
    .collect();

    let services = statuses
        .iter()
        .map(|status| {
            let requires_consent = status.kind.requires_consent();
            ServiceView {
                id: status.id.to_string(),
                name: status.name.clone(),
                kind: kind_name(status.kind),
                authenticated: status.authenticated,
                detail: status.detail.clone(),
                requires_consent,
                consented: !requires_consent || consent.is_granted(status.id.as_str()),
                consent_prompt: ConsentPrompt::for_backend(status.id.as_str()),
                connect_command: connect_command(status.id.as_str()),
            }
        })
        .collect();

    let tools = ironwire_agents::tools::all(state.catalog().current())
        .into_iter()
        .map(|tool| ToolView {
            id: tool.id,
            name: tool.name,
            config_path: tool.config_path.map(|path| path.display().to_string()),
            installed: tool.installed,
            wired: tool.wired,
            connect_command: tool.connect_command,
        })
        .collect();

    SettingsView {
        tools,
        privacy: PrivacySettingsView {
            mode: mode_name(privacy.mode()).to_string(),
            summary: privacy.summary(),
            options,
            trusted_backends: privacy.trusted_backends.clone(),
        },
        services,
    }
}

/// What to run for the parts a GUI has no business doing.
///
/// Pointing Claude Code at IronWire means an environment variable in the user's
/// shell profile; pointing Codex at it means editing their `config.toml`. Both
/// are files the user owns, and `ironwire connect` already shows the change
/// before making it (`docs/TRUST.md`). A menu offering to do it silently would
/// be the wrong end of that trade.
fn connect_command(backend_id: &str) -> Option<String> {
    match backend_id {
        "claude-sub" => Some("ironwire connect claude".to_string()),
        "codex-sub" => Some("ironwire connect codex".to_string()),
        "nearai" => Some("ironwire connect near".to_string()),
        _ => None,
    }
}

/// The value a mode serialises as, matching `PrivacyMode`'s snake_case.
fn mode_name(mode: PrivacyMode) -> &'static str {
    match mode {
        PrivacyMode::Off => "off",
        PrivacyMode::Credentials => "credentials",
        PrivacyMode::Pii => "pii",
        PrivacyMode::Full => "full",
    }
}

fn kind_name(kind: ironwire_core::protocol::BackendKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_else(|| format!("{kind:?}").to_lowercase())
}

/// Change the privacy mode: in the running daemon, and in `config.toml`.
///
/// Both, and in that order. A change that only took effect at the next restart
/// would be a switch that appears to do nothing, and a change that only lived
/// in memory would silently revert the next time the daemon started — the user
/// would be back on a weaker filter without ever being told.
async fn privacy(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<PrivacyRequest>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }

    let Some(mode) = parse_mode(&request.mode) else {
        return bad_request(format!(
            "unknown privacy mode `{}` (try: off, credentials, pii, full)",
            request.mode
        ));
    };

    // The same refusal `Config::validate` makes at startup, and the same one
    // `GET /settings` greys the option out with — literally the same function,
    // because a settings screen that greys an option out while the endpoint
    // behind it accepts the change is worse than either behaviour alone.
    let current = state.privacy_config();
    if mode == PrivacyMode::Full {
        let statuses = state.backends.statuses().await;
        if let Some(reason) = full_is_blocked(&current, &statuses) {
            return bad_request(reason);
        }
    }

    state.set_privacy_mode(mode);

    // Persisted after the switch, and reported separately: the running daemon
    // is already filtering the way the user asked, and a config file we could
    // not write is a smaller problem than pretending nothing happened.
    let persisted = persist_privacy_mode(&state, mode);
    let summary = state.privacy_config().summary();
    match persisted {
        Ok(()) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "ok": true,
                "mode": mode_name(mode),
                "summary": summary,
                "persisted": true,
            })),
        )
            .into_response(),
        Err(detail) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "ok": true,
                "mode": mode_name(mode),
                "summary": summary,
                "persisted": false,
                "warning": format!(
                    "This is in force now, but could not be saved, so it will revert when the \
                     daemon restarts: {detail}"
                ),
            })),
        )
            .into_response(),
    }
}

/// Write the mode into `config.toml`, preserving everything else in it.
fn persist_privacy_mode(state: &AppState, mode: PrivacyMode) -> Result<(), String> {
    let Some(path) = state.config_path() else {
        return Err("this daemon was not started from a config file".to_string());
    };
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    };
    let edited = ironwire_core::config_edit::set_privacy_mode(&existing, mode)
        .map_err(|error| error.to_string())?;
    ironwire_core::atomic::write(&path, &edited)
        .map_err(|error| format!("writing {}: {error}", path.display()))
}

/// Record or withdraw consent for a subscription backend.
///
/// The daemon reads the consent ledger on every routing decision, so this takes
/// effect on the next request rather than the next restart — unlike
/// `ironwire disconnect --subscription`, which edits the file underneath a
/// running daemon and has to say so.
async fn consent(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<ConsentRequest>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }

    let Some(prompt) = ConsentPrompt::for_backend(&request.backend) else {
        return bad_request(format!(
            "`{}` is not a backend that consent applies to",
            request.backend
        ));
    };

    // Granting is the direction that needs the check. Withdrawing consent is
    // always allowed: a user who wants to stop should never be told that the
    // version of the question they answered was too old to stop with.
    if request.granted && request.prompt_version != prompt.version {
        return bad_request(format!(
            "this asked version {} of the consent question, but the current one is version {}. \
             Re-read it and answer again — a newer question is a different question.",
            request.prompt_version, prompt.version
        ));
    }

    let recorded = state.set_consent(&request.backend, request.granted);
    match recorded {
        Ok(()) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "ok": true,
                "backend": request.backend,
                "consented": request.granted,
            })),
        )
            .into_response(),
        Err(detail) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({
                // Deliberately not "ok with a warning" like the privacy write.
                // A consent we failed to record must not be treated as granted.
                "error": format!("could not record that: {detail}"),
            })),
        )
            .into_response(),
    }
}

fn parse_mode(name: &str) -> Option<PrivacyMode> {
    match name {
        "off" => Some(PrivacyMode::Off),
        "credentials" => Some(PrivacyMode::Credentials),
        "pii" => Some(PrivacyMode::Pii),
        "full" => Some(PrivacyMode::Full),
        _ => None,
    }
}

/// A refusal that is ours rather than the caller's — the request was fine and
/// the filesystem was not.
fn server_error(message: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

fn bad_request(message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({ "error": message })),
    )
        .into_response()
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
///
/// This is the one response in the daemon with no end of its own: a client
/// holds it for as long as it likes, and a quiet system sends nothing down it
/// for hours. So it is also the one that has to watch for the daemon stopping
/// and close itself, or graceful shutdown waits for it forever
/// (`crate::shutdown`).
async fn events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }

    let mut rx = state.events.subscribe();
    let closing = state.shutdown.clone();
    let stream = async_stream::stream! {
        // A comment frame immediately, so a client knows it is connected before
        // anything has happened — otherwise `ironwire watch` looks hung on a
        // quiet system, which is the normal state.
        yield Ok::<_, std::convert::Infallible>(
            axum::body::Bytes::from_static(b": connected\n\n"),
        );
        loop {
            tokio::select! {
                // Biased, so a shutdown announced in the same breath as an
                // event wins the race: the frame would be written into a
                // connection that is about to go, and leaving is the news.
                biased;
                () = closing.begins() => {
                    // Framing, like `: connected` — both clients read comment
                    // frames as framing, and this one tells them the stream
                    // ended because the daemon stopped rather than because the
                    // connection broke.
                    yield Ok(axum::body::Bytes::from_static(b": closing\n\n"));
                    break;
                }
                received = rx.recv() => match received {
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
                },
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
        privacy: state.privacy().map(|filter| filter.summary().to_string()),
        catalog_serial: state.catalog().serial(),
        update: state.update_status(),
        last_route: state.last_route().map(|route| LastRouteView {
            backend: route.backend,
            model: route.model,
            from: route.from,
            rung: route.rung,
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
        cache_hit_rate: summary.as_ref().and_then(|s| s.cache_hit_rate),
        cache_exchanges: summary.as_ref().map_or(0, |s| s.exchanges),
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

    let limit = query.limit.min(1000);
    let exchanges = match query.since {
        // Bounded by SQLite, not by truncating afterwards: an old cutoff would
        // otherwise read the whole ledger into memory under the mutex before
        // discarding almost all of it, blocking writes for the length of the
        // scan. `after_id` is what lets the caller advance -- see `LogQuery`.
        Some(from) => ledger.page(from, query.after_id, limit).unwrap_or_default(),
        None => ledger.recent(limit).unwrap_or_default(),
    };
    let last_24h = ledger
        .summary(chrono::Utc::now() - chrono::Duration::hours(24))
        .unwrap_or_default();
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
