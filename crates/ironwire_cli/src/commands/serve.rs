//! `ironwire serve` — run the loopback daemon.

use std::sync::Arc;

use anyhow::{Context, Result};
use ironwire_core::config::{Config, PathsConfig};
use ironwire_core::protocol::ModelTier;
use ironwire_creds::ConsentLedger;
use ironwire_creds::claude::ClaudeCodeCredentials;
use ironwire_ledger::Ledger;
use ironwire_proxy::server::ServeError;
use ironwire_proxy::state::{AppState, BackendRegistry};
use ironwire_upstream::anthropic::AnthropicBackend;
use ironwire_upstream::openai_chat::ChatCompletionsBackend;
use secrecy::SecretString;

use super::{control_token, paths};

/// Start the daemon and serve until Ctrl-C.
pub(crate) async fn run(port_override: Option<u16>) -> Result<()> {
    let paths = paths()?;
    let config = Config::load(&paths).context("loading config.toml")?;
    let port = port_override.unwrap_or(config.server.port);
    let consent = ConsentLedger::load(&paths.consent_file());
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
    let state = AppState::new(registry, config, consent, token)
        .with_port(port)
        .with_ledger(ledger);

    println!("IronWire listening on http://127.0.0.1:{port}");
    println!("  Claude Code: export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}/anthropic");
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
            AnthropicBackend::subscription(base_url_for(config, "claude-sub"), timeout)
                .context("building the Claude subscription backend")?,
        ));
    }

    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
        && !key.is_empty()
    {
        registry.push(Arc::new(
            AnthropicBackend::api_key(
                SecretString::from(key),
                base_url_for(config, "anthropic-key"),
                timeout,
            )
            .context("building the Anthropic API backend")?,
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
                base_url_for(config, "nearai")
                    .or_else(|| std::env::var("IRONWIRE_NEARAI_BASE_URL").ok()),
                nearai_models(config),
                timeout,
            )
            .context("building the NEAR AI backend")?,
        ));
    }

    Ok(registry)
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
fn base_url_for(config: &Config, id: &str) -> Option<String> {
    config
        .backends
        .iter()
        .find(|b| b.id == id)
        .and_then(|b| b.base_url.clone())
        .or_else(|| std::env::var("IRONWIRE_ANTHROPIC_BASE_URL").ok())
        .filter(|url| !url.is_empty())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
