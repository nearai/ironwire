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
        // Both settings are in a file we wrote, so both are ours to take back
        // out — exactly ours, and nothing the user put there themselves.
        "claude" => unwire_claude(),
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

    wire_claude(port, dry_run)?;

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

    wire_codex(port, dry_run)?;
    println!();

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
    write_with_backup(&path, &existing, &edit.contents, "toml")?;
    println!();
    println!("Written. Restart Codex to pick it up.");
    Ok(())
}

/// Point Claude Code at IronWire, in the file Claude Code already reads.
///
/// Two slots, one write:
///
/// - `env.ANTHROPIC_BASE_URL`, which is what actually routes it here. This used
///   to be an `export` line we printed for the user to run, which meant the
///   setup did not survive a new terminal and `doctor` spent its life
///   explaining that. A setting in the file is the same decision, made once.
/// - `statusLine`, IronWire's one line of screen space. IronWire will not write
///   into a response stream, so without this the only place it can say "your
///   traffic just moved" is a second terminal nobody is looking at.
///
/// Neither is taken from a user already using it — see [`crate::claude_settings`].
pub(crate) fn wire_claude(port: u16, dry_run: bool) -> Result<()> {
    let path = claude_settings_path()?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let command = format!("{} statusline", our_binary()?);
    let url = anthropic_url(port);
    let edit = claude_settings::connect(&existing, &command, Some(&url)).with_context(|| {
        format!(
            "{} is not valid JSON — IronWire will not rewrite a file it cannot read",
            path.display()
        )
    })?;

    if !edit.is_noop() {
        // TRUST.md: name the file before touching it, every time.
        println!("Writing {}:", path.display());
        for change in &edit.changes {
            println!("  · {change}");
        }
        if dry_run {
            println!("  [dry run] nothing was written.");
        } else {
            write_with_backup(&path, &existing, &edit.contents, "json")?;
            println!("  Claude Code picks this up on its next start.");
        }
    }

    // A slot of their own is not a failure, but it does mean the thing we
    // promised did not happen — so say what would make it happen by hand.
    if let Some(theirs) = edit.occupied_slot("ANTHROPIC_BASE_URL") {
        println!("  ANTHROPIC_BASE_URL is already set to `{theirs}`, so IronWire left it.");
        println!("  To route Claude Code here instead, set it to: {url}");
    }
    if let Some(theirs) = edit.occupied_slot("statusLine") {
        println!("  You already have a status line (`{theirs}`), so IronWire left it alone.");
        println!("  To include IronWire in it, add the output of: {command}");
    }
    Ok(())
}

/// Point Codex at IronWire, in the config file Codex already reads.
pub(crate) fn wire_codex(port: u16, dry_run: bool) -> Result<()> {
    let path = codex_config_path()?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let edit = codex_config::connect(&existing, port).with_context(|| {
        format!(
            "{} is not valid TOML — IronWire will not append to a file it cannot read",
            path.display()
        )
    })?;
    if edit.is_noop() {
        return Ok(());
    }

    println!("Writing {}:", path.display());
    for change in &edit.changes {
        println!("  · {change}");
    }
    // Said before the edit, like every other change in this file. This one is
    // not IronWire's limitation, but IronWire is what makes the user meet it,
    // and finding out afterwards — with a thread stuck on a model and no UI to
    // change it — is the worst way to learn.
    println!();
    println!("  One consequence worth knowing: Codex has no UI for changing the");
    println!("  model on a custom provider (openai/codex#15364). A desktop thread");
    println!("  keeps whatever model it was created with, and the CLI needs `-m`");
    println!("  or `model =` in config.toml. `ironwire disconnect codex` puts it");
    println!("  all back.");
    println!();
    if dry_run {
        println!("  [dry run] nothing was written.");
        return Ok(());
    }
    write_with_backup(&path, &existing, &edit.contents, "toml")?;
    // One file drives both clients, and "restart" means different things to each.
    println!("  CLI: start a new `codex` session.");
    println!("  Desktop: quit the app and relaunch it — reopening a window is not");
    println!("  enough, it keeps the old config.");
    Ok(())
}

/// The endpoint an Anthropic-speaking client points at.
fn anthropic_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/anthropic")
}

/// Write a file we did not create, keeping a copy of what was there.
///
/// The backup is what makes an automatic edit defensible: the user can always
/// get back exactly what they had, without us having to be right about what
/// mattered in it.
fn write_with_backup(
    path: &std::path::Path,
    existing: &str,
    contents: &str,
    extension: &str,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if !existing.is_empty() {
        let backup = path.with_extension(format!("{extension}.ironwire-backup"));
        std::fs::write(&backup, existing)
            .with_context(|| format!("writing {}", backup.display()))?;
        println!("  (previous contents saved to {})", backup.display());
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

/// Whether Claude Code is on this machine at all.
///
/// Either signal is enough: the config directory means it has run here, and the
/// binary means it can. Wiring an agent that is not installed writes settings
/// nothing will read, which is how a setup step turns into litter.
pub(crate) fn claude_installed() -> bool {
    claude_settings_path().is_ok_and(|path| path.parent().is_some_and(std::path::Path::exists))
        || on_path("claude")
}

/// Whether Codex is on this machine at all.
pub(crate) fn codex_installed() -> bool {
    codex_config_path().is_ok_and(|path| path.parent().is_some_and(std::path::Path::exists))
        || on_path("codex")
}

/// Whether an executable of this name is reachable on `PATH`.
fn on_path(name: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file() || candidate.with_extension("exe").is_file()
    })
}

/// Undo [`wire_claude`], removing only what it added.
fn unwire_claude() -> Result<()> {
    let path = claude_settings_path()?;
    let Ok(existing) = std::fs::read_to_string(&path) else {
        println!("{} does not exist — nothing to undo.", path.display());
        return Ok(());
    };
    let edit = claude_settings::disconnect(&existing).with_context(|| {
        format!(
            "{} is not valid JSON — IronWire will not rewrite a file it cannot read",
            path.display()
        )
    })?;
    if edit.is_noop() {
        println!("{} has nothing of IronWire's in it.", path.display());
        return Ok(());
    }
    println!("Writing {}:", path.display());
    for change in &edit.changes {
        println!("  · {change}");
    }
    write_with_backup(&path, &existing, &edit.contents, "json")?;
    println!("  Claude Code goes back to Anthropic on its next start.");
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
