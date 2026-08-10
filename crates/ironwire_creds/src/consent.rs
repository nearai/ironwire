//! Recorded consent for subscription backends.
//!
//! `docs/TRUST.md` §2: a subscription backend stays off until the user says yes
//! once, to a specific prompt, in plain language. That answer is recorded with
//! the prompt version it answered, so that changing what we ask invalidates
//! consent given to the old wording rather than silently inheriting it.
//!
//! `ironclaw_llm` emits a `tracing::warn!` at this point. A warning in a log is
//! not consent; this is.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Bumped whenever the consent prompt's *meaning* changes. A user who agreed
/// to v1 has not agreed to v2.
///
/// v2: `ironwire init` asks once for every subscription it found, rather than
/// once per backend, and the default moved from no to yes. Both change what a
/// keypress means, so consent given to v1 does not carry over.
pub const CONSENT_PROMPT_VERSION: u32 = 2;

/// The question a user must answer before a subscription backend is used.
///
/// Data rather than `println!` calls, because there is now more than one surface
/// that has to ask it: `ironwire connect` in a terminal, and the menu bar app.
/// Two hand-written copies of a consent prompt is two prompts, and the one that
/// gets edited is never the one someone read — while `CONSENT_PROMPT_VERSION`
/// goes on claiming they answered the same question.
///
/// `docs/TRUST.md` §2 fixes the content. Changing what it *means* requires
/// bumping the version, which invalidates consent given to the old wording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentPrompt {
    /// The version this wording is. Recorded with the answer.
    pub version: u32,
    /// Backend this grants, e.g. `claude-sub`.
    pub backend_id: String,
    /// What the user calls it, e.g. `Claude`.
    pub product: String,
    /// What IronWire will do, in one sentence.
    pub summary: String,
    /// What the user is taking on, one point at a time. Never softened, and
    /// never reordered so the cost reads last.
    pub points: Vec<String>,
    /// The question itself.
    pub question: String,
}

impl ConsentPrompt {
    /// The prompt for a backend, or `None` where consent is not the gate.
    ///
    /// An unknown id yields `None` rather than a generic prompt: a consent
    /// screen that cannot name what it is about is not consent.
    #[must_use]
    pub fn for_backend(backend_id: &str) -> Option<Self> {
        let (product, host, vendor, alternative) = match backend_id {
            "claude-sub" => (
                "Claude",
                "api.anthropic.com",
                "Anthropic",
                "an Anthropic API key",
            ),
            "codex-sub" => ("Codex", "chatgpt.com", "OpenAI", "an OpenAI API key"),
            _ => return None,
        };
        Some(Self {
            version: CONSENT_PROMPT_VERSION,
            backend_id: backend_id.to_string(),
            product: product.to_string(),
            summary: format!(
                "IronWire will read the OAuth token that {product} Code stores on this \
                 machine and send requests to {host} with it, from this computer only."
            ),
            points: vec![
                format!(
                    "This uses a private authentication path. {vendor} does not document \
                     it and may change or block it at any time."
                ),
                format!(
                    "Using it from a third-party proxy may fall outside your subscription's \
                     intended use. If {vendor} objects, it is your account that is affected."
                ),
                format!("Your token is never sent anywhere except {host}."),
                format!("You can use {alternative} instead — fully supported, no ambiguity."),
            ],
            question: format!("Enable the {product} subscription backend?"),
        })
    }
}

/// One recorded consent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentRecord {
    /// Prompt version the user answered.
    pub prompt_version: u32,
    /// When they answered.
    pub granted_at: DateTime<Utc>,
}

/// All recorded consents, keyed by backend id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConsentLedger {
    entries: BTreeMap<String, ConsentRecord>,
}

