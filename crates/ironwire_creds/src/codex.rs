//! Codex CLI credentials — delegated to `ironclaw_llm`.
//!
//! This used to be a hand-rolled reader for `~/.codex/auth.json`. It is not any
//! more: `ironclaw_llm::auth` already does exactly this, in production, with
//! the auth-mode quirks and the ChatGPT-vs-API-key base-URL split handled. A
//! second implementation of credential discovery is a second thing to keep
//! correct as Codex changes its file format, and the failure mode is a user
//! silently losing their subscription.
//!
//! What stays here is IronWire's own shape: a [`Bearer`] bound to the one host
//! that issued it (`docs/TRUST.md` I2), which is a proxy concern `ironclaw_llm`
//! has no reason to model.

use std::path::PathBuf;

use ironclaw_llm::auth::{self, CredentialSource};
use secrecy::{ExposeSecret, SecretString};

use crate::{Bearer, CredentialError, Result};

/// Host serving the ChatGPT subscription backend.
pub const CHATGPT_HOST: &str = "chatgpt.com";
/// Base URL for the ChatGPT subscription backend.
pub const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Host serving the metered OpenAI API.
pub const OPENAI_HOST: &str = "api.openai.com";
/// Base URL for the metered OpenAI API.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

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
    /// Base URL the issuing tool recorded for this credential.
    pub base_url: String,
    /// Where this came from, for `ironwire doctor`.
    pub source: String,
    /// Whether a refresh token is available. Display only — IronWire does not
    /// drive a refresh itself (see below).
    pub refreshable: bool,
}

impl std::fmt::Debug for CodexCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexCredentials")
            .field("token", &"<redacted>")
            .field("mode", &self.mode)
            .field("base_url", &self.base_url)
            .field("source", &self.source)
            .finish()
    }
}

impl CodexCredentials {
    /// Base URL this credential is valid against.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The ChatGPT account this credential belongs to, read from its own JWT.
    ///
    /// The ChatGPT backend requires a `chatgpt-account-id` header, and Codex
    /// only sends one on its own built-in provider path — a request routed
    /// through a custom provider arrives without it. Deriving it from the token
    /// we are about to present is not identity forgery (`docs/TRUST.md` §3): it
    /// names the account that already owns the credential, and a value read from
    /// the token cannot address any other account.
    ///
    /// `None` for an API key, which is not a JWT and needs no account header.
    #[must_use]
    pub fn account_id(&self) -> Option<String> {
        if self.mode != CodexMode::ChatGpt {
            return None;
        }
        account_id_from_jwt(self.token.expose_secret())
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

    /// Read the credential Codex stored.
    ///
    /// # Errors
    ///
    /// [`CredentialError::NotFound`] when Codex is not logged in here.
    pub fn discover() -> Result<Self> {
        Self::from_path(None)
    }

    /// Read from an explicit `auth.json`. Used by the conformance tests.
    ///
    /// # Errors
    ///
    /// [`CredentialError::NotFound`] when the file is absent or unusable.
    pub fn from_path(path: Option<PathBuf>) -> Result<Self> {
        let path = path.unwrap_or_else(auth_path);
        let persisted = auth::load_persisted_credentials(CredentialSource::CodexCli, Some(&path))
            .ok_or_else(|| CredentialError::NotFound {
            product: "Codex",
            locations: path.display().to_string(),
        })?;

        Ok(Self {
            token: persisted.token,
            // `is_subscription` is ironclaw's own reading of `auth_mode` plus
            // which credential is actually present — the distinction we need.
            mode: if persisted.is_subscription {
                CodexMode::ChatGpt
            } else {
                CodexMode::ApiKey
            },
            base_url: persisted.base_url,
            source: persisted.source_path.unwrap_or(path).display().to_string(),
            refreshable: persisted.refresh_token.is_some(),
        })
    }
}

/// Path Codex writes its credentials to.
///
/// `CODEX_HOME` is honoured here because Codex honours it — `ironclaw`'s default
/// path does not, and inheriting that would mean a user who moved their Codex
/// home is told they are not logged in when they are. The *parsing* still
/// belongs to `ironclaw`; only the location is ours.
#[must_use]
pub fn auth_path() -> PathBuf {
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home).join("auth.json");
    }
    auth::default_credentials_path(CredentialSource::CodexCli)
}

