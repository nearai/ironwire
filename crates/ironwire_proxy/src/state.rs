//! Daemon state: the backend registry, the routing policy, and consent.

use std::sync::{Arc, Mutex};

use ironwire_core::config::Config;
use ironwire_core::peek::IdentityMarkers;
use ironwire_core::policy::{Candidate, Policy};
use ironwire_core::protocol::BackendId;
use ironwire_creds::ConsentLedger;
use ironwire_ledger::Ledger;
use ironwire_quirks::QuirksStore;
use ironwire_update::UpdateStatus;
use ironwire_upstream::backend::{Backend, BackendStatus};
use ironwire_upstream::breaker::BreakerBoard;

use crate::events::EventBus;
use crate::privacy::PrivacyFilter;

/// The set of backends this daemon can route to.
#[derive(Clone, Default)]
pub struct BackendRegistry {
    backends: Vec<Arc<dyn Backend>>,
}

impl BackendRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a backend. Order is the tie-break for equally-preferred candidates.
    pub fn push(&mut self, backend: Arc<dyn Backend>) {
        self.backends.push(backend);
    }

    /// Look up by id.
    #[must_use]
    pub fn get(&self, id: &BackendId) -> Option<&Arc<dyn Backend>> {
        self.backends.iter().find(|b| b.id() == id)
    }

    /// All backends.
    #[must_use]
    pub fn all(&self) -> &[Arc<dyn Backend>] {
        &self.backends
    }

    /// Whether anything is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    /// Snapshot every backend's status, including a fresh credential check.
    pub async fn statuses(&self) -> Vec<BackendStatus> {
        let mut out = Vec::with_capacity(self.backends.len());
        for backend in &self.backends {
            out.push(backend.status().await);
        }
        out
    }

    /// Build the candidate list the router sees.
    ///
    /// `authenticated` comes from the freshly-collected statuses rather than a
    /// cached flag: a credential that expired ten seconds ago should not be
    /// routed to just because it worked at boot.
    #[must_use]
    pub fn candidates(
        &self,
        statuses: &[BackendStatus],
        consent: &ConsentLedger,
    ) -> Vec<Candidate> {
        self.backends
            .iter()
            .map(|backend| {
                let status = statuses.iter().find(|s| s.id == *backend.id());
                Candidate {
                    id: backend.id().clone(),
                    kind: backend.kind(),
                    caps: backend.capabilities().clone(),
                    quota: backend.quota(),
                    healthy: status.is_none_or(|s| s.authenticated),
                    consented: !backend.kind().requires_consent()
                        || consent.is_granted(backend.id().as_str()),
                    requires_client_identity: backend.requires_client_identity(),
                    models: backend.models(),
                    catalogue_from_provider: backend.catalogue_from_provider(),
                }
            })
            .collect()
    }
}

/// Shared daemon state.
#[derive(Clone)]
pub struct AppState {
    /// Registered backends.
    pub backends: BackendRegistry,
    /// Routing policy, including per-conversation affinity.
    pub policy: Arc<Mutex<Policy>>,
    /// Recorded subscription consents (`docs/TRUST.md` §2).
    pub consent: Arc<Mutex<ConsentLedger>>,
    /// Loaded configuration.
    pub config: Arc<Config>,
    /// Control-API bearer token. The control plane exposes the ledger and can
    /// change routing, so loopback alone is not enough on a shared machine.
    pub control_token: Arc<String>,
    /// Local trace ledger. `None` when `capture.enabled = false`, or when the
    /// ledger could not be opened — a ledger problem must never stop the proxy
    /// from doing its actual job.
    pub ledger: Option<Ledger>,
    /// Provider values refreshed through the signed quirks channel
    /// (`docs/UPDATES.md`).
    ///
    /// Behind a lock because a running daemon can install a newer document —
    /// which is the whole point of the channel: one that needed a restart to
    /// take effect would have the same latency as a release. The lock is held
    /// only long enough to clone an `Arc`, so the read path is a pointer copy.
    quirks: Arc<Mutex<Arc<QuirksStore>>>,
    /// What the last update check concluded. Notify-only — IronWire never
    /// applies an update itself.
    pub update: Arc<Mutex<UpdateStatus>>,
    /// Per-backend circuit state, so a failure is remembered past the end of
    /// the request that hit it (`ironwire_upstream::breaker`).
    pub breakers: Arc<BreakerBoard>,
    /// Route and health events, for `ironwire watch` and the menu bar app.
    /// Lossy and non-blocking by construction (`crate::events`).
    pub events: EventBus,
    /// The optional privacy filter. `None` unless the user turned it on and
    /// configured something for it to match (`docs/PRIVACY.md`).
    pub privacy: Option<Arc<PrivacyFilter>>,
    /// Port actually bound. Distinct from `config.server.port`, which is only
    /// a request: a `--port` override or a config reload would otherwise make
    /// `status` report a number nothing is listening on.
    pub port: u16,
}

