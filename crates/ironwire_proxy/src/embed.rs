//! Start IronWire inside a host application using the same assembly as the CLI.
//!
//! Errors carry fixed labels and at most a port; never credentials or bodies.

use std::sync::Arc;

use crate::state::{AppState, BackendRegistry};
use anyhow::{Context, Result};
use ironwire_catalog::CatalogStore;
use ironwire_core::config::ModelEntry;
use ironwire_core::config::{Config, PathsConfig};
use ironwire_core::protocol::{BackendId, BackendKind, ModelTier};
use ironwire_creds::ConsentLedger;
use ironwire_creds::claude::ClaudeCodeCredentials;
use ironwire_creds::codex::{CodexCredentials, CodexMode};
use ironwire_ledger::Ledger;
use ironwire_ledger::bodies::BodyStore;
use ironwire_upstream::anthropic::AnthropicBackend;
use ironwire_upstream::openai_chat::ChatCompletionsBackend;
use ironwire_upstream::openai_responses::ResponsesBackend;
use secrecy::SecretString;

mod catalog;
#[doc(hidden)]
pub mod files;
mod lock;
mod prune;
#[doc(hidden)]
pub mod updates;

/// Fixed-label startup failures, suitable for a host's refusal state.
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    /// Home or token initialization failed.
    #[error("could not prepare the IronWire home or control token")]
    Paths,
    /// Configuration was unreadable, invalid, or incompatible with spend limits.
    #[error("invalid config.toml; check the configuration and enable capture for spend limits")]
    Config,
    /// Another process owns this home.
    #[error("another IronWire owns this home on port {port}")]
    Lock {
        /// Port published by the current owner, or the requested port during startup.
        port: u16,
    },
    /// Another listener occupies the requested loopback port.
    #[error("port {port} is already in use")]
    PortInUse {
        /// Requested port.
        port: u16,
    },
    /// A loopback bind failed for another reason.
    #[error("could not bind the loopback listener")]
    Bind,
    /// A configured backend could not be constructed.
    #[error("could not build the backend registry")]
    Registry,
}

/// Startup observations rendered by the CLI; an embedded host controls its UI.
#[derive(Default)]
pub struct StartupReport {
    /// No backend could be registered.
    pub no_backends: bool,
    /// Verified catalog serial, or zero for built-in values.
    pub catalog_serial: u64,
    /// Optional local ledger-open diagnostic for the CLI.
    pub ledger_warning: Option<String>,
    /// Optional body-store-open diagnostic for the CLI.
    pub bodies_warning: Option<String>,
    /// The home discovery pointer could not be published.
    pub pointer_warning: bool,
}

/// A server's final outcome. No internal error text crosses the host boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExitError {
    /// The server returned an I/O error.
    #[error("the IronWire server stopped with an error")]
    Server,
    /// The server task panicked or was canceled.
    #[error("the IronWire server task ended unexpectedly")]
    Task,
}

/// Owns one running proxy. Dropping it requests graceful shutdown; resources
/// remain owned by the supervisor until in-flight requests finish. Hosts that
/// need confirmation of cleanup must await `shutdown`, not merely drop.
///
/// Task panics can be observed only when the host compiles with panic unwinding.
/// A host using `panic = "abort"` cannot contain a panic in any Tokio task.
pub struct EmbeddedProxy {
    port: u16,
    report: StartupReport,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Result<(), ExitError>>>,
    exit: Option<Result<(), ExitError>>,
}

