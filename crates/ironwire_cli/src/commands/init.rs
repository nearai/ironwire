//! `ironwire init` — the first two minutes.
//!
//! Everything this does can be done with the other commands. What it adds is
//! *ordering*: a new user should not have to read `--help` to learn that
//! connect comes before serve, or that a subscription needs a separate consent
//! step. Discovering that by trial and error is where first impressions go.
//!
//! It is deliberately not a wizard that changes things behind your back. It
//! looks at what is on the machine, says what IronWire could do with it, and
//! prints the commands — each of which is one the user could have found
//! themselves. `--write` is the only mode that touches anything, and even then
//! it never grants a subscription consent: that question has to be asked in its
//! own words (`docs/TRUST.md` §2).

use anyhow::{Context, Result};
use ironwire_core::DEFAULT_PORT;
use ironwire_core::config::Config;
use ironwire_creds::claude::ClaudeCodeCredentials;
use ironwire_creds::codex::{CodexCredentials, CodexMode};
use ironwire_creds::consent::ConsentLedger;

use super::paths;

/// What IronWire found on this machine.
struct Found {
    claude_subscription: bool,
    codex_subscription: bool,
    codex_api_key: bool,
    anthropic_key: bool,
    openai_key: bool,
    nearai_key: bool,
}

impl Found {
    fn detect() -> Self {
        let codex = CodexCredentials::discover().ok();
        Self {
            claude_subscription: ClaudeCodeCredentials::discover().is_ok(),
            codex_subscription: codex.as_ref().is_some_and(|c| c.mode == CodexMode::ChatGpt),
            codex_api_key: codex.is_some_and(|c| c.mode == CodexMode::ApiKey),
            anthropic_key: has_env("ANTHROPIC_API_KEY"),
            openai_key: has_env("OPENAI_API_KEY"),
            nearai_key: has_env("NEARAI_API_KEY"),
        }
    }

    fn anything(&self) -> bool {
        self.claude_subscription
            || self.codex_subscription
            || self.codex_api_key
            || self.anthropic_key
            || self.openai_key
            || self.nearai_key
    }
}

fn has_env(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.trim().is_empty())
}

/// Say what was found, and what each thing is worth.
fn report(found: &Found) {
    let line = |present: bool, what: &str, note: &str| {
        if present {
            println!("  found    {what:<28}{note}");
        }
    };
    line(
        found.claude_subscription,
        "Claude Code login",
        "capacity you have already paid for",
    );
    line(
        found.codex_subscription,
        "ChatGPT login (Codex)",
        "capacity you have already paid for",
    );
    line(found.codex_api_key, "OpenAI key (via Codex)", "metered");
    line(found.anthropic_key, "ANTHROPIC_API_KEY", "metered");
    line(found.openai_key, "OPENAI_API_KEY", "metered");
    line(
        found.nearai_key,
        "NEARAI_API_KEY",
        "credits; cross-family fallback",
    );

    if !found.anything() {
        println!("  nothing yet");
    }
}

/// Run `ironwire init`.
pub(crate) fn run(port: Option<u16>, write: bool) -> Result<()> {
    let paths = paths()?;
    let port = port
        .or_else(|| Config::load(&paths).ok().map(|c| c.server.port))
        .unwrap_or(DEFAULT_PORT);

    println!("IronWire — one local endpoint for the AI capacity you already have");
    println!();
    println!("Looking at this machine…");
    println!();

    let found = Found::detect();
    report(&found);

    if !found.anything() {
        println!();
        println!("Nothing found yet. Any one of these is enough to start:");
        println!();
        println!("    claude                      log in to Claude Code");
        println!("    codex login                 log in to Codex");
        println!("    export ANTHROPIC_API_KEY=…  a metered key");
        println!("    export NEARAI_API_KEY=…     NEAR AI credits");
        println!();
        println!("Then run `ironwire init` again.");
        return Ok(());
    }

    let consent = ConsentLedger::load(&paths.consent_file());
    println!();
    println!("Next:");
    println!();

    let mut step = 1;
    if found.claude_subscription && !consent.is_granted("claude-sub") {
        // Never done for the user, not even with `--write`. The consent prompt
        // is the whole mechanism, and a command that grants it silently would
        // make the prompt decorative (`docs/TRUST.md` §2).
        println!("  {step}. ironwire connect claude --subscription");
        println!("     Asks whether IronWire may use your Claude subscription. It");
        println!("     explains the risk first; you can say no and use a key instead.");
        println!();
        step += 1;
    }
    if found.codex_subscription && !consent.is_granted("codex-sub") {
        println!("  {step}. ironwire connect codex --subscription");
        println!("     The same question for ChatGPT.");
        println!();
        step += 1;
    }

    println!("  {step}. ironwire serve");
    println!("     Runs in the foreground on 127.0.0.1:{port}. Leave it running.");
    println!();
    step += 1;

    println!("  {step}. In another terminal, point your agent at it:");
    println!();
    println!("       eval \"$(ironwire env)\"     # Claude Code");
    println!("       ironwire connect codex       # Codex");
    println!();
    step += 1;

    println!("  {step}. ironwire doctor");
    println!("     Confirms your agents are actually pointed here — the thing");
    println!("     that is easy to get wrong and hard to notice.");
    println!();

    if write {
        write_config(&paths, port)?;
    } else {
        println!("Optional: `ironwire init --write` drops a commented config.toml at");
        println!(
            "{} so the settings are discoverable.",
            paths.config_file().display()
        );
    }

    println!();
    println!("Once it is running:");
    println!("    ironwire status    what capacity you have, and what is left");
    println!("    ironwire watch     live routing, quiet unless something changes");
    println!("    ironwire log       what your agents actually sent, and what it cost");
    Ok(())
}