impl AppState {
    /// Build state around a registry.
    #[must_use]
    pub fn new(
        backends: BackendRegistry,
        config: Config,
        consent: ConsentLedger,
        control_token: String,
    ) -> Self {
        Self {
            backends,
            policy: Arc::new(Mutex::new(Policy::new())),
            consent: Arc::new(Mutex::new(consent)),
            ledger: None,
            quirks: Arc::new(Mutex::new(Arc::new(QuirksStore::new(
                ironwire_quirks::QUIRKS_PUBLIC_KEY,
            )))),
            update: Arc::new(Mutex::new(UpdateStatus::Unknown)),
            breakers: Arc::new(BreakerBoard::default()),
            events: EventBus::new(),
            privacy: PrivacyFilter::from_config(&config.privacy).map(Arc::new),
            port: config.server.port,
            config: Arc::new(config),
            control_token: Arc::new(control_token),
        }
    }

    /// Install a quirks store loaded at startup.
    #[must_use]
    pub fn with_quirks(self, quirks: QuirksStore) -> Self {
        self.set_quirks(Arc::new(quirks));
        self
    }

    /// The quirks in force. A pointer copy, not a deep clone.
    #[must_use]
    pub fn quirks(&self) -> Arc<QuirksStore> {
        match self.quirks.lock() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// Install a newer quirks document, for the background refresh.
    pub fn set_quirks(&self, quirks: Arc<QuirksStore>) {
        match self.quirks.lock() {
            Ok(mut guard) => *guard = quirks,
            Err(poisoned) => *poisoned.into_inner() = quirks,
        }
    }

    /// Record what the last update check concluded.
    pub fn set_update_status(&self, status: UpdateStatus) {
        match self.update.lock() {
            Ok(mut slot) => *slot = status,
            Err(poisoned) => *poisoned.into_inner() = status,
        }
    }

    /// The last update check's conclusion.
    #[must_use]
    pub fn update_status(&self) -> UpdateStatus {
        match self.update.lock() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Identity markers currently in force.
    #[must_use]
    pub fn identity_markers(&self) -> IdentityMarkers {
        let store = self.quirks();
        let quirks = store.current();
        IdentityMarkers {
            claude_code_system_prefix: quirks.client_identity.claude_code_system_prefix.clone(),
            claude_code_user_agent_prefix: quirks
                .client_identity
                .claude_code_user_agent_prefix
                .clone(),
            codex_instructions_marker: quirks.client_identity.codex_instructions_marker.clone(),
            codex_originator_prefix: quirks.client_identity.codex_originator_prefix.clone(),
            compaction_markers: quirks.client_identity.compaction_markers.clone(),
        }
    }

    /// Attach a trace ledger.
    #[must_use]
    pub fn with_ledger(mut self, ledger: Option<Ledger>) -> Self {
        self.ledger = ledger;
        self
    }

    /// Record the port actually bound.
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Read the consent ledger.
    #[must_use]
    pub fn consent_snapshot(&self) -> ConsentLedger {
        match self.consent.lock() {
            Ok(c) => c.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ironwire_core::protocol::BackendKind;
    use ironwire_core::quota::QuotaSnapshot;
    use ironwire_upstream::anthropic::{AnthropicBackend, anthropic_models};

    fn registry() -> BackendRegistry {
        let mut registry = BackendRegistry::new();
        registry.push(Arc::new(
            AnthropicBackend::subscription(None, 60).expect("client builds"),
        ));
        registry
    }

    fn status(id: &str, authenticated: bool) -> BackendStatus {
        BackendStatus {
            id: BackendId::from(id),
            name: id.to_string(),
            kind: BackendKind::Subscription,
            authenticated,
            detail: None,
            quota: QuotaSnapshot::default(),
            models: anthropic_models(),
        }
    }

    #[test]
    fn a_subscription_without_consent_is_not_a_usable_candidate() {
        let registry = registry();
        let statuses = vec![status("claude-sub", true)];
        let candidates = registry.candidates(&statuses, &ConsentLedger::default());
        assert_eq!(candidates.len(), 1);
        assert!(!candidates[0].consented);
    }

    #[test]
    fn granting_consent_makes_the_candidate_usable() {
        let registry = registry();
        let mut consent = ConsentLedger::default();
        consent.grant("claude-sub", Utc::now());
        let statuses = vec![status("claude-sub", true)];
        let candidates = registry.candidates(&statuses, &consent);
        assert!(candidates[0].consented);
    }

    #[test]
    fn an_unauthenticated_backend_is_marked_unhealthy() {
        let registry = registry();
        let statuses = vec![status("claude-sub", false)];
        let candidates = registry.candidates(&statuses, &ConsentLedger::default());
        assert!(!candidates[0].healthy);
    }

    #[test]
    fn lookup_by_id_finds_registered_backends() {
        let registry = registry();
        assert!(registry.get(&BackendId::from("claude-sub")).is_some());
        assert!(registry.get(&BackendId::from("nope")).is_none());
        assert!(!registry.is_empty());
    }
}

#[cfg(test)]
mod quirks_tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use ironwire_core::config::Config;
    use ironwire_creds::ConsentLedger;

    fn state() -> AppState {
        AppState::new(
            BackendRegistry::new(),
            Config::default(),
            ConsentLedger::default(),
            "t".to_string(),
        )
    }

    #[test]
    fn a_fresh_daemon_runs_on_the_compiled_in_values() {
        let state = state();
        assert_eq!(
            state.quirks().serial(),
            0,
            "0 means built-ins, not a document"
        );
        assert_eq!(
            state.identity_markers().claude_code_system_prefix,
            ironwire_core::peek::CLAUDE_CODE_SYSTEM_PREFIX
        );
    }

    #[test]
    fn a_newer_signed_document_takes_effect_without_a_restart() {
        // The whole point of the channel. One that needed a restart would have
        // the same latency as a release, which is the problem it solves.
        //
        // Signed properly rather than installed through a back door: an
        // unverified-install path would defeat the channel entirely
        // (`docs/TRUST.md` I2), so this test uses its own keypair and goes
        // through `apply`, exercising the real path.
        let signing = SigningKey::from_bytes(&[11u8; 32]);
        let verifying = signing.verifying_key().to_bytes();

        let document = serde_json::json!({
            "schema_version": 1,
            "serial": 42,
            "issued_at": "2026-08-08T00:00:00Z",
            "client_identity": {
                "claude_code_system_prefix": "You are Something Else",
                "codex_instructions_marker": "Codex",
            },
        })
        .to_string();
        let signed = ironwire_quirks::SignedQuirks {
            signature: hex::encode(signing.sign(document.as_bytes()).to_bytes()),
            document,
        };

        let mut store = QuirksStore::new(verifying);
        store
            .apply(&signed)
            .expect("a correctly signed document applies");

        let state = state();
        assert_eq!(state.quirks().serial(), 0);
        state.set_quirks(Arc::new(store));

        assert_eq!(state.quirks().serial(), 42);
        assert_eq!(
            state.identity_markers().claude_code_system_prefix,
            "You are Something Else"
        );
    }

    #[test]
    fn a_document_signed_by_the_wrong_key_never_reaches_the_daemon() {
        let attacker = SigningKey::from_bytes(&[99u8; 32]);
        let ours = SigningKey::from_bytes(&[11u8; 32])
            .verifying_key()
            .to_bytes();

        let document = serde_json::json!({
            "schema_version": 1,
            "serial": 99,
            "issued_at": "2026-08-08T00:00:00Z",
            "client_identity": {"claude_code_system_prefix": "You are Evil"},
        })
        .to_string();
        let signed = ironwire_quirks::SignedQuirks {
            signature: hex::encode(attacker.sign(document.as_bytes()).to_bytes()),
            document,
        };

        let mut store = QuirksStore::new(ours);
        assert!(
            store.apply(&signed).is_err(),
            "a document signed by the wrong key was accepted"
        );
        assert_eq!(store.serial(), 0, "the built-ins must stay in force");
    }

    #[test]
    fn reading_the_quirks_is_a_pointer_copy_not_a_deep_clone() {
        // It happens on every request; a deep clone of the model catalogue per
        // request would be a real cost for no reason.
        let state = state();
        let a = state.quirks();
        let b = state.quirks();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
