//! `ironwire connect` — wire a client up, and gate subscription backends
//! behind recorded consent.
//!
//! Two rules, both from `docs/TRUST.md`:
//!
//! - Every command prints the exact file it is about to modify *before*
//!   modifying it, and `--dry-run` shows the change without making it.
//! - A subscription backend stays off until the user answers a specific
//!   question in plain language, and the answer is recorded against the version
//!   of the question they were asked.

use std::io::Write;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use ironwire_core::DEFAULT_PORT;
use ironwire_creds::claude::ClaudeCodeCredentials;
use ironwire_creds::codex::CodexCredentials;
use ironwire_creds::consent::ConsentLedger;

use super::paths;

/// Run `ironwire connect <target>`.
pub(crate) fn run(
    target: &str,
    subscription: bool,
    dry_run: bool,
    port: Option<u16>,
) -> Result<()> {
    let port = port.unwrap_or(DEFAULT_PORT);
    match target {
        "claude" => connect_claude(subscription, dry_run, port),
        "codex" => connect_codex(subscription, dry_run, port),
        "anthropic-api" | "openai-api" | "near" => {
            bail!("`ironwire connect {target}` lands in M2 — see docs/ROADMAP.md")
        }
        other => bail!("unknown target `{other}` (try: claude, codex)"),
    }
}

/// Run `ironwire disconnect <target>`.
pub(crate) fn disconnect(target: &str, subscription: bool) -> Result<()> {
    if subscription {
        let paths = paths()?;
        let path = paths.consent_file();
        let mut ledger = ConsentLedger::load(&path);
        let backend = match target {
            "claude" => "claude-sub",
            "codex" => "codex-sub",
            other => bail!("unknown target `{other}`"),
        };
        ledger.revoke(backend);
        ledger.save(&path).context("writing the consent ledger")?;
        println!("Revoked consent for the {target} subscription backend.");
        println!("Restart the daemon for this to take effect: `ironwire serve`");
        return Ok(());
    }
    println!("Remove the IronWire settings you added for {target}:");
    match target {
        "claude" => println!("  unset ANTHROPIC_BASE_URL"),
        "codex" => println!("  remove [model_providers.ironwire] from ~/.codex/config.toml"),
        other => bail!("unknown target `{other}`"),
    }
    Ok(())
}

/// Print the environment a client needs.
pub(crate) fn print_env(port: Option<u16>) -> Result<()> {
    let port = port.unwrap_or(DEFAULT_PORT);
    println!("export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}/anthropic");
    Ok(())
}

fn connect_claude(subscription: bool, dry_run: bool, port: u16) -> Result<()> {
    println!("Claude Code → IronWire");
    println!();
    println!("Point Claude Code at IronWire by exporting this in your shell:");
    println!();
    println!("    export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}/anthropic");
    println!();
    println!("Add it to your shell profile to make it stick, or run:");
    println!();
    println!("    eval \"$(ironwire env)\"");
    println!();

    match ClaudeCodeCredentials::discover() {
        Ok(creds) => {
            let plan = creds.subscription_type.as_deref().unwrap_or("unknown plan");
            println!("Found a Claude Code login ({plan}) at {}.", creds.source);
        }
        Err(e) => {
            println!("No Claude Code login found: {e}");
            println!("IronWire can still route through ANTHROPIC_API_KEY.");
            return Ok(());
        }
    }

    if !subscription {
        println!();
        println!(
            "To let IronWire use that subscription, run:\n\
             \n    ironwire connect claude --subscription\n"
        );
        return Ok(());
    }

    if dry_run {
        println!();
        println!("[dry run] would record consent for the `claude-sub` backend.");
        return Ok(());
    }

    let paths = paths()?;
    let path = paths.consent_file();
    let mut ledger = ConsentLedger::load(&path);
    if ledger.is_granted("claude-sub") {
        println!();
        println!("The Claude subscription backend is already enabled.");
        return Ok(());
    }

    if !ask_subscription_consent(
        "Claude",
        "api.anthropic.com",
        "Anthropic",
        "an Anthropic API key",
    )? {
        println!("Left disabled. IronWire will use API keys only.");
        return Ok(());
    }

    ledger.grant("claude-sub", Utc::now());
    ledger.save(&path).context("writing the consent ledger")?;
    println!();
    println!("Enabled. Recorded in {}.", path.display());
    println!("Start the daemon with `ironwire serve`.");
    Ok(())
}

fn connect_codex(_subscription: bool, _dry_run: bool, port: u16) -> Result<()> {
    println!("Codex → IronWire");
    println!();
    println!("The OpenAI façade lands in M2 (see docs/ROADMAP.md). When it does,");
    println!("this command will add the following to ~/.codex/config.toml:");
    println!();
    println!("    model_provider = \"ironwire\"");
    println!();
    println!("    [model_providers.ironwire]");
    println!("    name = \"IronWire\"");
    println!("    base_url = \"http://127.0.0.1:{port}/openai/v1\"");
    println!("    wire_api = \"responses\"");
    println!();
    match CodexCredentials::discover() {
        Ok(creds) => println!(
            "Found a Codex login ({:?}) at {}.",
            creds.mode, creds.source
        ),
        Err(e) => println!("No Codex login found: {e}"),
    }
    Ok(())
}

/// The consent prompt. `docs/TRUST.md` §2 fixes its content; changing what it
/// *means* requires bumping `CONSENT_PROMPT_VERSION`, which invalidates
/// consent given to the old wording.
fn ask_subscription_consent(
    product: &str,
    host: &str,
    vendor: &str,
    alternative: &str,
) -> Result<bool> {
    println!();
    println!("  IronWire will read the OAuth token that {product} Code stores on this");
    println!("  machine and send requests to {host} with it, from this computer only.");
    println!();
    println!("  · This uses a private authentication path. {vendor} does not document");
    println!("    it and may change or block it at any time.");
    println!("  · Using it from a third-party proxy may fall outside your subscription's");
    println!("    intended use. If {vendor} objects, it is your account that is affected.");
    println!("  · Your token is never sent anywhere except {host}.");
    println!("  · You can use {alternative} instead — fully supported, no ambiguity.");
    println!();
    print!("  Enable the {product} subscription backend? [y/N] ");
    std::io::stdout().flush().ok();

    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("reading your answer")?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