impl EmbeddedProxy {
    /// Actual bound loopback port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }
    /// Startup observations for the host to render.
    #[must_use]
    pub fn startup_report(&self) -> &StartupReport {
        &self.report
    }
    /// True after the server and its housekeeping have ended.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.exit.is_some()
            || self
                .task
                .as_ref()
                .is_none_or(tokio::task::JoinHandle::is_finished)
    }
    /// Observe an unexpected exit without transferring shutdown ownership.
    /// Cancellation-safe: dropping this future leaves the proxy running.
    pub async fn wait(&mut self) -> Result<(), ExitError> {
        if let Some(exit) = self.exit {
            return exit;
        }
        let exit = match self.task.as_mut() {
            Some(task) => task.await.unwrap_or(Err(ExitError::Task)),
            None => Ok(()),
        };
        self.task = None;
        self.exit = Some(exit);
        exit
    }
    /// Drain in-flight requests, stop housekeeping, flush quota, and release
    /// the listener, discovery pointer, and home lock. No timeout truncates a
    /// model stream; the host owns any policy for abandoning its own process.
    pub async fn shutdown(mut self) {
        self.request_shutdown();
        let _ = self.wait().await;
    }
    fn request_shutdown(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for EmbeddedProxy {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

struct OwnedPointer {
    path: std::path::PathBuf,
    endpoint: ironwire_core::discovery::Endpoint,
}
impl Drop for OwnedPointer {
    fn drop(&mut self) {
        if ironwire_core::discovery::Endpoint::read_from(&self.path).as_ref()
            == Some(&self.endpoint)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

struct Tasks(Vec<tokio::task::JoinHandle<()>>);
impl Tasks {
    async fn stop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
        for task in self.0.drain(..) {
            let _ = task.await;
        }
    }
}
impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Start a loopback proxy against an explicit IronWire home. An empty home is
/// prepared automatically; existing configuration and consent are preserved.
/// `Some(0)` requests an ephemeral port, reported by `EmbeddedProxy::port`.
///
/// The pointer is published within this home only. The CLI additionally
/// publishes its conventional desktop-discovery pointer. Starting an embedded
/// instance never overwrites an unrelated real user's discovery file.
///
/// # Errors
/// Returns a fixed-label refusal before starting any background task.
pub async fn start(
    home: &std::path::Path,
    port_override: Option<u16>,
) -> Result<EmbeddedProxy, EmbedError> {
    std::fs::create_dir_all(home).map_err(|_| EmbedError::Paths)?;
    files::restrict_permissions(home, 0o700).map_err(|_| EmbedError::Paths)?;
    let paths = PathsConfig::rooted_at(std::fs::canonicalize(home).map_err(|_| EmbedError::Paths)?);
    let config = Config::load(&paths).map_err(|_| EmbedError::Config)?;
    let port = port_override.unwrap_or(config.server.port);
    if config.limits.any_cap() && !config.capture.enabled {
        return Err(EmbedError::Config);
    }
    // Acquire ownership before reading mutable consent or minting a token.
    let mut lock = Some(lock::acquire(&paths.lock_file(), port).await?);
    let token = files::control_token(&paths).map_err(|_| EmbedError::Paths)?;
    let consent = ConsentLedger::load(&paths.consent_file());
    let registry = build_registry(&config).map_err(|_| EmbedError::Registry)?;
    restore_quota(&registry, &paths);
    let listener = crate::server::bind(port).await.map_err(|e| match e {
        crate::server::ServeError::PortInUse { port } => EmbedError::PortInUse { port },
        _ => EmbedError::Bind,
    })?;
    let port = listener.local_addr().map_err(|_| EmbedError::Bind)?.port();
    if let Some(lock) = lock.as_mut() {
        lock.publish(port)?;
    }
    let mut report = StartupReport {
        no_backends: registry.is_empty(),
        ..StartupReport::default()
    };
    let ledger = open_ledger(&paths, &config, &mut report);
    let catalog = CatalogStore::load(ironwire_catalog::CATALOG_PUBLIC_KEY, &paths.catalog_file());
    report.catalog_serial = catalog.serial();
    let bodies = open_bodies(&paths, &config, &mut report);
    sweep_bodies(&ledger, bodies.as_deref());
    let mut background = Tasks(Vec::new());
    if let Some(task) = prune::spawn(ledger.clone(), config.capture.retain_days, bodies.clone()) {
        background.0.push(task);
    }
    let checks = config.updates.check;
    let state = AppState::new(registry, config, consent, token)
        .with_port(port)
        .with_paths(paths.clone())
        .with_ledger(ledger)
        .with_bodies(bodies)
        .with_catalog(catalog);
    seed_spend(&state);
    if let Some(task) = updates::spawn_check(state.clone(), &paths, checks) {
        background.0.push(task);
    }
    if let Some(task) = catalog::spawn_refresh(state.clone(), &paths, checks) {
        background.0.push(task);
    }
    background.0.push(spawn_catalogue_discovery(state.clone()));
    let quota = QuotaWriter::new(paths.quota_file());
    background.0.push(quota.spawn(state.clone()));
    let endpoint = ironwire_core::discovery::Endpoint::new(port, paths.control_token_file());
    let pointer = OwnedPointer {
        path: paths.home.join("endpoint.json"),
        endpoint,
    };
    let published = serde_json::to_string_pretty(&pointer.endpoint)
        .ok()
        .is_some_and(|body| ironwire_core::atomic::write(&pointer.path, &body).is_ok());
    report.pointer_warning = !published;
    let pointer = published.then_some(pointer);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server_state = state.clone();
    let server = tokio::spawn(crate::server::serve_on(
        listener,
        server_state,
        async move {
            let _ = rx.await;
        },
    ));
    let task = tokio::spawn(async move {
        // Await the server in a separate task so unwinding still runs cleanup.
        let result = server
            .await
            .map_err(|_| ExitError::Task)
            .and_then(|r| r.map_err(|_| ExitError::Server));
        background.stop().await;
        quota.write_now(&state);
        drop(state);
        drop(pointer);
        drop(lock);
        result
    });
    Ok(EmbeddedProxy {
        port,
        report,
        shutdown: Some(tx),
        task: Some(task),
        exit: None,
    })
}

/// Writes observed quota to disk, on a timer and once at shutdown.
///
/// One writer, holding the last rendered document, so the periodic write and
/// the shutdown write cannot race into a last-writer-wins where the loser is
/// the *newer* snapshot. Both go through [`Self::write_now`].
#[derive(Clone)]
struct QuotaWriter {
    path: std::path::PathBuf,
    last: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl QuotaWriter {
    /// How often to check whether anything changed.
    ///
    /// A write per request would be absurd — SSE-observed usage updates several
    /// times within a single stream — and the value being protected is only
    /// useful at restart, so half a minute of lag costs nothing.
    const PERIOD: std::time::Duration = std::time::Duration::from_secs(30);

    fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            last: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn spawn(&self, state: crate::state::AppState) -> tokio::task::JoinHandle<()> {
        let writer = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Self::PERIOD);
            ticker.tick().await; // fires immediately; nothing has changed yet
            loop {
                ticker.tick().await;
                writer.write_now(&state);
            }
        })
    }

    /// Render and write, unless nothing has changed since the last write.
    ///
    /// Reads `Backend::quota()` directly rather than `statuses()`: the latter
    /// does a fresh credential check per backend, which on a thirty-second
    /// timer would re-read the Keychain a few thousand times a day to learn
    /// something it already has in a mutex.
    fn write_now(&self, state: &crate::state::AppState) {
        let quotas: Vec<(String, ironwire_core::quota::QuotaSnapshot)> = state
            .backends
            .all()
            .iter()
            .map(|backend| (backend.id().to_string(), backend.quota()))
            .collect();
        let rendered = ironwire_core::quota_store::render(&quotas, chrono::Utc::now());

        let mut last = match self.last.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        // `written_at` changes every time, so compare the part that carries
        // meaning rather than the whole document.
        if last.as_deref().map(strip_written_at) == Some(strip_written_at(&rendered)) {
            return;
        }
        // A bookkeeping failure must never affect routing — same rule as the
        // ledger. Warn once per change, not once per tick.
        if let Err(_error) = ironwire_core::quota_store::write(&self.path, &rendered) {
            tracing::warn!("could not persist observed quota");
            return;
        }
        *last = Some(rendered);
    }
}

/// The document minus its timestamp line, for change detection.
fn strip_written_at(document: &str) -> String {
    document
        .lines()
        .filter(|line| !line.trim_start().starts_with("\"written_at\""))
        .collect()
}

/// Seed each backend with what it had observed before the last shutdown.
///
/// A backend id in the file that is not in this registry is ignored rather than
/// resurrected: the user may have disconnected it, and quota for a backend we
/// will not route to is not a fact about anything.
fn restore_quota(registry: &BackendRegistry, paths: &PathsConfig) {
    let stored = ironwire_core::quota_store::load(&paths.quota_file(), chrono::Utc::now());
    if stored.is_empty() {
        return;
    }
    for backend in registry.all() {
        if let Some(quota) = ironwire_core::quota_store::for_backend(&stored, backend.id().as_str())
        {
            backend.restore_quota(quota);
        }
    }
}

/// Seed the spend tracker from what the ledger already recorded today.
///
/// Metered backends only, and the same local-midnight window `status` reports.
/// Without this a daemon restarted after $8 of a $10 cap would resume at zero,
/// and the cap would be resettable by restarting.
fn seed_spend(state: &crate::state::AppState) {
    if !state.config.limits.any_cap() {
        return;
    }
    let Some(ledger) = state.ledger.as_ref() else {
        return;
    };
    let now = chrono::Utc::now();
    let Ok(summary) = ledger.summary(crate::spend::window_start(now)) else {
        return;
    };
    let metered: std::collections::HashSet<&str> = state
        .backends
        .all()
        .iter()
        .filter(|backend| backend.kind().is_metered())
        .map(|backend| backend.id().as_str())
        .collect();
    let spent = summary
        .cost_by_backend
        .iter()
        .filter(|(backend, _)| metered.contains(backend.as_str()))
        .map(|(backend, cost)| (BackendId::from(backend.as_str()), *cost));

    let seeded = crate::spend::SpendTracker::seeded(spent, now);
    match state.spend.lock() {
        Ok(mut tracker) => *tracker = seeded,
        Err(poisoned) => *poisoned.into_inner() = seeded,
    }
}

/// Learn every backend's catalogue in the background, once, at startup.
///
/// In the background because a probe is a network round trip per backend and
/// the daemon must be answering requests immediately; a request that arrives
/// first simply routes on the compiled-in list, which is what it did before.
/// Failures are logged at debug and otherwise ignored — this is an
/// optimisation of what we know, not a health check, and `ironwire doctor`
/// remains the place that reports whether a backend actually works.
fn spawn_catalogue_discovery(state: crate::state::AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        for backend in state.backends.all() {
            if let Err(_error) = backend.probe().await {
                tracing::debug!("could not learn this backend's catalogue at startup");
            }
        }
    })
}

