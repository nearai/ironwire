//! Claude Code subscription credentials.
//!
//! Claude Code stores an OAuth token in the macOS Keychain under
//! `Claude Code-credentials`, and on other platforms in
//! `~/.claude/.credentials.json`. The token authenticates against
//! `api.anthropic.com` with `Authorization: Bearer` plus the
//! `anthropic-beta: oauth-2025-04-20` flag — `x-api-key` is rejected for it.
//!
//! Ported from `ironclaw_llm::anthropic_oauth`, which proves this path in
//! production but keeps the reader private.

use std::path::PathBuf;

use chrono::{DateTime, TimeZone, Utc};
use secrecy::SecretString;
use serde::Deserialize;

use crate::{Bearer, CredentialError, Result};

/// The only host a Claude subscription token may be sent to.
pub const ANTHROPIC_HOST: &str = "api.anthropic.com";

/// API version required alongside the OAuth beta flag. The newer dated
/// versions are not valid with it.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Beta flag that enables OAuth bearer auth. Without it the API answers 401
/// "OAuth authentication is currently not supported."
pub const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";

/// Access tokens Claude Code issues carry this prefix. Anything else in the
/// credential store is some other product's secret and is left alone.
const ACCESS_TOKEN_PREFIX: &str = "sk-ant-oat";

/// macOS Keychain service name.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

#[derive(Debug, Deserialize)]
struct CredentialFile {
    #[serde(rename = "claudeAiOauth")]
    oauth: OauthBlock,
}

#[derive(Debug, Deserialize)]
struct OauthBlock {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier")]
    rate_limit_tier: Option<String>,
}

/// A discovered Claude Code subscription credential.
#[derive(Clone)]
pub struct ClaudeCodeCredentials {
    access_token: SecretString,
    /// When the token expires, if the store said.
    pub expires_at: Option<DateTime<Utc>>,
    /// Plan name as the store reports it, e.g. `max`. Display only.
    pub subscription_type: Option<String>,
    /// Rate-limit tier as the store reports it. Display only.
    pub rate_limit_tier: Option<String>,
    /// Where this came from, for `ironwire doctor`.
    pub source: String,
}

impl std::fmt::Debug for ClaudeCodeCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCodeCredentials")
            .field("access_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("subscription_type", &self.subscription_type)
            .field("source", &self.source)
            .finish()
    }
}

impl ClaudeCodeCredentials {
    /// Bearer bound to `api.anthropic.com`.
    #[must_use]
    pub fn bearer(&self) -> Bearer {
        Bearer {
            token: self.access_token.clone(),
            issuer_host: ANTHROPIC_HOST,
        }
    }

    /// Whether the token is past its stated expiry.
    ///
    /// A token with no stated expiry is treated as live: Claude Code refreshes
    /// in the background, so the honest move is to try it and let a 401 drive
    /// a re-read rather than refusing pre-emptively.
    #[must_use]
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|exp| now >= exp)
    }

    /// Locate and read the credential.
    ///
    /// Tries the macOS Keychain first (where Claude Code puts it on macOS),
    /// then the file. Both are checked on every platform so a Keychain-less
    /// macOS environment still works.
    ///
    /// # Errors
    ///
    /// [`CredentialError::NotFound`] when Claude Code is not logged in here,
    /// or [`CredentialError::Malformed`] when the store holds something we do
    /// not recognise.
    pub fn discover() -> Result<Self> {
        let mut looked = Vec::new();

        if cfg!(target_os = "macos") {
            looked.push(format!("Keychain:{KEYCHAIN_SERVICE}"));
            if let Some(json) = read_keychain() {
                return Self::parse(&json, format!("Keychain:{KEYCHAIN_SERVICE}"));
            }
        }

        let path = credentials_path();
        looked.push(path.display().to_string());
        match std::fs::read_to_string(&path) {
            Ok(json) => Self::parse(&json, path.display().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CredentialError::NotFound {
                product: "Claude Code",
                locations: looked.join(", "),
            }),
            Err(source) => Err(CredentialError::Io {
                product: "Claude Code",
                source,
            }),
        }
    }

    /// Parse a credential document. Public for the conformance tests.
    ///
    /// # Errors
    ///
    /// [`CredentialError::Malformed`] when the document does not hold a
    /// recognisable Claude Code access token.
    pub fn parse(json: &str, source: String) -> Result<Self> {
        let parsed: CredentialFile =
            serde_json::from_str(json).map_err(|_| CredentialError::Malformed {
                product: "Claude Code",
                path: source.clone(),
            })?;

        // A store entry whose token is not Claude Code's is some other
        // product's secret. Refusing it is how we avoid ever sending a
        // credential to a host that did not issue it.
        if !parsed.oauth.access_token.starts_with(ACCESS_TOKEN_PREFIX) {
            return Err(CredentialError::Malformed {
                product: "Claude Code",
                path: source,
            });
        }

        Ok(Self {
            access_token: SecretString::from(parsed.oauth.access_token),
            expires_at: parsed.oauth.expires_at.and_then(millis_to_utc),
            subscription_type: parsed.oauth.subscription_type,
            rate_limit_tier: parsed.oauth.rate_limit_tier,
            source,
        })
    }
}

