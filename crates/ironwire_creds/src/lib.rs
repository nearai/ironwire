//! Credential discovery for IronWire backends.
//!
//! IronWire reads credentials the official clients already store on this
//! machine. It never mints them, never uploads them, and never attaches one to
//! a host other than its own issuer (`docs/TRUST.md` I2).
//!
//! Reuse note: `ironclaw_llm::auth` already exposes Codex CLI credentials via
//! `CredentialSource::CodexCli`, and this crate delegates to it behind the
//! `ironclaw-auth` feature. The Claude Code reader is currently a private
//! function inside `ironclaw_llm::anthropic_oauth`, so it is ported here; the
//! upstream fix is to add `CredentialSource::ClaudeCode` (`docs/DESIGN.md` §7).
#![warn(missing_docs)]

pub mod claude;
pub mod codex;
pub mod consent;

use secrecy::SecretString;

pub use claude::ClaudeCodeCredentials;
pub use codex::CodexCredentials;
pub use consent::{ConsentLedger, ConsentRecord};

/// Errors from reading or refreshing a credential.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// No credential was found in any known location.
    #[error("{product} is not logged in on this machine (looked in {locations})")]
    NotFound {
        /// Human-readable product name, e.g. "Claude Code".
        product: &'static str,
        /// Where we looked, joined for display.
        locations: String,
    },

    /// A credential file was found but could not be parsed.
    #[error("{product} credentials at {path} are not in a shape we recognise")]
    Malformed {
        /// Human-readable product name.
        product: &'static str,
        /// Path that failed.
        path: String,
    },

    /// The credential is present but expired and cannot be refreshed here.
    #[error("{product} credentials have expired; re-authenticate with `{command}`")]
    Expired {
        /// Human-readable product name.
        product: &'static str,
        /// What the user should run.
        command: &'static str,
    },

    /// I/O failure.
    #[error("reading {product} credentials: {source}")]
    Io {
        /// Human-readable product name.
        product: &'static str,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// A bearer credential plus the single host it may be sent to.
///
/// The `issuer_host` field is not decoration: it is the enforcement point for
/// `docs/TRUST.md` I2. Upstream clients assert it before attaching the token.
#[derive(Clone)]
pub struct Bearer {
    /// The token. Never logged, never serialized.
    pub token: SecretString,
    /// The only host this token may be sent to.
    pub issuer_host: &'static str,
}

impl std::fmt::Debug for Bearer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bearer")
            .field("token", &"<redacted>")
            .field("issuer_host", &self.issuer_host)
            .finish()
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, CredentialError>;