/// Open the trace ledger, or explain why we are running without one.
///
/// A ledger failure must never stop the proxy: the user's agent working matters
/// more than our bookkeeping.
fn open_ledger(paths: &PathsConfig, config: &Config, report: &mut StartupReport) -> Option<Ledger> {
    if !config.capture.enabled {
        return None;
    }
    match Ledger::open(&paths.ledger_file()) {
        Ok(ledger) => Some(ledger),
        Err(error) => {
            report.ledger_warning = Some(error.to_string());
            None
        }
    }
}

/// Open the body store, or explain why we are running without one.
///
/// Same rule as the ledger: bookkeeping never stops the proxy from serving.
fn open_bodies(
    paths: &PathsConfig,
    config: &Config,
    report: &mut StartupReport,
) -> Option<std::sync::Arc<BodyStore>> {
    if !config.capture.enabled || !config.capture.bodies {
        return None;
    }
    match BodyStore::open(&paths.bodies_dir()) {
        Ok(store) => Some(std::sync::Arc::new(store)),
        Err(error) => {
            report.bodies_warning = Some(error.to_string());
            None
        }
    }
}

/// Collect body files no ledger row claims. Never fatal: an uncollected
/// orphan costs disk, and failing to serve costs the user their agent.
fn sweep_bodies(ledger: &Option<Ledger>, bodies: Option<&BodyStore>) {
    let (Some(ledger), Some(bodies)) = (ledger.as_ref(), bodies) else {
        return;
    };
    match ledger
        .live_body_refs()
        .map_err(|e| e.to_string())
        .and_then(|live| bodies.retain_only(&live).map_err(|e| e.to_string()))
    {
        Ok(0) => {}
        Ok(removed) => tracing::info!(removed, "swept captured bodies no exchange claims"),
        Err(_error) => tracing::warn!("could not sweep the captured bodies"),
    }
}