/// Path Claude Code writes credentials to on non-macOS platforms.
#[must_use]
pub fn credentials_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".claude")
        .join(".credentials.json")
}

fn read_keychain() -> Option<String> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
}

fn millis_to_utc(millis: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(millis).single()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "claudeAiOauth": {
            "accessToken": "sk-ant-oat01-EXAMPLE",
            "refreshToken": "sk-ant-ort01-EXAMPLE",
            "expiresAt": 1786165549135,
            "scopes": ["user:inference"],
            "subscriptionType": "max",
            "rateLimitTier": "default_claude_max"
        }
    }"#;

    #[test]
    fn parses_the_shape_claude_code_actually_writes() {
        let creds = ClaudeCodeCredentials::parse(SAMPLE, "test".into()).expect("parses");
        assert_eq!(creds.subscription_type.as_deref(), Some("max"));
        assert_eq!(creds.rate_limit_tier.as_deref(), Some("default_claude_max"));
        assert!(creds.expires_at.is_some());
    }

    #[test]
    fn a_token_is_bound_to_the_host_that_issued_it() {
        let creds = ClaudeCodeCredentials::parse(SAMPLE, "test".into()).expect("parses");
        assert_eq!(creds.bearer().issuer_host, ANTHROPIC_HOST);
    }

    #[test]
    fn a_foreign_token_in_the_store_is_refused() {
        // TRUST.md I2: we must never pick up someone else's secret and send it
        // to Anthropic just because it was in a file we read.
        let foreign = r#"{"claudeAiOauth": {"accessToken": "ghp_someothersecret"}}"#;
        let err = ClaudeCodeCredentials::parse(foreign, "test".into())
            .expect_err("must refuse a non-Claude token");
        assert!(matches!(err, CredentialError::Malformed { .. }));
    }

    #[test]
    fn garbage_is_malformed_not_a_panic() {
        assert!(ClaudeCodeCredentials::parse("not json", "test".into()).is_err());
        assert!(ClaudeCodeCredentials::parse("{}", "test".into()).is_err());
    }

    #[test]
    fn expiry_is_honoured_when_stated_and_ignored_when_absent() {
        let creds = ClaudeCodeCredentials::parse(SAMPLE, "test".into()).expect("parses");
        let expires = creds.expires_at.expect("sample states an expiry");
        assert!(creds.is_expired(expires));
        assert!(!creds.is_expired(expires - chrono::Duration::seconds(1)));

        let no_expiry = r#"{"claudeAiOauth": {"accessToken": "sk-ant-oat01-X"}}"#;
        let creds = ClaudeCodeCredentials::parse(no_expiry, "test".into()).expect("parses");
        assert!(
            !creds.is_expired(Utc::now()),
            "an unstated expiry must not pre-emptively disable the backend"
        );
    }

    #[test]
    fn debug_never_leaks_the_token() {
        let creds = ClaudeCodeCredentials::parse(SAMPLE, "test".into()).expect("parses");
        let rendered = format!("{creds:?}");
        assert!(!rendered.contains("EXAMPLE"), "token leaked into Debug");
        assert!(rendered.contains("redacted"));

        let rendered = format!("{:?}", creds.bearer());
        assert!(
            !rendered.contains("EXAMPLE"),
            "token leaked into Bearer Debug"
        );
    }
}