impl ConsentLedger {
    /// Load from disk. A missing file is an empty ledger — the correct
    /// starting state, since it means nothing has been consented to.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Persist to disk.
    ///
    /// # Errors
    ///
    /// Propagates I/O and serialization failures; a consent we failed to record
    /// must not be treated as granted.
    /// Written atomically. [`Self::load`] fails closed on a corrupt file —
    /// correct, and it means a truncated write would silently withdraw *every*
    /// consent the user ever gave, not just fail to record the new one. Nothing
    /// about that symptom points at a crash (`ironwire_core::atomic`).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        ironwire_core::atomic::write(path, &text)
    }

    /// Whether `backend_id` has current consent.
    ///
    /// Consent recorded against an older prompt version does not count.
    #[must_use]
    pub fn is_granted(&self, backend_id: &str) -> bool {
        self.entries
            .get(backend_id)
            .is_some_and(|r| r.prompt_version >= CONSENT_PROMPT_VERSION)
    }

    /// Record consent for `backend_id`.
    pub fn grant(&mut self, backend_id: &str, now: DateTime<Utc>) {
        self.entries.insert(
            backend_id.to_string(),
            ConsentRecord {
                prompt_version: CONSENT_PROMPT_VERSION,
                granted_at: now,
            },
        );
    }

    /// Withdraw consent for `backend_id`.
    pub fn revoke(&mut self, backend_id: &str) {
        self.entries.remove(backend_id);
    }

    /// Backends with current consent, sorted.
    #[must_use]
    pub fn granted(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(_, r)| r.prompt_version >= CONSENT_PROMPT_VERSION)
            .map(|(id, _)| id.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp")
    }

    #[test]
    fn nothing_is_consented_by_default() {
        let ledger = ConsentLedger::default();
        assert!(!ledger.is_granted("claude-sub"));
        assert!(ledger.granted().is_empty());
    }

    #[test]
    fn a_missing_file_is_an_empty_ledger_not_a_crash() {
        let ledger = ConsentLedger::load(Path::new("/nonexistent/consent.json"));
        assert!(!ledger.is_granted("claude-sub"));
    }

    #[test]
    fn grant_and_revoke_round_trip_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("consent.json");

        let mut ledger = ConsentLedger::default();
        ledger.grant("claude-sub", now());
        ledger.save(&path).expect("saves");

        let reloaded = ConsentLedger::load(&path);
        assert!(reloaded.is_granted("claude-sub"));
        assert_eq!(reloaded.granted(), vec!["claude-sub"]);

        let mut reloaded = reloaded;
        reloaded.revoke("claude-sub");
        reloaded.save(&path).expect("saves");
        assert!(!ConsentLedger::load(&path).is_granted("claude-sub"));
    }

    #[test]
    fn consent_to_an_older_prompt_does_not_carry_forward() {
        // If we change what we're asking, the old yes doesn't answer it.
        let json = r#"{"claude-sub": {"prompt_version": 0, "granted_at": "2026-01-01T00:00:00Z"}}"#;
        let ledger: ConsentLedger = serde_json::from_str(json).expect("parses");
        assert!(!ledger.is_granted("claude-sub"));
    }
}

#[cfg(test)]
mod durability_tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn a_corrupt_ledger_grants_nothing() {
        // Fail-closed is the only acceptable direction here: a consent ledger
        // that fails *open* would use someone's subscription because a file got
        // truncated.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("consent.json");
        std::fs::write(&path, "{\"entries\": {\"claude-sub\"").expect("write");