/// Write a commented `config.toml`, if there is not one already.
fn write_config(paths: &ironwire_core::config::PathsConfig, port: u16) -> Result<()> {
    let path = paths.config_file();
    if path.exists() {
        println!("{} already exists — leaving it alone.", path.display());
        return Ok(());
    }

    let template = config_template(port);
    std::fs::write(&path, template).with_context(|| format!("writing {}", path.display()))?;
    println!("Wrote {}", path.display());
    Ok(())
}

/// The commented default config.
///
/// Every value in it *is* a default. The file exists to make the settings
/// discoverable, not to change behaviour — a generated config that silently
/// differs from the built-in defaults is a trap, and the test below is what
/// keeps the two from drifting apart.
fn config_template(port: u16) -> String {
    format!(
        r#"# IronWire configuration. Every value below is the default; the file
# exists so the settings are discoverable, not to change anything.

[server]
port = {port}
# Idle timeout for a single upstream request. Long, because coding agents
# legitimately generate for many minutes.
upstream_timeout_secs = 900

[capture]
# Local trace ledger: what `ironwire log` reads. Metadata only.
enabled = true
# Request and response bodies. Off by default — these contain your source.
bodies = false
# Days of history to keep. Pruned daily by the daemon; 0 keeps everything,
# which is a real choice but not a good default for a file nobody watches.
retain_days = 90

[resilience]
# Emit an SSE ping after this many seconds of upstream silence, so a client
# whose patience is shorter than the model's thinking time does not give up.
keepalive_secs = 15
# Give up on a silent upstream after this long and end the stream with a
# stated error rather than pinging into the void.
stall_timeout_secs = 180
# Compaction turns send the whole conversation and think for far longer.
compaction_stall_timeout_secs = 600
max_reconnects = 2

[updates]
# Check for a newer release, at most once a day. IronWire never applies one.
check = true

[privacy]
# Optional. Substitutes values on the way out and restores them on the way
# back, so a provider never sees them. Off by default: it is the one thing
# here that modifies your requests.
#
# See `ironwire privacy check <file>` to find out what it would actually
# catch before you rely on it, and docs/PRIVACY.md for what it cannot.
enabled = false
# API keys, tokens and private keys, by shape.
secrets = true
# Exact strings to substitute — your employer, a customer's domain. Note that
# listing them here writes them to this file.
named_values = []
# Values inside fenced code blocks and tool results are left alone by default:
# in a coding session they are almost always the code being edited, and
# substituting them makes the model write something that does not work.
scan_code_blocks = false
scan_tool_results = false
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generated_config_parses_and_equals_the_built_in_defaults() {
        // The trap this guards against: a template that drifts from the
        // defaults turns "here is a file documenting what IronWire does" into
        // "here is a file that quietly changes what IronWire does".
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ironwire_core::config::PathsConfig::rooted_at(dir.path());
        std::fs::write(paths.config_file(), config_template(DEFAULT_PORT)).expect("write");

        let loaded = Config::load(&paths).expect("the generated config parses");
        assert_eq!(loaded, Config::default());
    }

    #[test]
    fn the_generated_config_carries_the_port_it_was_given() {
        let template = config_template(9999);
        assert!(template.contains("port = 9999"));
    }

    #[test]
    fn the_privacy_filter_is_off_in_the_generated_config() {
        // `docs/TRUST.md` I7: it modifies requests, so it is never on unless
        // the user turned it on — including in a file we wrote for them.
        let template = config_template(DEFAULT_PORT);
        let start = template.find("[privacy]").expect("has a privacy section");
        assert!(
            template[start..].contains("enabled = false"),
            "the generated config would have enabled the filter"
        );
    }
}
