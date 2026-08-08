//! Codex CLI credentials (ChatGPT subscription or OpenAI API key).
//!
//! Codex writes `~/.codex/auth.json` with an `auth_mode` of either `chatgpt`
//! (OAuth against `chatgpt.com/backend-api/codex`) or `apiKey` (a plain
//! `OPENAI_API_KEY` against `api.openai.com`). The two modes route to different
//! hosts, so the mode determines the bearer's `issuer_host`.
//!
//! `ironclaw_llm::auth::load_persisted_credentials(CredentialSource::CodexCli)`
//! already does this and should be delegated to once the `ironclaw-auth`
//! feature lands (`docs/DESIGN.md` §7). This reader exists so M2 is not blocked
//! on that dependency, and so the default build stays small.

use std::path::PathBuf;

use secrecy::SecretString;
use serde::Deserialize;

use crate::{Bearer, CredentialError, Result};

/// Host serving the ChatGPT subscription backend.
pub const CHATGPT_HOST: &str = "chatgpt.com";
/// Base URL for the ChatGPT subscription backend.
pub const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Host serving the metered OpenAI API.
pub const OPENAI_HOST: &str = "api.openai.com";
/// Base URL for the metered OpenAI API.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Debug, Deserialize)]
struct AuthFile {
    #[serde(default)]
    auth_mode: Option<String>,
    #[serde(rename = "OPENAI_API_KEY", default)]
    api_key: Option<String>,
    #[serde(default)]
    tokens: Option<Tokens>,
}

#[derive(Debug, Deserialize)]
struct Tokens {
    access_token: String,
    #[serde(default)]
    account_id: Option<String>,
}

/// Which capacity pool a Codex credential draws on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexMode {
    /// ChatGPT subscription: marginal cost zero, capacity scarce, consent
    /// required (`docs/TRUST.md` §2).
    ChatGpt,
    /// Metered OpenAI API key.
    ApiKey,
}

/// A discovered Codex credential.
#[derive(Clone)]
pub struct CodexCredentials {
    token: SecretString,
    /// Which pool this draws on.
    pub mode: CodexMode,
    /// ChatGPT account id, when present. Display only.
    pub account_id: Option<String>,
    /// Where this came from.
    pub source: String,
}

impl std::fmt::Debug for CodexCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexCredentials")
            .field("token", &"<redacted>")
            .field("mode", &self.mode)
            .field("account_id", &self.account_id)
            .field("source", &self.source)
            .finish()
    }
}

impl CodexCredentials {
    /// Base URL this credential is valid against.
    #[must_use]
    pub fn base_url(&self) -> &'static str {
        match self.mode {
            CodexMode::ChatGpt => CHATGPT_BASE_URL,
            CodexMode::ApiKey => OPENAI_BASE_URL,
        }
    }

    /// Bearer bound to the host matching this credential's mode.
    #[must_use]
    pub fn bearer(&self) -> Bearer {
        Bearer {
            token: self.token.clone(),
            issuer_host: match self.mode {
                CodexMode::ChatGpt => CHATGPT_HOST,
                CodexMode::ApiKey => OPENAI_HOST,
            },
        }
    }

    /// Read `~/.codex/auth.json`.
    ///
    /// # Errors
    ///
    /// [`CredentialError::NotFound`] when Codex is not logged in here.
    pub fn discover() -> Result<Self> {
        let path = auth_path();
        match std::fs::read_to_string(&path) {
            Ok(json) => Self::parse(&json, path.display().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CredentialError::NotFound {
                product: "Codex",
                locations: path.display().to_string(),
            }),
            Err(source) => Err(CredentialError::Io {
                product: "Codex",
                source,
            }),
        }
    }

    /// Parse an `auth.json` document.
    ///
    /// # Errors
    ///
    /// [`CredentialError::Malformed`] when neither auth mode is usable.
    pub fn parse(json: &str, source: String) -> Result<Self> {
        let parsed: AuthFile =
            serde_json::from_str(json).map_err(|_| CredentialError::Malformed {
                product: "Codex",
                path: source.clone(),
            })?;

        let malformed = || CredentialError::Malformed {
            product: "Codex",
            path: source.clone(),
        };

        // `auth_mode` is advisory: what actually decides is which credential is
        // present. A file claiming `chatgpt` with no tokens is an apiKey login
        // in practice, and vice versa.
        let prefers_chatgpt = parsed.auth_mode.as_deref() != Some("apiKey");
        let tokens = parsed.tokens;
        let api_key = parsed.api_key.filter(|k| !k.is_empty());

        match (prefers_chatgpt, tokens, api_key) {
            (true, Some(tokens), _) => Ok(Self {
                token: SecretString::from(tokens.access_token),
                mode: CodexMode::ChatGpt,
                account_id: tokens.account_id,
                source,
            }),
            (_, _, Some(key)) => Ok(Self {
                token: SecretString::from(key),
                mode: CodexMode::ApiKey,
                account_id: None,
                source,
            }),
            (false, Some(tokens), None) => Ok(Self {
                token: SecretString::from(tokens.access_token),
                mode: CodexMode::ChatGpt,
                account_id: tokens.account_id,
                source,
            }),
            _ => Err(malformed()),
        }
    }
}

/// Path Codex writes its credentials to.
#[must_use]
pub fn auth_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("CODEX_HOME") {
        return PathBuf::from(dir).join("auth.json");
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".codex")
        .join("auth.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHATGPT_SAMPLE: &str = r#"{
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "eyJhbGciOi",
            "access_token": "eyJACCESS",
            "refresh_token": "rt.1.EXAMPLE",
            "account_id": "36afe797-0000"
        },
        "last_refresh": "2026-07-30T04:20:00Z"
    }"#;

    #[test]
    fn parses_the_shape_codex_actually_writes() {
        let creds = CodexCredentials::parse(CHATGPT_SAMPLE, "test".into()).expect("parses");
        assert_eq!(creds.mode, CodexMode::ChatGpt);
        assert_eq!(creds.account_id.as_deref(), Some("36afe797-0000"));
        assert_eq!(creds.base_url(), CHATGPT_BASE_URL);
        assert_eq!(creds.bearer().issuer_host, CHATGPT_HOST);
    }

    #[test]
    fn api_key_mode_routes_to_the_metered_host() {
        let json = r#"{"auth_mode": "apiKey", "OPENAI_API_KEY": "sk-proj-EXAMPLE"}"#;
        let creds = CodexCredentials::parse(json, "test".into()).expect("parses");
        assert_eq!(creds.mode, CodexMode::ApiKey);
        assert_eq!(creds.base_url(), OPENAI_BASE_URL);
        assert_eq!(creds.bearer().issuer_host, OPENAI_HOST);
    }

    #[test]
    fn the_credential_present_wins_over_a_stale_mode_label() {
        // A file that says `chatgpt` but holds only a key is an apiKey login.
        let json = r#"{"auth_mode": "chatgpt", "OPENAI_API_KEY": "sk-proj-EXAMPLE"}"#;
        let creds = CodexCredentials::parse(json, "test".into()).expect("parses");
        assert_eq!(creds.mode, CodexMode::ApiKey);
    }

    #[test]
    fn an_empty_key_is_not_a_credential() {
        let json = r#"{"auth_mode": "apiKey", "OPENAI_API_KEY": ""}"#;
        assert!(CodexCredentials::parse(json, "test".into()).is_err());
    }

    #[test]
    fn debug_never_leaks_the_token() {
        let creds = CodexCredentials::parse(CHATGPT_SAMPLE, "test".into()).expect("parses");
        assert!(!format!("{creds:?}").contains("eyJACCESS"));
    }
}
