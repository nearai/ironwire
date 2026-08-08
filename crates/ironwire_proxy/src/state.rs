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
                    models: backend.models().to_vec(),
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
    /// (`docs/UPDATES.md`). Read-mostly, so a plain snapshot rather than a lock.
    pub quirks: Arc<QuirksStore>,
    /// What the last update check concluded. Notify-only — IronWire never
    /// applies an update itself.
    pub update: Arc<Mutex<UpdateStatus>>,
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
            quirks: Arc::new(QuirksStore::new(ironwire_quirks::QUIRKS_PUBLIC_KEY)),
            update: Arc::new(Mutex::new(UpdateStatus::Unknown)),
            port: config.server.port,
            config: Arc::new(config),
            control_token: Arc::new(control_token),
        }
    }

    /// Install a quirks store loaded at startup.
    #[must_use]
    pub fn with_quirks(mut self, quirks: QuirksStore) -> Self {
        self.quirks = Arc::new(quirks);
        self
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
        let quirks = self.quirks.current();
        IdentityMarkers {
            claude_code_system_prefix: quirks.client_identity.claude_code_system_prefix.clone(),
            codex_instructions_marker: quirks.client_identity.codex_instructions_marker.clone(),
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
