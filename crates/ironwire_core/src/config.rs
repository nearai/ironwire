//! On-disk configuration and the runtime home directory.
//!
//! Everything IronWire persists lives under `$IRONWIRE_HOME` (default
//! `~/.ironwire`, mode 0700). See `docs/PACKAGING.md` for the layout.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::DEFAULT_PORT;
use crate::error::{Error, Result};

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
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self {
            keepalive_secs: 15,
            stall_timeout_secs: 180,
            max_reconnects: 2,
            compaction_stall_timeout_secs: 600,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
}

fn default_true() -> bool {
    true
}

/// Top-level configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Listener settings.
    pub server: ServerConfig,
    /// Trace capture settings.
    pub capture: CaptureConfig,
    /// Update-check settings.
    pub updates: UpdateConfig,
    /// Stream-resilience settings.
    pub resilience: ResilienceConfig,
    /// Optional privacy filter. Off by default (`docs/PRIVACY.md`).
    pub privacy: PrivacyConfig,
    /// Configured backends, in preference order for ties.
    pub backends: Vec<BackendConfig>,
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

            let Some(kind) = BackendKind::parse(&backend.kind) else {
                return Err(invalid(
                    &backend.id,
                    format!(
                        "`kind = \"{}\"` is not a backend IronWire can build. \
                         Valid kinds: {}.",
                        backend.kind,
                        BackendKind::ALL.join(", ")
                    ),
                ));
            };

            // The one kind with no defaults to fall back on: there is no
            // canonical host for "some OpenAI-compatible server", and building
            // it against nothing yields a backend that fails every request.
            if kind == BackendKind::OpenAiCompatible {
                if backend.base_url.is_none() {
                    return Err(invalid(
                        &backend.id,
                        "`kind = \"openai-compatible\"` needs a `base_url`; there \
                         is no default host to assume."
                            .to_string(),
                    ));
                }
                if backend.api_key_env.is_none() {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
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
}

impl BackendKind {
    /// Every accepted spelling, for the error message that lists them.
    pub const ALL: &'static [&'static str] = &[
        "claude-subscription",
        "anthropic-api",
        "codex-subscription",
        "openai-api",
        "nearai",
        "openai-compatible",
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

    #[test]
    fn every_advertised_kind_parses() {
        for kind in BackendKind::ALL {
            assert!(
                BackendKind::parse(kind).is_some(),
                "`{kind}` is advertised but not accepted"
            );
        }
        assert!(BackendKind::parse("not-a-kind").is_none());
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
}