/// Read `chatgpt_account_id` out of a Codex access token.
///
/// The claim lives under the namespaced `https://api.openai.com/auth` object.
/// The signature is neither checked nor needed here — we are reading a claim out
/// of a token we already hold and are about to send to the issuer, which will
/// verify it. A token we cannot parse yields `None` and the header is simply
/// omitted, which is the same position we would be in without this function.
fn account_id_from_jwt(token: &str) -> Option<String> {
    use base64::Engine as _;

    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_auth(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(contents.as_bytes()).expect("write");
        (dir, path)
    }

    #[test]
    fn a_chatgpt_login_is_recognised_as_subscription_capacity() {
        let (_dir, path) = write_auth(
            r#"{
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": "eyJhbGciOi",
                    "access_token": "eyJACCESS",
                    "refresh_token": "rt.1.EXAMPLE",
                    "account_id": "36afe797-0000"
                },
                "last_refresh": "2026-07-30T04:20:00Z"
            }"#,
        );
        let creds = CodexCredentials::from_path(Some(path)).expect("reads");
        assert_eq!(creds.mode, CodexMode::ChatGpt);
        assert_eq!(creds.bearer().issuer_host, CHATGPT_HOST);
        assert!(creds.base_url().contains("chatgpt.com"));
        assert!(creds.refreshable);
    }

    #[test]
    fn an_api_key_login_routes_to_the_metered_host() {
        let (_dir, path) =
            write_auth(r#"{"auth_mode": "apiKey", "OPENAI_API_KEY": "sk-proj-EXAMPLE"}"#);
        let creds = CodexCredentials::from_path(Some(path)).expect("reads");
        assert_eq!(creds.mode, CodexMode::ApiKey);
        assert_eq!(creds.bearer().issuer_host, OPENAI_HOST);
        assert!(creds.base_url().contains("api.openai.com"));
    }

    #[test]
    fn a_missing_login_is_reported_as_not_found() {
        let err = CodexCredentials::from_path(Some(PathBuf::from("/nonexistent/auth.json")))
            .expect_err("must not invent a credential");
        assert!(matches!(err, CredentialError::NotFound { .. }));
    }

    /// A Codex-shaped access token. Header and signature are inert — only the
    /// payload is ever read.
    fn jwt_with(payload: &serde_json::Value) -> String {
        use base64::Engine as _;
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.{}",
            engine.encode(br#"{"alg":"RS256","typ":"JWT"}"#),
            engine.encode(payload.to_string()),
            engine.encode(b"not-a-real-signature"),
        )
    }

    #[test]
    fn the_account_id_comes_out_of_the_token_we_are_about_to_present() {
        // TRUST.md §3: the header names the account that owns the credential.
        // There is no path here that could address a different account.
        let token = jwt_with(&serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "36afe797-0000"},
            "exp": 1_800_000_000u64,
        }));
        let (_dir, path) = write_auth(&format!(
            r#"{{"auth_mode": "chatgpt", "tokens": {{"access_token": "{token}", "refresh_token": "rt"}}}}"#
        ));
        let creds = CodexCredentials::from_path(Some(path)).expect("reads");
        assert_eq!(creds.account_id().as_deref(), Some("36afe797-0000"));
    }

    #[test]
    fn an_api_key_has_no_account_id_to_report() {
        let (_dir, path) =
            write_auth(r#"{"auth_mode": "apiKey", "OPENAI_API_KEY": "sk-proj-EXAMPLE"}"#);
        let creds = CodexCredentials::from_path(Some(path)).expect("reads");
        assert_eq!(creds.account_id(), None);
    }

    #[test]
    fn a_token_we_cannot_parse_omits_the_header_rather_than_guessing() {
        assert_eq!(account_id_from_jwt("not-a-jwt"), None);
        assert_eq!(account_id_from_jwt("a.!!!not-base64!!!.c"), None);
        // Well-formed, but the claim is simply not there.
        assert_eq!(
            account_id_from_jwt(&jwt_with(&serde_json::json!({"sub": "user_1"}))),
            None
        );
    }

    #[test]
    fn debug_never_leaks_the_token() {
        let (_dir, path) =
            write_auth(r#"{"auth_mode": "apiKey", "OPENAI_API_KEY": "sk-proj-SECRETVALUE"}"#);
        let creds = CodexCredentials::from_path(Some(path)).expect("reads");
        assert!(!format!("{creds:?}").contains("SECRETVALUE"));
        assert!(!format!("{:?}", creds.bearer()).contains("SECRETVALUE"));
    }
}
