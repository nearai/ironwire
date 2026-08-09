//! On-disk configuration and the runtime home directory.
//!
//! Everything IronWire persists lives under `$IRONWIRE_HOME` (default
//! `~/.ironwire`, mode 0700). See `docs/PACKAGING.md` for the layout.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::DEFAULT_PORT;
use crate::error::{Error, Result};
use crate::protocol::{BackendKind, ModelTier};

/// Resolved filesystem locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathsConfig {
    /// `$IRONWIRE_HOME`.
    pub home: PathBuf,
}

impl PathsConfig {
    /// Resolve from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoHome`] when neither `$IRONWIRE_HOME` nor a home
    /// directory can be determined.
    pub fn resolve() -> Result<Self> {
        if let Some(home) = std::env::var_os("IRONWIRE_HOME") {
            return Ok(Self {
                home: PathBuf::from(home),
            });
        }
        let home = dirs::home_dir().ok_or(Error::NoHome)?.join(".ironwire");
        Ok(Self { home })
    }

    /// Root under an explicit directory. Used by tests.
    #[must_use]
    pub fn rooted_at(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// `config.toml`.
    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    /// Recorded subscription consents (`docs/TRUST.md` §2).
    #[must_use]
    pub fn consent_file(&self) -> PathBuf {
        self.home.join("consent.json")
    }

    /// Control-API bearer token, mode 0600.
    #[must_use]
    pub fn control_token_file(&self) -> PathBuf {
        self.home.join("control.token")
    }

    /// Single-daemon lockfile.
    #[must_use]
    pub fn lock_file(&self) -> PathBuf {
        self.home.join("daemon.lock")
    }

    /// Local trace ledger.
    #[must_use]
    pub fn ledger_file(&self) -> PathBuf {
        self.home.join("ledger.sqlite")
    }

    /// Cached signed provider-quirks document.
    #[must_use]
    pub fn quirks_file(&self) -> PathBuf {
        self.home.join("quirks.json")
    }

    /// Observed quota from the previous run, mode 0600
    /// (`crate::quota_store`).
    #[must_use]
    pub fn quota_file(&self) -> PathBuf {
        self.home.join("quota.json")
    }

    /// Cached result of the last update check.
    #[must_use]
    pub fn update_cache_file(&self) -> PathBuf {
        self.home.join("update.json")
    }
}

/// Listener settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Loopback port. There is deliberately no `host` field — IronWire binds
    /// `127.0.0.1` and nothing else (`docs/TRUST.md` I1).
    pub port: u16,
    /// Idle timeout for upstream requests, in seconds. Long, because coding
    /// agents legitimately generate for many minutes.
    pub upstream_timeout_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            upstream_timeout_secs: 900,
        }
    }
}

/// Trace capture settings. Local capture is on; upload is a separate, explicit
/// decision made elsewhere (`docs/TRUST.md` §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    /// Record metadata for every exchange.
    pub enabled: bool,
    /// Also record request and response bodies. Off by default: these contain
    /// the user's source code.
    pub bodies: bool,
    /// How many days of exchanges to keep.
    ///
    /// A ledger with no retention grows for the life of the install, silently,
    /// on a machine where nobody is watching a SQLite file in a dotdir. Ninety
    /// days is long enough for "what did my agent do last quarter" and short
    /// enough that the file stays a few tens of megabytes.
    ///
    /// `0` disables pruning — an explicit choice someone can make, not the
    /// default, and not the accident it currently is.
    pub retain_days: u32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bodies: false,
            retain_days: 90,
        }
    }
}