/// Construct every backend the environment can support.
///
/// A backend that cannot find a credential is still registered: `status` should
/// be able to say "Claude subscription — not logged in" rather than silently
/// omitting it, which reads as though IronWire never heard of it.
fn build_registry(config: &Config) -> Result<BackendRegistry> {
    let timeout = config.server.upstream_timeout_secs;
    let mut registry = BackendRegistry::new();

    // `enabled = false` is filtered here, at the point of registration, rather
    // than by skipping the config entry. A backend can arrive from discovery
    // without any entry naming it, so filtering the config list would let a
    // backend the user switched off come straight back.
    let mut push = |backend: Arc<dyn ironwire_upstream::backend::Backend>| {
        if is_disabled(config, backend.id().as_str()) {
            tracing::info!("backend disabled in config.toml; not registered");
            return;
        }
        registry.push(backend);
    };

    if ClaudeCodeCredentials::discover().is_ok() {
        push(Arc::new(
            AnthropicBackend::subscription(
                base_url_for(config, "claude-sub", "IRONWIRE_ANTHROPIC_BASE_URL"),
                timeout,
            )
            .context("building the Claude subscription backend")?,
        ));
    }

    if let Some(key) = api_key_for(config, "anthropic-key", "ANTHROPIC_API_KEY", &real_env) {
        push(Arc::new(
            AnthropicBackend::api_key(
                key,
                base_url_for(config, "anthropic-key", "IRONWIRE_ANTHROPIC_BASE_URL"),
                timeout,
            )
            .context("building the Anthropic API backend")?,
        ));
    }

    // The ChatGPT subscription. Same wire as the metered OpenAI key below, so
    // the two are rungs of one ladder and falling between them costs nothing
    // but money (`docs/DESIGN.md` §3).
    if CodexCredentials::discover().is_ok_and(|c| c.mode == CodexMode::ChatGpt) {
        push(Arc::new(
            ResponsesBackend::codex_subscription(
                base_url_for(config, "codex-sub", "IRONWIRE_CODEX_BASE_URL"),
                timeout,
            )
            .context("building the ChatGPT subscription backend")?,
        ));
    }

    // A key from the environment, or the one Codex itself stored — a user who
    // ran `codex login --api-key` has exactly one place they expect it to live.
    if let Some(key) =
        api_key_for(config, "openai-key", "OPENAI_API_KEY", &real_env).or_else(codex_stored_key)
    {
        push(Arc::new(
            ResponsesBackend::openai_api_key(
                key,
                base_url_for(config, "openai-key", "IRONWIRE_OPENAI_BASE_URL"),
                timeout,
            )
            .context("building the OpenAI API backend")?,
        ));
    }

    // NEAR AI is a different API family, so it is only ever reached through the
    // translated lane — and only at a turn boundary (`docs/PROTOCOL.md` §6).
    //
    // Registered whether or not a key was found, unlike the backends above. It
    // is the destination `privacy.mode = "full"` names by default, and a
    // trusted destination the user cannot see is one they cannot act on: with
    // no entry at all, `full` reads as broken rather than as needing a key.
    // Without one it reports `authenticated: false` and names what to set.
    push(Arc::new(
        ChatCompletionsBackend::nearai(
            api_key_for(config, "nearai", "NEARAI_API_KEY", &real_env),
            base_url_for(config, "nearai", "IRONWIRE_NEARAI_BASE_URL"),
            models_for(config, "nearai", BackendKind::Credits).unwrap_or_default(),
            timeout,
        )
        .context("building the NEAR AI backend")?,
    ));

    // Anything the user declared that discovery does not produce. Appended
    // rather than prepended: registration order is the tie-break in
    // `Policy::select`, so putting config entries first would silently change
    // which backend wins a tie for every existing user.
    for entry in &config.backends {
        if !entry.enabled || DISCOVERED_IDS.contains(&entry.id.as_str()) {
            continue;
        }
        if let Some(backend) = backend_from_config(entry, timeout, &real_env)? {
            push(backend);
        }
    }

    Ok(registry)
}

