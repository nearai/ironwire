//! `ironwire serve` — run the loopback daemon.

use std::sync::Arc;

use anyhow::{Context, Result};
use ironwire_core::config::Config;
use ironwire_creds::ConsentLedger;
use ironwire_creds::claude::ClaudeCodeCredentials;
use ironwire_proxy::state::{AppState, BackendRegistry};
use ironwire_upstream::anthropic::AnthropicBackend;
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

    let state = AppState::new(registry, config, consent, token).with_port(port);

    println!("IronWire listening on http://127.0.0.1:{port}");
    println!("  Claude Code: export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}/anthropic");
    println!();

    ironwire_proxy::serve(state, port, shutdown_signal())
        .await
        .context("serving")?;
    Ok(())
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

    Ok(registry)
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