/// What `ironwire status` may say about how fast capacity is going.
///
/// Everything here describes *IronWire's own traffic*, measured from the local
/// ledger — never a provider's quota, which is reported or `unknown` and never
/// estimated (`AGENTS.md` rule 2, `docs/CRITIQUE.md` §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsageConfig {
    /// Show the session section on `ironwire status`. Needs `capture.enabled`
    /// — with no ledger there is nothing to measure.
    pub enabled: bool,
    /// Length of a session window, in hours. Five is Claude Code's, and the
    /// figure the whole comparison is calibrated against; change it only if
    /// your provider's window really is a different length.
    pub session_hours: u32,
    /// How far back to look for completed windows when calibrating against
    /// your own history. Eight days: long enough for a working week, short
    /// enough that a change in how you work shows up inside one.
    pub history_hours: u32,
    /// Your subscription plan: `pro`, `max5`, `max20`, or `team`.
    ///
    /// There is deliberately no default. Published per-window token limits do
    /// not exist — the figures in circulation are reverse-engineered — so
    /// IronWire will not assert one. Setting this makes the ceiling *your*
    /// claim about *your* plan, and the status screen says so. Left unset, the
    /// comparison is against your own past sessions, which needs no table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            session_hours: 5,
            history_hours: 192,
            plan: None,
        }
    }
}

/// Update-check settings. IronWire never applies an update itself; this only
/// governs whether it *looks* (`docs/UPDATES.md`, `docs/TRUST.md` §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    /// Check for a newer release, at most once a day. The one request IronWire
    /// makes that is not the user's own work, and switchable off.
    pub check: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self { check: true }
    }
}

/// How hard to work at keeping a streamed response alive.
///
/// These exist because a coding agent's own patience is shorter than a model's
/// thinking time, and the gap is where "Response stalled mid-stream" lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResilienceConfig {
    /// Emit an SSE `ping` after this many seconds of upstream silence.
    pub keepalive_secs: u64,
    /// Give up on a silent upstream after this many seconds and end the stream
    /// with a stated error rather than pinging forever.
    pub stall_timeout_secs: u64,
    /// How many times to transparently restart a stream that died before
    /// producing any content.
    pub max_reconnects: usize,
    /// Stall timeout for a turn that looks like compaction
    /// (`docs/PROTOCOL.md` §8).
    ///
    /// A compaction turn sends the whole conversation and asks for a long
    /// summary, so it thinks for far longer than an ordinary turn before
    /// producing a token. Applying the normal timeout to it would abandon the
    /// one turn whose output becomes permanent — and it would do so most often
    /// in exactly the longest sessions, where compaction matters most.
    pub compaction_stall_timeout_secs: u64,
    /// Stall timeout for a turn served by local capacity.
    ///
    /// A 70B model on a laptop CPU can take minutes to its first token, which
    /// the ordinary timeout reads as a dead upstream. Defaults to the ordinary
    /// one and is floored at it, so this can lengthen IronWire's patience with
    /// a slow local model and never shorten it.
    pub local_stall_timeout_secs: u64,
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self {
            keepalive_secs: 15,
            stall_timeout_secs: 180,
            max_reconnects: 2,
            compaction_stall_timeout_secs: 600,
            local_stall_timeout_secs: 180,
        }
    }
}

impl ResilienceConfig {
    /// The stall timeout to apply to this turn.
    #[must_use]
    pub fn stall_timeout_for(&self, likely_compaction: bool) -> u64 {
        if likely_compaction {
            // Never *shorter* than the ordinary timeout, whatever a user
            // configures: a config that made compaction more fragile than a
            // normal turn would be the opposite of the point.
            self.compaction_stall_timeout_secs
                .max(self.stall_timeout_secs)
        } else {
            self.stall_timeout_secs
        }
    }

    /// The stall timeout for a turn, given whether local capacity is serving it.
    ///
    /// Same floor rule, same reason: a local model is the slowest thing
    /// IronWire routes to, so a configured value below the ordinary timeout
    /// would make the one backend that needs the most patience the least
    /// patiently treated.
    #[must_use]
    pub fn stall_timeout_for_backend(&self, likely_compaction: bool, is_local: bool) -> u64 {
        let base = self.stall_timeout_for(likely_compaction);
        if is_local {
            base.max(self.local_stall_timeout_secs)
                .max(self.stall_timeout_secs)
        } else {
            base
        }
    }
}