/// Ids that discovery produces on its own.
///
/// An entry naming one of these configures it; an entry naming anything else
/// constructs it.
const DISCOVERED_IDS: &[&str] = &[
    "claude-sub",
    "anthropic-key",
    "codex-sub",
    "openai-key",
    "nearai",
];

/// Build a backend that discovery would not have produced.
///
/// `Ok(None)` when the credential is simply absent — the same rule discovery
/// follows, where not being logged in is a normal state rather than an error.
///
/// # Errors
///
/// Propagates a client build failure. Configuration that cannot describe a
/// backend at all is rejected earlier, by `Config::validate`, so that a bad
/// file fails before the port is bound rather than at the first request.
fn backend_from_config(
    entry: &ironwire_core::config::BackendConfig,
    timeout: u64,
    env: EnvLookup<'_>,
) -> Result<Option<Arc<dyn ironwire_upstream::backend::Backend>>> {
    use ironwire_core::config::BackendImpl;

    let key = |default: &str| entry_key(entry, default, env);
    let backend: Option<Arc<dyn ironwire_upstream::backend::Backend>> =
        match BackendImpl::parse(&entry.kind) {
            Some(BackendImpl::ClaudeSubscription) => ClaudeCodeCredentials::discover()
                .is_ok()
                .then(|| {
                    AnthropicBackend::subscription(entry.base_url.clone(), timeout)
                        .context("building a configured Claude subscription backend")
                })
                .transpose()?
                .map(|b| Arc::new(b) as Arc<dyn ironwire_upstream::backend::Backend>),
            Some(BackendImpl::AnthropicApi) => key("ANTHROPIC_API_KEY")
                .map(|key| {
                    AnthropicBackend::api_key(key, entry.base_url.clone(), timeout)
                        .context("building a configured Anthropic API backend")
                })
                .transpose()?
                .map(|b| Arc::new(b) as Arc<dyn ironwire_upstream::backend::Backend>),
            Some(BackendImpl::CodexSubscription) => CodexCredentials::discover()
                .is_ok_and(|c| c.mode == CodexMode::ChatGpt)
                .then(|| {
                    ResponsesBackend::codex_subscription(entry.base_url.clone(), timeout)
                        .context("building a configured ChatGPT subscription backend")
                })
                .transpose()?
                .map(|b| Arc::new(b) as Arc<dyn ironwire_upstream::backend::Backend>),
            Some(BackendImpl::OpenAiApi) => key("OPENAI_API_KEY")
                .or_else(codex_stored_key)
                .map(|key| {
                    ResponsesBackend::openai_api_key(key, entry.base_url.clone(), timeout)
                        .context("building a configured OpenAI API backend")
                })
                .transpose()?
                .map(|b| Arc::new(b) as Arc<dyn ironwire_upstream::backend::Backend>),
            Some(BackendImpl::NearAi) => key("NEARAI_API_KEY")
                .map(|key| {
                    ChatCompletionsBackend::nearai(
                        Some(key),
                        entry.base_url.clone(),
                        entry
                            .models
                            .as_ref()
                            .map(|models| models_from(models, BackendKind::Credits))
                            .unwrap_or_default(),
                        timeout,
                    )
                    .context("building a configured NEAR AI backend")
                })
                .transpose()?
                .map(|b| Arc::new(b) as Arc<dyn ironwire_upstream::backend::Backend>),
            Some(BackendImpl::Local) => {
                // `validate` has already established the base URL. The key is
                // genuinely optional here: most local servers take no auth, and
                // `None` sends no `Authorization` header at all.
                let Some(base_url) = entry.base_url.clone() else {
                    return Ok(None);
                };
                Some(Arc::new(
                    ChatCompletionsBackend::local(
                        BackendId::from(entry.id.as_str()),
                        &entry.id,
                        base_url,
                        entry.api_key_env.as_deref().and_then(|name| {
                            env(name)
                                .filter(|key| !key.is_empty())
                                .map(SecretString::from)
                        }),
                        entry
                            .models
                            .as_ref()
                            .map(|models| models_from(models, BackendKind::Local))
                            .unwrap_or_default(),
                        timeout,
                    )
                    .context("building a configured local backend")?,
                )
                    as Arc<dyn ironwire_upstream::backend::Backend>)
            }
            Some(BackendImpl::OpenAiCompatible) => {
                // `validate` has already established that both are present.
                let (Some(base_url), Some(key)) = (entry.base_url.clone(), key("")) else {
                    return Ok(None);
                };
                Some(Arc::new(
                    ChatCompletionsBackend::new(
                        BackendId::from(entry.id.as_str()),
                        &entry.id,
                        BackendKind::ApiKey,
                        Some(key),
                        base_url,
                        entry
                            .models
                            .as_ref()
                            .map(|models| models_from(models, BackendKind::ApiKey))
                            .unwrap_or_default(),
                        timeout,
                    )
                    .context("building a configured OpenAI-compatible backend")?,
                )
                    as Arc<dyn ironwire_upstream::backend::Backend>)
            }
            // Unreachable for a config that came through `Config::validate`.
            None => None,
        };
    Ok(backend)
}

