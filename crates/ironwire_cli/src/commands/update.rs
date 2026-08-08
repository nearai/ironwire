//! `ironwire update` — and the daemon's background check.
//!
//! IronWire is **notify-only**. It never downloads or applies an update: it is
//! a daemon holding credentials in the middle of a streamed response, and
//! `docs/PROTOCOL.md` §5 says an interrupted stream past the first byte cannot
//! be recovered. A proxy that restarts itself unprompted would be causing the
//! outage it exists to prevent.
//!
//! So this module reports, and tells the user the command that belongs to
//! *their* install — the package manager's, when a package manager owns it.

use anyhow::Result;
use ironwire_core::config::PathsConfig;
use ironwire_proxy::state::AppState;
use ironwire_update::{CheckedAt, InstallMethod, Manifest, UpdateStatus};

/// Where the release manifest lives. Pinned rather than configurable: a
/// redirectable update source is a supply-chain hole for no benefit.
const MANIFEST_URL: &str = "https://ironwire.dev/releases/manifest.json";

/// Show what the running daemon knows, or check directly if it is not running.
pub(crate) async fn run(port: Option<u16>) -> Result<()> {
    let paths = super::paths()?;
    let install = install_method();

    // Prefer the daemon's cached answer so `ironwire update` costs no request.
    let status = match super::control_client::ControlClient::new(port) {
        Ok(client) => match client.status().await {
            Ok(status) if status.update != UpdateStatus::Unknown => status.update,
            _ => check_now(&paths, install).await,
        },
        Err(_) => check_now(&paths, install).await,
    };

    println!("ironwire {}", env!("CARGO_PKG_VERSION"));
    match status {
        UpdateStatus::UpToDate => println!("Up to date."),
        UpdateStatus::Unknown => {
            println!("Could not determine the latest release.");
            println!("Update checks may be disabled, or the network is unavailable.");
        }
        UpdateStatus::Available {
            latest,
            summary,
            upgrade_command,
        } => {
            println!("{latest} is available.");
            if let Some(summary) = summary {
                println!("  {summary}");
            }
            print_upgrade(install, upgrade_command.as_deref());
        }
        UpdateStatus::Unsupported {
            latest,
            minimum_supported,
            upgrade_command,
        } => {
            // Worth stronger words: below the floor, IronWire is likely broken
            // against current provider APIs rather than merely old.
            println!("This version is below the supported floor ({minimum_supported}).");
            println!("Providers may have changed in ways this build does not handle.");
            println!("{latest} is available.");
            print_upgrade(install, upgrade_command.as_deref());
        }
    }
    Ok(())
}

fn print_upgrade(install: InstallMethod, command: Option<&str>) {
    match command {
        Some(command) if install == InstallMethod::ShellInstaller => {
            println!("\n  {command}");
        }
        Some(command) => {
            // Deliberately the package manager's command: self-updating a
            // managed install desyncs it from its manager.
            println!("\n  {command}");
        }
        None => println!("\nThis looks like a local build — upgrade it the way you built it."),
    }
}

fn install_method() -> InstallMethod {
    std::env::current_exe()
        .map(|exe| InstallMethod::detect(&exe))
        .unwrap_or(InstallMethod::Unmanaged)
}

/// Fetch the manifest and evaluate it, caching the result.
async fn check_now(paths: &PathsConfig, install: InstallMethod) -> UpdateStatus {
    let Some(manifest) = fetch_manifest().await else {
        return UpdateStatus::Unknown;
    };
    let status = ironwire_update::evaluate(env!("CARGO_PKG_VERSION"), &manifest, install);
    let checked = CheckedAt {
        at: chrono::Utc::now(),
        status: status.clone(),
    };
    if let Err(error) = ironwire_update::save_cache(&paths.update_cache_file(), &checked) {
        tracing::debug!(%error, "could not cache the update check");
    }
    status
}

async fn fetch_manifest() -> Option<Manifest> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        // No install id, no per-check identifier, nothing about the user's
        // work: version and platform only (`docs/TRUST.md` §7).
        .user_agent(format!(
            "ironwire/{} ({})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS
        ))
        .build()
        .ok()?;
    client
        .get(MANIFEST_URL)
        .send()
        .await
        .ok()?
        .json::<Manifest>()
        .await
        .ok()
}

/// Start the daemon's background check.
///
/// Rate-limited to once a day, honours the kill switch, and never blocks
/// startup: a proxy that will not start because a release server is down would
/// be a worse failure than being out of date.
pub(crate) fn spawn_check(state: AppState, paths: &PathsConfig, enabled: bool) {
    let cache_path = paths.update_cache_file();
    let cached = ironwire_update::load_cache(&cache_path);

    // Whatever the last check concluded is worth showing immediately, even if
    // it is too soon to check again.
    if let Some(cached) = &cached {
        state.set_update_status(cached.status.clone());
    }
    if !ironwire_update::should_check(enabled, cached.as_ref(), chrono::Utc::now()) {
        return;
    }

    let paths = paths.clone();
    let install = install_method();
    tokio::spawn(async move {
        let status = check_now(&paths, install).await;
        if status.is_actionable() {
            tracing::info!(
                ?status,
                "a newer IronWire is available (run `ironwire update`)"
            );
        }
        state.set_update_status(status);
    });
}
