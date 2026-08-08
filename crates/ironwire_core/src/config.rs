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
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bodies: false,
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
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self {
            keepalive_secs: 15,
            stall_timeout_secs: 180,
            max_reconnects: 2,
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
            Ok(text) => toml::from_str(&text).map_err(|source| Error::ConfigParse {
                path: path.to_path_buf(),
                source,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(Error::ConfigRead {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            },
            updates: UpdateConfig { check: false },
            resilience: ResilienceConfig::default(),
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