/// Reading an environment variable, as a value so tests need not mutate a
/// global that every other test in the process shares.
type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The real environment.
fn real_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The key for a config entry: the variable it names, or the default name.
fn entry_key(
    entry: &ironwire_core::config::BackendConfig,
    default_env: &str,
    env: EnvLookup<'_>,
) -> Option<SecretString> {
    let name = entry.api_key_env.as_deref().unwrap_or(default_env);
    if name.is_empty() {
        return None;
    }
    env(name)
        .filter(|key| !key.is_empty())
        .map(SecretString::from)
}

/// The key for a discovered backend, honouring an `api_key_env` override.
///
/// A user whose key lives in `ANTHROPIC_API_KEY_WORK` had no way to say so:
/// the name was hardcoded, and the field that exists to name it was never read.
fn api_key_for(
    config: &Config,
    id: &str,
    default_env: &str,
    env: EnvLookup<'_>,
) -> Option<SecretString> {
    let name = config
        .backends
        .iter()
        .find(|entry| entry.id == id)
        .and_then(|entry| entry.api_key_env.as_deref())
        .unwrap_or(default_env);
    env(name)
        .filter(|key| !key.is_empty())
        .map(SecretString::from)
}

/// Whether the user switched this backend off.
///
/// Checked where a backend is registered rather than where a config entry is
/// read: discovery produces backends no entry names, so filtering the config
/// list would let a disabled backend come straight back.
fn is_disabled(config: &Config, id: &str) -> bool {
    config
        .backends
        .iter()
        .any(|entry| entry.id == id && !entry.enabled)
}