/// The optional privacy filter.
///
/// **Off by default, and that is a trust commitment rather than a default**
/// (`docs/TRUST.md` I7). Everything else in IronWire rests on forwarding bytes
/// it did not change; this filter changes them by design, so it is never on
/// unless the user turned it on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrivacyConfig {
    /// Master switch.
    pub enabled: bool,
    /// Tier 1: substitute values with a machine-checkable secret shape.
    pub secrets: bool,
    /// Tier 2: exact strings to substitute, nominated by the user.
    ///
    /// These are the user's own sensitive values and they live in
    /// `$IRONWIRE_HOME/config.toml` (mode 0700). That is a real trade-off:
    /// listing them is what makes tier 2 work, and it also writes them down.
    /// Stated here rather than left for someone to discover.
    pub named_values: Vec<String>,
    /// Scan inside fenced code blocks. Off by default — a value in one is
    /// nearly always the code being edited, and substituting it makes the model
    /// rewrite a file into something that does not work.
    pub scan_code_blocks: bool,
    /// Scan replayed tool results. Off by default — they are output from the
    /// user's own machine that the model needs verbatim to reason about.
    pub scan_tool_results: bool,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            secrets: true,
            named_values: Vec::new(),
            scan_code_blocks: false,
            scan_tool_results: false,
        }
    }
}

impl PrivacyConfig {
    /// Whether the filter would actually do anything.
    ///
    /// Enabled-but-configured-with-nothing is a real state and it should read
    /// as off, not as protection.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled && (self.secrets || !self.named_values.is_empty())
    }

    /// One line describing what is running — never what the user is safe from
    /// (`docs/TRUST.md` I7).
    #[must_use]
    pub fn summary(&self) -> String {
        if !self.enabled {
            return "off".to_string();
        }
        let mut parts = Vec::new();
        if self.secrets {
            parts.push("secrets".to_string());
        }
        if !self.named_values.is_empty() {
            parts.push(format!("{} named value(s)", self.named_values.len()));
        }
        if parts.is_empty() {
            "on, but nothing configured to match".to_string()
        } else {
            parts.join(" + ")
        }
    }
}

/// A user-configured backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    /// Stable id, e.g. `claude-sub`.
    pub id: String,
    /// Backend implementation to construct: `claude-subscription`,
    /// `anthropic-api`, `codex-subscription`, `openai-api`, `nearai`,
    /// `openai-compatible`.
    pub kind: String,
    /// Whether the router may use it.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Base URL override, for `openai-compatible` and for testing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Environment variable holding the API key, where applicable. IronWire
    /// stores no secrets in `config.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Model slugs this backend should offer, best first. Configurable because
    /// third-party catalogues move faster than our release cadence.
    ///
    /// A bare string takes the tier IronWire infers from its name — except on
    /// a local backend, where it is [`ModelTier::Fast`] (see
    /// [`ModelEntry::tier_on`]). `{ name = "...", tier = "..." }` states one
    /// outright.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<ModelEntry>>,
}

/// A configured model: a slug, and optionally the tier it should count as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ModelEntry {
    /// Just the slug.
    Name(String),
    /// The slug and the tier the user says it belongs to.
    Tiered {
        /// Model slug, as the provider spells it.
        name: String,
        /// `fast`, `balanced` or `frontier`.
        tier: ModelTier,
    },
}

impl ModelEntry {
    /// The slug.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Name(name) | Self::Tiered { name, .. } => name,
        }
    }

    /// The tier this entry counts as on a backend of `kind`.
    ///
    /// A declared tier always wins. Otherwise the name decides — except on
    /// local capacity, where the name must not.
    ///
    /// `ModelTier::from_model_hint` resolves anything it does not recognise to
    /// `Frontier`, deliberately: for a hosted catalogue, guessing low silently
    /// downgrades a user's work. For a local catalogue the same default is
    /// catastrophic, because local capacity also sorts cheapest — `qwen3-coder:30b`
    /// would read as frontier-tier and take work meant for Opus. So a local
    /// model is `Fast` until the user says otherwise. They opt it up the
    /// ladder; IronWire never does it on their behalf.
    #[must_use]
    pub fn tier_on(&self, kind: BackendKind) -> ModelTier {
        match self {
            Self::Tiered { tier, .. } => *tier,
            Self::Name(name) => {
                if kind == BackendKind::Local {
                    ModelTier::Fast
                } else {
                    ModelTier::from_model_hint(name)
                }
            }
        }
    }
}

fn default_true() -> bool {
    true
}

