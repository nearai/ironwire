//! `ironwire serve` — run the loopback daemon.

use std::sync::Arc;

use anyhow::{Context, Result};
use ironwire_core::config::{Config, PathsConfig};
use ironwire_core::protocol::ModelTier;
use ironwire_creds::ConsentLedger;
use ironwire_creds::claude::ClaudeCodeCredentials;
use ironwire_creds::codex::{CodexCredentials, CodexMode};
use ironwire_ledger::Ledger;
use ironwire_proxy::server::ServeError;
use ironwire_proxy::state::{AppState, BackendRegistry};
use ironwire_quirks::QuirksStore;
use ironwire_upstream::anthropic::AnthropicBackend;
use ironwire_upstream::openai_chat::ChatCompletionsBackend;
use ironwire_upstream::openai_responses::ResponsesBackend;
use secrecy::SecretString;

use super::{control_token, paths};

/// Start the daemon and serve until Ctrl-C.
pub(crate) async fn run(port_override: Option<u16>) -> Result<()> {
    let paths = paths()?;
    let config = Config::load(&paths).context("loading config.toml")?;
    let port = port_override.unwrap_or(config.server.port);
    let consent = ConsentLedger::load(&paths.consent_file());
    let config_updates_enabled = config.updates.check;
    let token = control_token(&paths)?;

    let registry = build_registry(&config)?;
    if registry.is_empty() {
        eprintln!(
            "No backends are available.\n\
             Run `ironwire connect claude --subscription` or set ANTHROPIC_API_KEY."
        );
    }

    // Bind before announcing: printing "listening on 8463" and then failing to
    // bind sends people looking in entirely the wrong place.
    let listener = match ironwire_proxy::server::bind(port).await {
        Ok(listener) => listener,
        Err(ServeError::PortInUse { port }) => return Err(port_in_use(port).await),
        Err(other) => return Err(anyhow::Error::new(other).context("binding")),
    };

    let ledger = open_ledger(&paths, &config);
    // Provider values that may have been refreshed since this binary shipped.
    // A missing or untrusted document silently leaves the built-ins in force.
    let quirks = QuirksStore::load(ironwire_quirks::QUIRKS_PUBLIC_KEY, &paths.quirks_file());
    if quirks.serial() > 0 {
        println!("  provider quirks: serial {}", quirks.serial());
    }
    let state = AppState::new(registry, config, consent, token)
        .with_port(port)
        .with_ledger(ledger)
        .with_quirks(quirks);

    // Notify-only: check rarely, tell the user, never act. See docs/UPDATES.md.
    super::update::spawn_check(state.clone(), &paths, config_updates_enabled);
    // Provider quirks refresh while running, so a changed `anthropic-beta`
    // flag is a minutes-long fix rather than a restart (docs/UPDATES.md §2).
    super::quirks::spawn_refresh(state.clone(), &paths, config_updates_enabled);

    println!("IronWire listening on http://127.0.0.1:{port}");
    println!("  Claude Code: export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}/anthropic");
    println!("  Codex:       ironwire connect codex");
    println!();

    ironwire_proxy::server::serve_on(listener, state, shutdown_signal())
        .await
        .context("serving")
}

/// Open the trace ledger, or explain why we are running without one.
///
/// A ledger failure must never stop the proxy: the user's agent working matters
/// more than our bookkeeping.
fn open_ledger(paths: &PathsConfig, config: &Config) -> Option<Ledger> {
    if !config.capture.enabled {
        return None;
    }
    match Ledger::open(&paths.ledger_file()) {
        Ok(ledger) => Some(ledger),
        Err(error) => {
            eprintln!(
                "Warning: could not open the trace ledger ({error}). \
                 Routing continues; `ironwire log` will be empty."
            );
            None
        }
    }
}

/// Turn "address in use" into something a user can act on.
///
/// The common case by far is a second `ironwire serve`, and the second-most
/// common is an unrelated process squatting the port. Those need different
/// responses, and we can tell them apart by asking.
async fn port_in_use(port: u16) -> anyhow::Error {
    let health = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .ok();
    let is_ironwire = match health {
        Some(client) => client
            .get(format!("http://127.0.0.1:{port}/_ironwire/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success()),
        None => false,
    };

    if is_ironwire {
        anyhow::anyhow!(
            "IronWire is already running on port {port}.\n\
             Use it (`ironwire status`), or stop it and start again."
        )
    } else {
        anyhow::anyhow!(
            "Port {port} is in use by something that is not IronWire.\n\
             Pick another with `ironwire serve --port <n>`, and point your \
             clients at it with `ironwire env --port <n>`."
        )
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

    if ClaudeCodeCredentials::discover().is_ok() {
        registry.push(Arc::new(
            AnthropicBackend::subscription(
                base_url_for(config, "claude-sub", "IRONWIRE_ANTHROPIC_BASE_URL"),
                timeout,
            )
            .context("building the Claude subscription backend")?,
        ));
    }

    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
        && !key.is_empty()
    {
        registry.push(Arc::new(
            AnthropicBackend::api_key(
                SecretString::from(key),
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
        registry.push(Arc::new(
            ResponsesBackend::codex_subscription(
                base_url_for(config, "codex-sub", "IRONWIRE_CODEX_BASE_URL"),
                timeout,
            )
            .context("building the ChatGPT subscription backend")?,
        ));
    }

    // A key from the environment, or the one Codex itself stored — a user who
    // ran `codex login --api-key` has exactly one place they expect it to live.
    if let Some(key) = openai_api_key() {
        registry.push(Arc::new(
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
    if let Ok(key) = std::env::var("NEARAI_API_KEY")
        && !key.is_empty()
    {
        registry.push(Arc::new(
            ChatCompletionsBackend::nearai(
                SecretString::from(key),
                base_url_for(config, "nearai", "IRONWIRE_NEARAI_BASE_URL"),
                nearai_models(config),
                timeout,
            )
            .context("building the NEAR AI backend")?,
        ));
    }

    Ok(registry)
}

/// The metered OpenAI key, from the environment or from Codex's own store.
fn openai_api_key() -> Option<SecretString> {
    if let Ok(key) = std::env::var("OPENAI_API_KEY")
        && !key.is_empty()
    {
        return Some(SecretString::from(key));
    }
    let creds = CodexCredentials::discover().ok()?;
    (creds.mode == CodexMode::ApiKey).then(|| creds.bearer().token)
}

/// Models to offer from NEAR AI.
///
/// Configurable because the catalogue moves faster than our releases; the
/// default is one frontier-tier slug so a user with only a key still gets a
/// working fallback.
fn nearai_models(config: &Config) -> Vec<(String, ModelTier)> {
    config
        .backends
        .iter()
        .find(|b| b.id == "nearai")
        .and_then(|b| b.models.clone())
        .map_or_else(
            || vec![("deepseek-v3".to_string(), ModelTier::Frontier)],
            |models| {
                models
                    .into_iter()
                    .map(|m| {
                        let tier = ModelTier::from_model_hint(&m);
                        (m, tier)
                    })
                    .collect()
            },
        )
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

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