/// The metered OpenAI key Codex itself stored, for a user who ran
/// `codex login --api-key` and expects it to be found there.
fn codex_stored_key() -> Option<SecretString> {
    let creds = CodexCredentials::discover().ok()?;
    (creds.mode == CodexMode::ApiKey).then(|| creds.bearer().token)
}

/// Models configured for a backend, if the user listed any.
///
/// Applies to every kind that accepts a catalogue, not only NEAR AI: a
/// configured list was previously read for one id and silently ignored for
/// every other. `None` means "nothing configured", which is different from an
/// empty list and leaves the provider's own catalogue in charge.
fn models_for(config: &Config, id: &str, kind: BackendKind) -> Option<Vec<(String, ModelTier)>> {
    config
        .backends
        .iter()
        .find(|entry| entry.id == id)
        .and_then(|entry| entry.models.as_ref())
        .map(|models| models_from(models, kind))
}

/// Tier each configured slug for a backend of `kind`.
///
/// The tier is not a property of the name alone: the same slug means something
/// different on local capacity, which sorts cheapest and must not inherit a
/// frontier tier from a name nobody recognises (`ModelEntry::tier_on`).
fn models_from(models: &[ModelEntry], kind: BackendKind) -> Vec<(String, ModelTier)> {
    models
        .iter()
        .map(|entry| (entry.name().to_string(), entry.tier_on(kind)))
        .collect()
}