/// Top-level configuration.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Listener settings.
    pub server: ServerConfig,
    /// Trace capture settings.
    pub capture: CaptureConfig,
    /// What the status screen may say about burn rate and projections.
    pub usage: UsageConfig,
    /// Update-check settings.
    pub updates: UpdateConfig,
    /// Stream-resilience settings.
    pub resilience: ResilienceConfig,
    /// Optional privacy filter. Off by default (`docs/PRIVACY.md`).
    pub privacy: PrivacyConfig,
    /// Configured backends, in preference order for ties.
    pub backends: Vec<BackendConfig>,
    /// Spend caps. No cap unless the user sets one.
    pub limits: LimitsConfig,
}

/// What to do when a spend cap is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreachAction {
    /// Keep working on whatever free capacity remains, following the ordinary
    /// ladder. The default, because the product's promise is that the agent
    /// does not die — a cap that killed the session by default would invert it.
    #[default]
    Descend,
    /// Stop, so the user finds out immediately. For someone who wants a hard
    /// stop and will set it deliberately.
    Refuse,
}

/// A cap on one backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendLimit {
    /// Backend id this applies to.
    pub id: String,
    /// Dollars per day. `0` means no cap.
    pub daily_spend_usd: f64,
}

/// Spend caps.
///
/// Money only, and metered money at that: a subscription is already paid for,
/// and capping it would cap capacity the user bought (see
/// `Summary::cost_by_backend`). Prepaid credits are excluded for the same
/// reason — `BackendKind::is_metered` draws the line.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// Dollars per day across every metered backend. `None` or `0` is no cap.
    pub daily_spend_usd: Option<f64>,
    /// What happens when a cap is reached.
    pub on_breach: BreachAction,
    /// Per-backend caps, which apply on top of any global one.
    pub backends: Vec<BackendLimit>,
}

impl LimitsConfig {
    /// Whether any cap is actually set.
    ///
    /// A configured `[limits]` block with every value zero is not a cap, and
    /// must not switch on the machinery — including the startup check that
    /// refuses to run a cap without the ledger behind it.
    #[must_use]
    pub fn any_cap(&self) -> bool {
        self.daily_spend_usd.is_some_and(|cap| cap > 0.0)
            || self.backends.iter().any(|b| b.daily_spend_usd > 0.0)
    }

    /// The cap for one backend: its own, or the global one, whichever binds
    /// first. `None` when neither is set.
    #[must_use]
    pub fn cap_for(&self, id: &str) -> Option<f64> {
        let specific = self
            .backends
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.daily_spend_usd)
            .filter(|cap| *cap > 0.0);
        let global = self.daily_spend_usd.filter(|cap| *cap > 0.0);
        match (specific, global) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (only, None) | (None, only) => only,
        }
    }
}

impl Config {
    /// Load from `paths.config_file()`, returning defaults when absent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ConfigRead`] or [`Error::ConfigParse`] when the file
    /// exists but cannot be used. A missing file is not an error — a fresh
    /// install must start.
    pub fn load(paths: &PathsConfig) -> Result<Self> {
        Self::load_from(&paths.config_file())
    }

