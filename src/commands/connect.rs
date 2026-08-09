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

use crate::claude_settings;
use crate::codex_config;

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
        "near" => connect_near(port),
        "anthropic-api" => connect_api_key(
            "Anthropic",
            "ANTHROPIC_API_KEY",
            "https://console.anthropic.com/settings/keys",
        ),
        "openai-api" => connect_api_key(
            "OpenAI",
            "OPENAI_API_KEY",
            "https://platform.openai.com/api-keys",
        ),
        other => {
            bail!("unknown target `{other}`\n\ntry: claude, codex, near, anthropic-api, openai-api")
        }
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
    match target {
        // Claude Code is pointed here by an environment variable, which is the
        // user's shell to own — we can only say what to remove.
        "claude" => {
            println!("Remove the IronWire setting you added for Claude Code:");
            println!("  unset ANTHROPIC_BASE_URL");
            println!("  (and remove it from your shell profile)");
            // The status line, unlike the variable, is in a file we wrote — so
            // it is ours to take back out.
            remove_status_line()
        }
        // Codex is pointed here by a file we wrote, so we can undo it.
        "codex" => disconnect_codex(),
        other => bail!("unknown target `{other}`"),
    }
}

/// Print the environment a client needs.
pub(crate) fn print_env(port: Option<u16>, shell: Option<String>) -> Result<()> {
    let port = port.unwrap_or(DEFAULT_PORT);
    let url = format!("http://127.0.0.1:{port}/anthropic");

    // Default to the shell the user is actually running, not to bash. Someone
    // in fish who pipes `export FOO=bar` into `eval` gets a syntax error and no
    // idea why, which is a miserable first five minutes.
    let shell = shell.unwrap_or_else(|| {
        std::env::var("SHELL")
            .ok()
            .and_then(|path| {
                std::path::Path::new(&path)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| "sh".to_string())
    });

    match shell.as_str() {
        "fish" => println!("set -gx ANTHROPIC_BASE_URL {url}"),
        "powershell" | "pwsh" => println!("$env:ANTHROPIC_BASE_URL = \"{url}\""),
        "cmd" => println!("set ANTHROPIC_BASE_URL={url}"),
        "nu" | "nushell" => println!("$env.ANTHROPIC_BASE_URL = \"{url}\""),
        // bash, zsh, ksh, dash, sh, and anything else POSIX-ish.
        _ => println!("export ANTHROPIC_BASE_URL={url}"),
    }
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

    install_status_line(dry_run)?;

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

fn connect_codex(subscription: bool, dry_run: bool, port: u16) -> Result<()> {
    println!("Codex → IronWire");
    println!();

    let path = codex_config_path()?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let edit = codex_config::connect(&existing, port).with_context(|| {
        format!(
            "{} is not valid TOML — IronWire will not append to a file it cannot read",
            path.display()
        )
    })?;

    // TRUST.md: name the file before touching it, every time.
    if edit.is_noop() {
        println!("{} already points at IronWire.", path.display());
    } else {
        println!("This will change {}:", path.display());
        for change in &edit.changes {
            println!("  · {change}");
        }
        println!();
        if dry_run {
            println!("[dry run] nothing was written.");
        } else {
            write_codex_config(&path, &existing, &edit.contents)?;
            println!("Written. Restart Codex to pick it up.");
        }
        println!();
    }

    match CodexCredentials::discover() {
        Ok(creds) => println!(
            "Found a Codex login ({:?}) at {}.",
            creds.mode, creds.source
        ),
        Err(e) => {
            println!("No Codex login found: {e}");
            println!("IronWire can still route Codex through OPENAI_API_KEY.");
            return Ok(());
        }
    }

    if !subscription {
        println!();
        println!(
            "To let IronWire use that ChatGPT subscription, run:\n\
             \n    ironwire connect codex --subscription\n"
        );
        return Ok(());
    }

    if dry_run {
        println!();
        println!("[dry run] would record consent for the `codex-sub` backend.");
        return Ok(());
    }

    let paths = paths()?;
    let consent_path = paths.consent_file();
    let mut ledger = ConsentLedger::load(&consent_path);
    if ledger.is_granted("codex-sub") {
        println!();
        println!("The ChatGPT subscription backend is already enabled.");
        return Ok(());
    }

    if !ask_subscription_consent("Codex", "chatgpt.com", "OpenAI", "an OpenAI API key")? {
        println!("Left disabled. IronWire will use API keys only.");
        return Ok(());
    }

    ledger.grant("codex-sub", Utc::now());
    ledger
        .save(&consent_path)
        .context("writing the consent ledger")?;
    println!();
    println!("Enabled. Recorded in {}.", consent_path.display());
    println!("Start the daemon with `ironwire serve`.");
    Ok(())
}

fn disconnect_codex() -> Result<()> {
    let path = codex_config_path()?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.is_empty() {
        println!("{} does not exist — nothing to undo.", path.display());
        return Ok(());
    }
    let edit = codex_config::disconnect(&existing)
        .with_context(|| format!("{} is not valid TOML", path.display()))?;
    if edit.is_noop() {
        println!("{} does not point at IronWire.", path.display());
        return Ok(());
    }
    println!("This will change {}:", path.display());
    for change in &edit.changes {
        println!("  · {change}");
    }
    write_codex_config(&path, &existing, &edit.contents)?;
    println!();
    println!("Written. Restart Codex to pick it up.");
    Ok(())
}

/// Write the config, keeping a copy of what was there.
///
/// The backup is not ceremony: this is a file the user edits by hand, and the
/// cost of being wrong about their config is an afternoon of theirs.
fn write_codex_config(path: &std::path::Path, existing: &str, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if !existing.is_empty() {
        let backup = path.with_extension("toml.ironwire-backup");
        std::fs::write(&backup, existing)
            .with_context(|| format!("writing {}", backup.display()))?;
        println!("  (previous contents saved to {})", backup.display());
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

/// Offer Claude Code's status line as IronWire's one line of screen space.
///
/// IronWire will not write into a response stream, so without this the only
/// place it can say "your traffic just moved" is a second terminal nobody is
/// looking at (`ironwire watch`). The status line is the harness's own
/// furniture, outside the transcript, and it is the honest channel.
fn install_status_line(dry_run: bool) -> Result<()> {
    let path = claude_settings_path()?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let command = format!("{} statusline", our_binary()?);
    let edit = claude_settings::connect(&existing, &command).with_context(|| {
        format!(
            "{} is not valid JSON — IronWire will not rewrite a file it cannot read",
            path.display()
        )
    })?;

    if let Some(theirs) = &edit.occupied_by {
        println!("You already have a status line (`{theirs}`), so IronWire left it alone.");
        println!("To include IronWire in it, add the output of:");
        println!();
        println!("    {command}");
        println!();
        return Ok(());
    }
    if edit.is_noop() {
        return Ok(());
    }

    println!("This will change {}:", path.display());
    for change in &edit.changes {
        println!("  · {change}");
    }
    if dry_run {
        println!("[dry run] nothing was written.");
        println!();
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if !existing.is_empty() {
        let backup = path.with_extension("json.ironwire-backup");
        std::fs::write(&backup, &existing)
            .with_context(|| format!("writing {}", backup.display()))?;
        println!("  (previous contents saved to {})", backup.display());
    }
    std::fs::write(&path, &edit.contents).with_context(|| format!("writing {}", path.display()))?;
    println!("Written. Claude Code picks it up on its next start.");
    println!();
    Ok(())
}

/// Undo [`install_status_line`], removing only what it added.
fn remove_status_line() -> Result<()> {
    let path = claude_settings_path()?;
    let Ok(existing) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let edit = claude_settings::disconnect(&existing).with_context(|| {
        format!(
            "{} is not valid JSON — IronWire will not rewrite a file it cannot read",
            path.display()
        )
    })?;
    if edit.is_noop() {
        return Ok(());
    }
    std::fs::write(&path, &edit.contents).with_context(|| format!("writing {}", path.display()))?;
    for change in &edit.changes {
        println!("  · {change}");
    }
    Ok(())
}

/// This binary's own path, so the status line keeps working for someone who
/// installed IronWire somewhere that is not on `PATH` — which is most people
/// running it from a build.
fn our_binary() -> Result<String> {
    let path = std::env::current_exe().context("locating the ironwire binary")?;
    Ok(path.display().to_string())
}

/// Where Claude Code keeps its settings. `CLAUDE_CONFIG_DIR` wins, as it does
/// for Claude Code.
fn claude_settings_path() -> Result<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR")
        && !dir.is_empty()
    {
        return Ok(std::path::PathBuf::from(dir).join("settings.json"));
    }
    let home = dirs::home_dir().context("could not locate your home directory")?;
    Ok(home.join(".claude").join("settings.json"))
}

/// Where Codex keeps its config. `CODEX_HOME` wins, as it does for Codex.
fn codex_config_path() -> Result<std::path::PathBuf> {
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.is_empty()
    {
        return Ok(std::path::PathBuf::from(home).join("config.toml"));
    }
    let home = dirs::home_dir().context("could not locate your home directory")?;
    Ok(home.join(".codex").join("config.toml"))
}

/// NEAR AI credits — the cross-family fallback lane.
fn connect_near(port: u16) -> Result<()> {
    println!("NEAR AI → IronWire");
    println!();

    match std::env::var("NEARAI_API_KEY") {
        Ok(key) if !key.is_empty() => {
            // Never the key itself, here or anywhere (`docs/TRUST.md` §5).
            println!(
                "Found NEARAI_API_KEY in your environment ({} chars).",
                key.len()
            );
            println!();
            println!("Restart the daemon to pick it up, then check it end to end:");
            println!();
            println!("    ironwire serve --port {port}");
            println!("    ironwire doctor");
        }
        _ => {
            println!("No NEARAI_API_KEY found.");
            println!();
            println!("Get a key at https://app.near.ai, then:");
            println!();
            println!("    export NEARAI_API_KEY=...");
            println!("    ironwire serve");
        }
    }

    println!();
    println!("NEAR AI is a different API family, so IronWire reaches it through");
    println!("the translated lane — and only at a turn boundary, never mid tool");
    println!("loop (docs/PROTOCOL.md §6). When a conversation moves there, your");
    println!("agent is talking to a different model family; `ironwire watch`");
    println!("tells you the moment it happens.");
    println!();
    println!("Device-key enrolment and trace-contribution credits land with M6");
    println!("(docs/ROADMAP.md). Today the key is all that is needed.");
    Ok(())
}

/// A metered API key. Nothing to write — IronWire keeps no secrets in
/// `config.toml` (`docs/TRUST.md` §5) — so this explains and verifies.
fn connect_api_key(vendor: &str, env: &str, url: &str) -> Result<()> {
    println!("{vendor} API key → IronWire");
    println!();
    match std::env::var(env) {
        Ok(key) if !key.is_empty() => {
            println!("Found {env} in your environment ({} chars).", key.len());
            println!();
            println!("Restart the daemon to pick it up:");
            println!();
            println!("    ironwire serve");
            println!("    ironwire doctor");
        }
        _ => {
            println!("No {env} found.");
            println!();
            println!("Get a key at {url}, then:");
            println!();
            println!("    export {env}=...");
            println!("    ironwire serve");
        }
    }
    println!();
    println!("IronWire reads this from the environment and never writes it to");
    println!("disk. There is no `ironwire` config field that holds a secret.");
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
