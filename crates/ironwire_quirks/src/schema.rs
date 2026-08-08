//! The quirks schema — and, more importantly, what it deliberately cannot say.
//!
//! # The security property
//!
//! **No type in this module can express a host, a URL, or a filesystem path.**
//!
//! That is the whole design. A remotely-updatable document that could name a
//! host would let whoever controls the signing key redirect a subscription
//! token to a server of their choosing — turning our own update channel into
//! an exfiltration path and voiding `docs/TRUST.md` I2. Validating such a field
//! would not be enough: the check would be one refactor away from being wrong.
//! Making it unrepresentable is enough forever.
//!
//! So: base URLs, credential file locations, and the credential→host binding
//! stay compiled into the binary and change only through a release.
//!
//! ## Review rule
//!
//! Unknown fields are **allowed** (forward compatibility — an old binary must
//! tolerate a newer document). The security property therefore is not "the
//! document contains no host", it is "*this binary's types* have no host field,
//! so a host in the document is inert". Adding a field here that names a
//! network location or a path is a `TRUST.md` change, not a schema change.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Schema version this binary understands.
///
/// A document declaring a *newer* major schema is refused rather than
/// partially applied — half-understanding a provider workaround is worse than
/// using the compiled-in defaults.
pub const SCHEMA_VERSION: u32 = 1;

/// Provider values that move faster than our release cadence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quirks {
    /// Schema this document targets.
    pub schema_version: u32,
    /// Monotonic counter. A document with a serial at or below the one already
    /// installed is refused — that is the rollback protection.
    pub serial: u64,
    /// When the document was issued. Display and diagnostics only; `serial` is
    /// what ordering decisions use, because a clock is not authenticated by a
    /// signature over it.
    pub issued_at: DateTime<Utc>,
    /// Anthropic protocol constants.
    #[serde(default)]
    pub anthropic: AnthropicQuirks,
    /// How a client's own identity is recognised (`docs/TRUST.md` §3).
    #[serde(default)]
    pub client_identity: ClientIdentityQuirks,
    /// Model catalogues, keyed by backend id.
    #[serde(default)]
    pub models: BTreeMap<String, Vec<ModelEntry>>,
}

/// Anthropic wire constants. Strings the API validates, not places we send to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnthropicQuirks {
    /// `anthropic-version` header value.
    pub api_version: String,
    /// Beta flag that enables OAuth bearer auth. This is the value most likely
    /// to change under us, and the reason this channel exists.
    pub oauth_beta: String,
}

impl Default for AnthropicQuirks {
    fn default() -> Self {
        Self {
            api_version: "2023-06-01".to_string(),
            oauth_beta: "oauth-2025-04-20".to_string(),
        }
    }
}

/// Markers that identify which product a request came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientIdentityQuirks {
    /// Prefix of one of Claude Code's system blocks.
    pub claude_code_system_prefix: String,
    /// Prefix of the `user-agent` Claude Code sends. Refreshable for the same
    /// reason as the Codex originator: it is the half that survives the client
    /// rewriting its prompt, which Claude Code has already done.
    #[serde(default = "default_claude_code_user_agent_prefix")]
    pub claude_code_user_agent_prefix: String,
    /// Substring identifying Codex in a Responses `instructions` field.
    pub codex_instructions_marker: String,
    /// Prefix of the `originator` header Codex sends. Refreshable because it is
    /// the half that keeps working when Codex reshapes its request body — which
    /// it has already done once, dropping `instructions` entirely.
    #[serde(default = "default_codex_originator_prefix")]
    pub codex_originator_prefix: String,
    /// Phrases suggesting a request is a compaction turn
    /// (`docs/PROTOCOL.md` §8).
    ///
    /// Refreshable precisely because these are the least knowable strings in
    /// the product: every harness words its compaction prompt differently, none
    /// document it, and all of them change it. Advisory only — a wrong value
    /// costs a slightly worse routing decision, never a wrong answer.
    #[serde(default = "default_compaction_markers")]
    pub compaction_markers: Vec<String>,
}

fn default_claude_code_user_agent_prefix() -> String {
    ironwire_core::peek::CLAUDE_CODE_USER_AGENT_PREFIX.to_string()
}

fn default_codex_originator_prefix() -> String {
    ironwire_core::peek::CODEX_ORIGINATOR_PREFIX.to_string()
}