    /// Load from an explicit path.
    ///
    /// # Errors
    ///
    /// As [`Config::load`].
    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let config: Self = toml::from_str(&text).map_err(|detail| Error::ConfigParse {
                    path: path.to_path_buf(),
                    detail,
                })?;
                // Only for a file that exists and parsed. A missing config must
                // still start a fresh install, which is the commonest case of
                // all.
                config.validate(path)?;
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(Error::ConfigRead {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Check what `deny_unknown_fields` cannot.
    ///
    /// Serde catches a misspelled *key*, which makes the file feel checked —
    /// so a value that is accepted reads as a value that works. These are the
    /// ways an entry can be well-formed and still describe a backend that
    /// cannot exist, and every one of them is better said now than discovered
    /// as a backend that silently never appears.
    ///
    /// # Errors
    ///
    /// [`Error::ConfigInvalid`], naming the entry at fault.
    pub fn validate(&self, path: &Path) -> Result<()> {
        let invalid = |id: &str, detail: String| Error::ConfigInvalid {
            path: path.to_path_buf(),
            id: id.to_string(),
            detail,
        };

        let mut seen: Vec<&str> = Vec::new();
        for backend in &self.backends {
            // Two entries with one id means `BackendRegistry::get` answers with
            // whichever was pushed first, and the other silently does nothing.
            if seen.contains(&backend.id.as_str()) {
                return Err(invalid(
                    &backend.id,
                    "declared twice. Ids must be unique: the router looks a \
                     backend up by id, so a duplicate is silently ignored."
                        .to_string(),
                ));
            }
            seen.push(&backend.id);

            let Some(kind) = BackendImpl::parse(&backend.kind) else {
                return Err(invalid(
                    &backend.id,
                    format!(
                        "`kind = \"{}\"` is not a backend IronWire can build. \
                         Valid kinds: {}.",
                        backend.kind,
                        BackendImpl::ALL.join(", ")
                    ),
                ));
            };

            // The id travels into ledger rows, event payloads and
            // `X-IronWire-Route`, so it is not free-form text.
            if !backend
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                || backend.id.is_empty()
            {
                return Err(invalid(
                    &backend.id,
                    "ids may contain only letters, digits, `-` and `_`: this one \
                     ends up in ledger rows, event payloads and the \
                     `X-IronWire-Route` header."
                        .to_string(),
                ));
            }

            // Neither kind has a host to fall back on: there is no canonical
            // address for "some OpenAI-compatible server", and building one
            // against nothing yields a backend that fails every request.
            if matches!(kind, BackendImpl::OpenAiCompatible | BackendImpl::Local) {
                if backend.base_url.is_none() {
                    return Err(invalid(
                        &backend.id,
                        format!(
                            "`kind = \"{}\"` needs a `base_url`; there is no \
                             default host to assume. For a local server it must \
                             include the OpenAI-compatible prefix, usually \
                             `/v1` — Ollama's native `/api/*` is a different \
                             protocol and is not supported.",
                            backend.kind
                        ),
                    ));
                }
                // A key is required for a hosted endpoint and optional for a
                // local one: most local servers take no auth at all, and
                // demanding a variable that holds nothing would be ceremony.
                if kind == BackendImpl::OpenAiCompatible && backend.api_key_env.is_none() {
                    return Err(invalid(
                        &backend.id,
                        "`kind = \"openai-compatible\"` needs an `api_key_env` \
                         naming the environment variable that holds its key. \
                         IronWire keeps no secrets in config.toml."
                            .to_string(),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// The backend implementations a `[[backends]]` entry can name.
///
/// Distinct from [`crate::protocol::BackendKind`], which is the *capacity*
/// class a backend draws on — subscription, key, credits, local. Several impls
/// here map to one capacity class and vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendImpl {
    /// Claude Code's stored subscription credential.
    ClaudeSubscription,
    /// The metered Anthropic API.
    AnthropicApi,
    /// Codex's stored ChatGPT credential.
    CodexSubscription,
    /// The metered OpenAI API.
    OpenAiApi,
    /// NEAR AI credits.
    NearAi,
    /// Any other endpoint speaking OpenAI Chat Completions.
    OpenAiCompatible,
    /// A model server on this machine or LAN. Free at the margin, and usually
    /// unauthenticated, which is what separates it from `openai-compatible`.
    Local,
}

impl BackendImpl {
    /// Every accepted spelling, for the error message that lists them.
    pub const ALL: &'static [&'static str] = &[
        "claude-subscription",
        "anthropic-api",
        "codex-subscription",
        "openai-api",
        "nearai",
        "openai-compatible",
        "local",
    ];

    /// Parse the `kind` field. `None` for anything not in [`Self::ALL`].
    #[must_use]
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "claude-subscription" => Some(Self::ClaudeSubscription),
            "anthropic-api" => Some(Self::AnthropicApi),
            "codex-subscription" => Some(Self::CodexSubscription),
            "openai-api" => Some(Self::OpenAiApi),
            "nearai" => Some(Self::NearAi),
            "openai-compatible" => Some(Self::OpenAiCompatible),
            "local" => Some(Self::Local),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(toml: &str) -> Result<Config> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml).expect("write");
        Config::load_from(&path)
    }

    /// The error has to name the entry. "invalid config" against a file with
    /// five backends in it is a scavenger hunt.
    fn rejection(toml: &str) -> String {
        match load(toml) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("accepted a config that cannot describe a backend:\n{toml}"),
        }
    }

    #[test]
    fn an_unknown_kind_is_refused_with_the_valid_ones_listed() {
        let message = rejection(
            r#"
            [[backends]]
            id = "typo"
            kind = "anthropic"
            "#,
        );
        assert!(message.contains("typo"), "{message}");
        assert!(message.contains("anthropic-api"), "{message}");
    }

    /// `BackendRegistry::get` answers with the first match, so the second entry
    /// would do nothing at all while looking like configuration.
    #[test]
    fn a_duplicate_id_is_refused() {
        let message = rejection(
            r#"
            [[backends]]
            id = "nearai"
            kind = "nearai"

            [[backends]]
            id = "nearai"
            kind = "nearai"
            "#,
        );
        assert!(message.contains("nearai"), "{message}");
    }

    #[test]
    fn an_openai_compatible_backend_needs_somewhere_to_send_requests() {
        let message = rejection(
            r#"
            [[backends]]
            id = "local"
            kind = "openai-compatible"
            api_key_env = "LOCAL_KEY"
            "#,
        );
        assert!(message.contains("base_url"), "{message}");
    }

    #[test]
    fn an_openai_compatible_backend_needs_a_named_key_variable() {
        let message = rejection(
            r#"
            [[backends]]
            id = "local"
            kind = "openai-compatible"
            base_url = "http://127.0.0.1:11434/v1"
            "#,
        );
        assert!(message.contains("api_key_env"), "{message}");
    }

    #[test]
    fn a_well_formed_backend_entry_is_accepted() {
        let config = load(
            r#"
            [[backends]]
            id = "local"
            kind = "openai-compatible"
            base_url = "http://127.0.0.1:11434/v1"
            api_key_env = "LOCAL_KEY"
            models = ["qwen3-coder"]

            [[backends]]
            id = "anthropic-key"
            kind = "anthropic-api"
            enabled = false
            "#,
        )
        .expect("valid");
        assert_eq!(config.backends.len(), 2);
        assert!(!config.backends[1].enabled);
    }

    /// Validation runs on a file that exists. A fresh install has none, and
    /// must still start.
    #[test]
    fn a_config_with_no_backends_block_is_valid() {
        assert!(load("[server]\nport = 8463\n").is_ok());
    }

    /// The rule the whole local-backend feature rests on. `from_model_hint`
    /// resolves an unrecognised slug to `Frontier`, and local capacity sorts
    /// cheapest, so a bare name on a local backend must not inherit that.
    #[test]
    fn a_bare_slug_on_local_capacity_is_fast_not_frontier() {
        let entry = ModelEntry::Name("qwen3-coder:30b".to_string());
        assert_eq!(entry.tier_on(BackendKind::Local), ModelTier::Fast);
        // The same slug on hosted capacity keeps the cautious default.
        assert_eq!(entry.tier_on(BackendKind::Credits), ModelTier::Frontier);
    }

    /// The user opts a local model up the ladder; IronWire never does.
    #[test]
    fn a_declared_tier_wins_everywhere() {
        let entry = ModelEntry::Tiered {
            name: "llama3.3:70b".to_string(),
            tier: ModelTier::Balanced,
        };
        assert_eq!(entry.tier_on(BackendKind::Local), ModelTier::Balanced);
        assert_eq!(entry.tier_on(BackendKind::ApiKey), ModelTier::Balanced);
    }

    #[test]
    fn a_models_list_accepts_both_spellings() {
        let config = load(
            r#"
            [[backends]]
            id = "ollama"
            kind = "local"
            base_url = "http://127.0.0.1:11434/v1"
            models = ["qwen3-coder:30b", { name = "llama3.3:70b", tier = "balanced" }]
            "#,
        )
        .expect("valid");
        let models = config.backends[0].models.as_ref().expect("declared");
        assert_eq!(models[0].name(), "qwen3-coder:30b");
        assert_eq!(models[1].tier_on(BackendKind::Local), ModelTier::Balanced);
    }

    /// Local servers usually take no auth, so requiring a key variable would be
    /// ceremony — but a host is still required, and the message says why.
    #[test]
    fn a_local_backend_needs_a_base_url_but_not_a_key() {
        assert!(
            load(
                r#"
                [[backends]]
                id = "ollama"
                kind = "local"
                base_url = "http://127.0.0.1:11434/v1"
                "#,
            )
            .is_ok()
        );
        let message = rejection(
            r#"
            [[backends]]
            id = "ollama"
            kind = "local"
            "#,
        );
        assert!(message.contains("base_url"), "{message}");
        assert!(
            message.contains("/v1"),
            "the fix must be spelled out: {message}"
        );
    }

    /// The id reaches ledger rows, event payloads and `X-IronWire-Route`.
    #[test]
    fn an_id_is_restricted_to_a_conservative_character_set() {
        for bad in ["has space", "quote\"d", "semi;colon", ""] {
            let toml = format!("[[backends]]\nid = \"{bad}\"\nkind = \"nearai\"\n");
            assert!(
                Config::load_from(&{
                    let dir = tempfile::tempdir().expect("tempdir");
                    let path = dir.path().join("config.toml");
                    std::fs::write(&path, &toml).expect("write");
                    std::mem::forget(dir);
                    path
                })
                .is_err(),
                "accepted id {bad:?}"
            );
        }
    }

    /// A `[limits]` block with nothing in it is not a cap, and must not switch
    /// on the machinery — including the startup refusal that a real cap
    /// triggers when the ledger is off.
    #[test]
    fn an_empty_limits_block_is_not_a_cap() {
        assert!(!LimitsConfig::default().any_cap());
        assert!(
            !LimitsConfig {
                daily_spend_usd: Some(0.0),
                ..LimitsConfig::default()
            }
            .any_cap()
        );
        assert!(
            LimitsConfig {
                daily_spend_usd: Some(10.0),
                ..LimitsConfig::default()
            }
            .any_cap()
        );
    }

    /// A per-backend cap and a global cap both apply; whichever binds first
    /// wins, because the user meant both.
    #[test]
    fn the_tighter_of_two_caps_binds() {
        let limits = LimitsConfig {
            daily_spend_usd: Some(10.0),
            on_breach: BreachAction::Descend,
            backends: vec![BackendLimit {
                id: "anthropic-key".into(),
                daily_spend_usd: 5.0,
            }],
        };
        assert_eq!(limits.cap_for("anthropic-key"), Some(5.0));
        assert_eq!(
            limits.cap_for("openai-key"),
            Some(10.0),
            "a backend with no cap of its own still counts against the global one"
        );
        assert_eq!(
            LimitsConfig::default().cap_for("anthropic-key"),
            None,
            "no cap configured is not a cap of zero"
        );
    }

    #[test]
    fn descend_is_the_default_breach_action() {
        // The product's promise is that the agent does not die. A cap that
        // killed the session by default would invert it.
        assert_eq!(LimitsConfig::default().on_breach, BreachAction::Descend);
    }

    #[test]
    fn every_advertised_kind_parses() {
        for kind in BackendImpl::ALL {
            assert!(
                BackendImpl::parse(kind).is_some(),
                "`{kind}` is advertised but not accepted"
            );
        }
        assert!(BackendImpl::parse("not-a-kind").is_none());
    }

    #[test]
    fn a_missing_config_yields_defaults_rather_than_failing() {
        let cfg = Config::load_from(Path::new("/nonexistent/ironwire/config.toml"))
            .expect("a fresh install must start");
        assert_eq!(cfg.server.port, DEFAULT_PORT);
        assert!(cfg.capture.enabled);
        assert!(!cfg.capture.bodies, "bodies contain user source code");
    }

    #[test]
    fn config_round_trips() {
        let cfg = Config {
            server: ServerConfig {
                port: 9000,
                upstream_timeout_secs: 60,
            },
            capture: CaptureConfig {
                enabled: true,
                bodies: true,
                retain_days: 30,
            },
            usage: UsageConfig {
                enabled: true,
                session_hours: 5,
                history_hours: 192,
                plan: Some("max5".into()),
            },
            updates: UpdateConfig { check: false },
            resilience: ResilienceConfig::default(),
            privacy: PrivacyConfig::default(),
            backends: vec![BackendConfig {
                id: "claude-sub".into(),
                kind: "claude-subscription".into(),
                enabled: true,
                base_url: None,
                api_key_env: None,
                models: None,
            }],
            limits: LimitsConfig {
                daily_spend_usd: Some(10.0),
                on_breach: BreachAction::Refuse,
                backends: vec![BackendLimit {
                    id: "anthropic-key".into(),
                    daily_spend_usd: 5.0,
                }],
            },
        };
        let text = toml::to_string(&cfg).expect("serializes");
        let back: Config = toml::from_str(&text).expect("deserializes");
        assert_eq!(cfg, back);
    }

    #[test]
    fn unknown_config_keys_are_rejected_not_ignored() {
        // A typo in a routing knob must not silently do nothing.
        let err = toml::from_str::<Config>("[server]\nprot = 1234\n");
        assert!(err.is_err());
    }

    #[test]
    fn paths_hang_off_one_root() {
        let paths = PathsConfig::rooted_at("/tmp/iw-test");
        assert_eq!(paths.config_file(), Path::new("/tmp/iw-test/config.toml"));
        assert_eq!(paths.lock_file(), Path::new("/tmp/iw-test/daemon.lock"));
        assert_eq!(
            paths.control_token_file(),
            Path::new("/tmp/iw-test/control.token")
        );
    }
}

#[cfg(test)]
mod resilience_tests {
    use super::ResilienceConfig;

    #[test]
    fn a_compaction_turn_gets_the_longer_patience() {
        let config = ResilienceConfig::default();
        assert!(config.stall_timeout_for(true) > config.stall_timeout_for(false));
    }

    #[test]
    fn an_ordinary_turn_is_unaffected() {
        let config = ResilienceConfig::default();
        assert_eq!(config.stall_timeout_for(false), config.stall_timeout_secs);
    }

    #[test]
    fn a_misconfigured_compaction_timeout_never_makes_compaction_more_fragile() {
        // Someone lowering `compaction_stall_timeout_secs` below the ordinary
        // one would get the exact opposite of what the setting is for, and the
        // symptom — abandoned compaction turns in long sessions — is very hard
        // to trace back to a config value.
        let config = ResilienceConfig {
            stall_timeout_secs: 180,
            compaction_stall_timeout_secs: 10,
            ..ResilienceConfig::default()
        };
        assert_eq!(config.stall_timeout_for(true), 180);
    }

    #[test]
    fn a_local_turn_can_be_given_more_patience() {
        let config = ResilienceConfig {
            stall_timeout_secs: 180,
            local_stall_timeout_secs: 900,
            ..ResilienceConfig::default()
        };
        assert_eq!(config.stall_timeout_for_backend(false, true), 900);
        assert_eq!(
            config.stall_timeout_for_backend(false, false),
            180,
            "a hosted turn keeps the ordinary timeout"
        );
    }

    /// The same trap as the compaction floor: a local model is the slowest
    /// thing IronWire routes to, so a value below the ordinary timeout would
    /// make the backend that needs the most patience the least patiently
    /// treated.
    #[test]
    fn a_misconfigured_local_timeout_never_makes_a_local_turn_more_fragile() {
        let config = ResilienceConfig {
            stall_timeout_secs: 180,
            local_stall_timeout_secs: 10,
            ..ResilienceConfig::default()
        };
        assert_eq!(config.stall_timeout_for_backend(false, true), 180);
    }

    /// A compaction turn on local capacity gets the longer of the two, not
    /// whichever rule was consulted last.
    #[test]
    fn a_local_compaction_turn_gets_the_longest_patience() {
        let config = ResilienceConfig {
            stall_timeout_secs: 180,
            compaction_stall_timeout_secs: 600,
            local_stall_timeout_secs: 300,
            ..ResilienceConfig::default()
        };
        assert_eq!(config.stall_timeout_for_backend(true, true), 600);
    }
}