        let ledger = ConsentLedger::load(&path);
        assert!(!ledger.is_granted("claude-sub"));
    }

    #[test]
    fn a_missing_ledger_grants_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ConsentLedger::load(&dir.path().join("absent.json"));
        assert!(!ledger.is_granted("claude-sub"));
    }

    #[test]
    fn saving_leaves_no_partial_file_and_no_temp_file() {
        // The pairing that makes atomicity matter: `load` fails closed, so a
        // truncated write does not fail — it silently withdraws every consent
        // the user ever gave, and nothing about that points at a crash.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("consent.json");

        let mut ledger = ConsentLedger::default();
        ledger.grant("claude-sub", Utc::now());
        ledger.grant("codex-sub", Utc::now());
        ledger.save(&path).expect("saves");

        let reloaded = ConsentLedger::load(&path);
        assert!(reloaded.is_granted("claude-sub"));
        assert!(reloaded.is_granted("codex-sub"));

        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["consent.json".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn the_ledger_is_not_writable_by_other_local_users() {
        // Another local user granting a consent on this user's behalf would be
        // a straightforward way around `docs/TRUST.md` §2.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("consent.json");
        ConsentLedger::default().save(&path).expect("saves");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o077, 0, "group or other can access it: {mode:o}");
    }

    /// Both surfaces that ask for consent read the same wording from here. A
    /// second copy would drift, and the recorded version would go on claiming
    /// both users answered the same question.
    #[test]
    fn every_backend_that_requires_consent_has_a_prompt() {
        for backend in ["claude-sub", "codex-sub"] {
            let prompt = ConsentPrompt::for_backend(backend).expect("has a prompt");
            assert_eq!(prompt.backend_id, backend);
            assert_eq!(prompt.version, CONSENT_PROMPT_VERSION);
            assert!(!prompt.summary.is_empty());
            assert!(!prompt.question.is_empty());
        }
    }

    /// A consent screen that cannot name what it is about is not consent, so an
    /// unknown backend gets nothing rather than a generic form of words.
    #[test]
    fn an_unknown_backend_has_no_prompt_rather_than_a_generic_one() {
        assert!(ConsentPrompt::for_backend("nearai").is_none());
        assert!(ConsentPrompt::for_backend("").is_none());
    }

    /// `docs/TRUST.md` §2 fixes what has to be said. These are the four things a
    /// user is agreeing to, and none of them may quietly go missing.
    #[test]
    fn the_prompt_states_the_cost_and_not_only_the_benefit() {
        let prompt = ConsentPrompt::for_backend("claude-sub").expect("has a prompt");
        let all = prompt.points.join(" ");
        assert!(
            all.contains("does not document"),
            "the path being private: {all}"
        );
        assert!(
            all.contains("your account that is affected"),
            "who bears the risk: {all}"
        );
        assert!(
            all.contains("never sent anywhere except"),
            "where the token goes: {all}"
        );
        assert!(all.contains("instead"), "the alternative: {all}");
        assert!(
            prompt.summary.contains("this computer only"),
            "the scope: {}",
            prompt.summary
        );
    }

    /// The prompt names the product and the host it will talk to, because
    /// "enable the subscription backend" on its own does not say what happens.
    #[test]
    fn the_prompt_names_the_product_and_the_host() {
        let claude = ConsentPrompt::for_backend("claude-sub").expect("has a prompt");
        assert!(
            claude.summary.contains("api.anthropic.com"),
            "{}",
            claude.summary
        );
        assert!(claude.question.contains("Claude"), "{}", claude.question);

        let codex = ConsentPrompt::for_backend("codex-sub").expect("has a prompt");
        assert!(codex.summary.contains("chatgpt.com"), "{}", codex.summary);
        assert!(codex.question.contains("Codex"), "{}", codex.question);
    }

    /// The prompt is stored as sentences, not pre-broken lines: a terminal wraps
    /// it to its width and a menu lays it out in a view. Embedded newlines would
    /// make one of those two look wrong.
    #[test]
    fn the_prompt_carries_no_layout_of_its_own() {
        let prompt = ConsentPrompt::for_backend("claude-sub").expect("has a prompt");
        assert!(!prompt.summary.contains('\n'));
        for point in &prompt.points {
            assert!(!point.contains('\n'), "{point}");
            assert!(
                !point.contains("  "),
                "double spaces suggest hand-wrapping: {point}"
            );
        }
    }
}