fn default_compaction_markers() -> Vec<String> {
    ironwire_core::peek::COMPACTION_MARKERS
        .iter()
        .map(|m| (*m).to_string())
        .collect()
}

impl Default for ClientIdentityQuirks {
    fn default() -> Self {
        Self {
            claude_code_system_prefix: "You are Claude Code".to_string(),
            claude_code_user_agent_prefix: default_claude_code_user_agent_prefix(),
            codex_instructions_marker: "Codex".to_string(),
            codex_originator_prefix: default_codex_originator_prefix(),
            compaction_markers: default_compaction_markers(),
        }
    }
}

/// One model a backend can serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Provider's model slug.
    pub id: String,
    /// Quality tier: `fast`, `balanced`, or `frontier`. A value we do not
    /// recognise is treated as `frontier` by the caller — over-serving is
    /// recoverable, under-serving is silent.
    pub tier: String,
}

impl Default for Quirks {
    /// The compiled-in baseline: what this binary knows without a network.
    ///
    /// A fresh install with no connectivity must work, so the defaults are the
    /// values that were correct when the binary shipped — never empty.
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            serial: 0,
            issued_at: DateTime::UNIX_EPOCH,
            anthropic: AnthropicQuirks::default(),
            client_identity: ClientIdentityQuirks::default(),
            models: BTreeMap::new(),
        }
    }
}

impl Quirks {
    /// Models for a backend, or an empty slice when the document says nothing —
    /// in which case the backend keeps its compiled-in catalogue.
    #[must_use]
    pub fn models_for(&self, backend_id: &str) -> &[ModelEntry] {
        self.models.get(backend_id).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property this whole module exists to hold. If someone adds a field
    /// that names a host or a path, this fails and they have to come read the
    /// module docs.
    #[test]
    fn the_schema_cannot_express_a_host_a_url_or_a_path() {
        let serialised = serde_json::to_string(&Quirks::default()).expect("serialises");
        let document: serde_json::Value = serde_json::from_str(&serialised).expect("parses");

        fn walk(value: &serde_json::Value, path: &str) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, child) in map {
                        let lower = key.to_ascii_lowercase();
                        for banned in ["url", "host", "endpoint", "path", "base", "addr", "uri"] {
                            assert!(
                                !lower.contains(banned),
                                "`{path}.{key}` names a network location or filesystem path. \
                                 A signed remote document must not be able to redirect where a \
                                 credential goes — see the module docs and docs/TRUST.md I2."
                            );
                        }
                        walk(child, &format!("{path}.{key}"));
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        walk(item, path);
                    }
                }
                _ => {}
            }
        }
        walk(&document, "quirks");
    }

    #[test]
    fn the_compiled_in_default_is_usable_offline() {
        // A fresh install with no network must still talk to Anthropic.
        let quirks = Quirks::default();
        assert!(!quirks.anthropic.api_version.is_empty());
        assert!(!quirks.anthropic.oauth_beta.is_empty());
        assert!(!quirks.client_identity.claude_code_system_prefix.is_empty());
    }

    #[test]
    fn unknown_fields_are_tolerated_for_forward_compatibility() {
        // An older binary must survive a newer document rather than refusing
        // to start — that is the point of a data channel.
        let document = serde_json::json!({
            "schema_version": 1,
            "serial": 7,
            "issued_at": "2026-08-08T00:00:00Z",
            "anthropic": {"api_version": "2023-06-01", "oauth_beta": "oauth-2026-01-01",
                          "some_future_knob": true},
            "a_whole_new_section": {"x": 1},
        });
        let quirks: Quirks = serde_json::from_value(document).expect("tolerates unknown fields");
        assert_eq!(quirks.anthropic.oauth_beta, "oauth-2026-01-01");
    }

    #[test]
    fn omitted_sections_fall_back_to_the_compiled_in_values() {
        let document = serde_json::json!({
            "schema_version": 1, "serial": 1, "issued_at": "2026-08-08T00:00:00Z",
        });
        let quirks: Quirks = serde_json::from_value(document).expect("parses");
        assert_eq!(quirks.anthropic, AnthropicQuirks::default());
        assert_eq!(quirks.client_identity, ClientIdentityQuirks::default());
    }

    #[test]
    fn a_backend_with_no_catalogue_keeps_its_own() {
        assert!(Quirks::default().models_for("nearai").is_empty());
    }
}