/// Base-URL override for a backend.
///
/// `config.toml` is the user-facing form; the environment variable exists so
/// the conformance harness can point a real backend at a recording mock
/// without writing config (`docs/PROTOCOL.md` §7.2).
///
/// The environment variable is named per family rather than shared. A single
/// override would send *every* backend to whichever mock the harness happened to
/// start — including one holding a credential for a different provider, which
/// `check_host` would then have to refuse (`docs/TRUST.md` I2).
fn base_url_for(config: &Config, id: &str, env_key: &str) -> Option<String> {
    config
        .backends
        .iter()
        .find(|b| b.id == id)
        .and_then(|b| b.base_url.clone())
        .or_else(|| std::env::var(env_key).ok())
        .filter(|url| !url.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironwire_core::config::BackendConfig;

    fn entry(id: &str, kind: &str) -> BackendConfig {
        BackendConfig {
            id: id.to_string(),
            kind: kind.to_string(),
            enabled: true,
            base_url: None,
            api_key_env: None,
            models: None,
        }
    }

    fn env_with(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name: &str| {
            owned
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    /// The field existed and was never read, so a key that did not live under
    /// the hardcoded name was unreachable.
    #[test]
    fn api_key_env_names_the_variable_the_key_is_read_from() {
        let env = env_with(&[("WORK_KEY", "sk-work"), ("ANTHROPIC_API_KEY", "sk-default")]);
        let mut config = Config::default();
        let mut declared = entry("anthropic-key", "anthropic-api");
        declared.api_key_env = Some("WORK_KEY".to_string());
        config.backends.push(declared);

        let key = api_key_for(&config, "anthropic-key", "ANTHROPIC_API_KEY", &env)
            .expect("a key was configured");
        assert_eq!(secrecy::ExposeSecret::expose_secret(&key), "sk-work");
    }

    #[test]
    fn the_default_variable_is_used_when_none_is_named() {
        let env = env_with(&[("ANTHROPIC_API_KEY", "sk-default")]);
        let key = api_key_for(
            &Config::default(),
            "anthropic-key",
            "ANTHROPIC_API_KEY",
            &env,
        )
        .expect("a key was configured");
        assert_eq!(secrecy::ExposeSecret::expose_secret(&key), "sk-default");
    }

    /// The kind that could not be built at all before this: the one documented
    /// route to an endpoint IronWire does not discover.
    #[test]
    fn an_openai_compatible_entry_builds_a_backend_at_its_own_url() {
        let mut declared = entry("local", "openai-compatible");
        declared.base_url = Some("http://127.0.0.1:11434/v1".to_string());
        declared.api_key_env = Some("LOCAL_KEY".to_string());
        declared.models = Some(vec![ModelEntry::Name("qwen3-coder".to_string())]);

        let built = backend_from_config(&declared, 60, &env_with(&[("LOCAL_KEY", "sk-local")]))
            .expect("builds")
            .expect("a credential was available");
        assert_eq!(built.id().as_str(), "local");
        assert_eq!(
            built.models(),
            vec![("qwen3-coder".to_string(), ModelTier::Frontier)]
        );
    }

    /// A missing credential is a normal state, not an error — the same rule
    /// discovery follows.
    #[test]
    fn an_entry_whose_key_is_absent_is_skipped_rather_than_failing() {
        let mut declared = entry("local", "openai-compatible");
        declared.base_url = Some("http://127.0.0.1:11434/v1".to_string());
        declared.api_key_env = Some("LOCAL_KEY".to_string());

        let built = backend_from_config(&declared, 60, &env_with(&[])).expect("no error");
        assert!(built.is_none());
    }

    /// The landmine: a backend discovery produces on its own must still be
    /// switched off by config, or `enabled = false` is a no-op that reads like
    /// a kill switch.
    #[test]
    fn a_discovered_backend_can_be_disabled_by_config() {
        let mut config = Config::default();
        let mut declared = entry("claude-sub", "claude-subscription");
        declared.enabled = false;
        config.backends.push(declared);

        assert!(is_disabled(&config, "claude-sub"));
        assert!(
            !is_disabled(&config, "codex-sub"),
            "unrelated backends stay"
        );
        assert!(
            !is_disabled(&Config::default(), "claude-sub"),
            "a config with no entries disables nothing"
        );
    }

    #[test]
    fn a_configured_catalogue_is_read_for_any_backend_not_just_nearai() {
        let mut config = Config::default();
        let mut declared = entry("anthropic-key", "anthropic-api");
        declared.models = Some(vec![ModelEntry::Name("claude-haiku-4-5".to_string())]);
        config.backends.push(declared);

        assert_eq!(
            models_for(&config, "anthropic-key", BackendKind::ApiKey),
            Some(vec![("claude-haiku-4-5".to_string(), ModelTier::Fast)])
        );
        assert_eq!(
            models_for(&config, "nearai", BackendKind::Credits),
            None,
            "nothing configured is not the same as an empty catalogue"
        );
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[tokio::test]
    async fn the_host_can_observe_an_ended_task_and_read_the_result_again() {
        let task = tokio::spawn(async { Err(ExitError::Server) });
        let mut proxy = EmbeddedProxy {
            port: 1,
            report: StartupReport::default(),
            shutdown: None,
            task: Some(task),
            exit: None,
        };
        assert_eq!(proxy.wait().await, Err(ExitError::Server));
        assert!(proxy.is_finished());
        assert_eq!(proxy.wait().await, Err(ExitError::Server));
    }

    #[tokio::test]
    async fn canceling_a_wait_keeps_shutdown_ownership() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async {
            let _ = rx.await;
            Ok(())
        });
        let mut proxy = EmbeddedProxy {
            port: 1,
            report: StartupReport::default(),
            shutdown: Some(tx),
            task: Some(task),
            exit: None,
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), proxy.wait())
                .await
                .is_err()
        );
        assert!(!proxy.is_finished());
        proxy.request_shutdown();
        assert_eq!(proxy.wait().await, Ok(()));
    }

    #[tokio::test]
    async fn stopping_housekeeping_waits_until_its_captured_resources_drop() {
        struct Marker(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Marker {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let marker = Marker(dropped.clone());
        let mut tasks = Tasks(vec![tokio::spawn(async move {
            let _marker = marker;
            std::future::pending::<()>().await;
        })]);
        tasks.stop().await;
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn pointer_cleanup_leaves_another_owners_endpoint_alone() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("endpoint.json");
        let ours = ironwire_core::discovery::Endpoint::new(1, home.path().join("ours"));
        let theirs = ironwire_core::discovery::Endpoint::new(2, home.path().join("theirs"));
        let pointer = OwnedPointer {
            path: path.clone(),
            endpoint: ours,
        };
        ironwire_core::atomic::write(&path, &serde_json::to_string(&theirs).unwrap()).unwrap();
        drop(pointer);
        assert_eq!(
            ironwire_core::discovery::Endpoint::read_from(&path),
            Some(theirs)
        );
    }
}
