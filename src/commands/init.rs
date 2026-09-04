//! `ironwire init` — set IronWire up, in one command.
//!
//! It looks at what is on the machine, asks the one question that has to be
//! asked, wires up every agent it finds, and leaves the daemon running. When it
//! returns, `claude` works.
//!
//! It used to print the commands instead of running them — five of them, plus
//! an `export` and a second terminal — on the reasoning that a setup step the
//! user runs themselves is a setup step they consented to. That reasoning is
//! right about exactly one thing, subscription credentials (`docs/TRUST.md`
//! §2), and wrong about the rest: writing a settings file we then name, or
//! starting a daemon the user just asked for, is not a decision they need to
//! make twice. So the consent gate stays, as one prompt, and everything else
//! happens.
//!
//! `--dry-run` shows every change without making one.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use ironwire_core::DEFAULT_PORT;
use ironwire_core::config::Config;
use ironwire_creds::claude::ClaudeCodeCredentials;
use ironwire_creds::codex::{CodexCredentials, CodexMode};
use ironwire_creds::consent::ConsentLedger;

use super::{connect, paths, service};

/// A key an agent on this machine is already configured to use.
///
/// Worth finding because it is capacity the user already has, and worth
/// reporting separately because IronWire reads keys from the *daemon's*
/// environment: a key that lives only in an agent's config file is one the
/// daemon cannot see, and that gap is invisible until a request fails.
struct AgentKey {
    /// Which agent's config it came out of.
    agent: &'static str,
    /// The variable it names. Never the value — `docs/TRUST.md` §5.
    var: String,
    /// Whether the daemon will actually be able to read it.
    in_environment: bool,
}

/// What IronWire found on this machine.
struct Found {
    claude_subscription: bool,
    codex_subscription: bool,
    codex_api_key: bool,
    anthropic_key: bool,
    openai_key: bool,
    nearai_key: bool,
    agent_keys: Vec<AgentKey>,
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
            agent_keys: detect_agent_keys(),
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

    /// Subscriptions that are here but not yet consented to.
    fn ungranted_subscriptions(&self, consent: &ConsentLedger) -> Vec<Subscription> {
        SUBSCRIPTIONS
            .iter()
            .filter(|s| (s.present)(self) && !consent.is_granted(s.backend_id))
            .copied()
            .collect()
    }
}

/// One subscription backend, and how to talk about it.
#[derive(Clone, Copy)]
struct Subscription {
    backend_id: &'static str,
    /// What the user calls the thing they pay for.
    product: &'static str,
    /// The app that stored the token we would be replaying.
    client: &'static str,
    /// The only host its token is ever sent to.
    host: &'static str,
    /// Who would object.
    vendor: &'static str,
    /// The unambiguous alternative.
    alternative: &'static str,
    present: fn(&Found) -> bool,
}

const SUBSCRIPTIONS: &[Subscription] = &[
    Subscription {
        backend_id: "claude-sub",
        product: "Claude",
        client: "Claude Code",
        host: "api.anthropic.com",
        vendor: "Anthropic",
        alternative: "an Anthropic API key",
        present: |f| f.claude_subscription,
    },
    Subscription {
        backend_id: "codex-sub",
        product: "ChatGPT",
        client: "Codex",
        host: "chatgpt.com",
        vendor: "OpenAI",
        alternative: "an OpenAI API key",
        present: |f| f.codex_subscription,
    },
];

/// Join names the way a sentence would: "a", "a and b", "a, b and c".
///
/// The prompt reads out loud to someone deciding whether to hand over a
/// credential, so it has to be a sentence rather than a comma-separated list.
/// `conjunction` because a list of things we would use and a list of things
/// they could use instead are not the same kind of list.
fn join<'a>(items: impl Iterator<Item = &'a str>, conjunction: &str) -> String {
    let items: Vec<_> = items.collect();
    match items.split_last() {
        None => String::new(),
        Some((last, [])) => (*last).to_string(),
        Some((last, rest)) => format!("{} {conjunction} {last}", rest.join(", ")),
    }
}

fn has_env(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.trim().is_empty())
}

/// Read the keys the installed agents are already configured with.
///
/// Detection only. Copying a secret out of one config and into another — or
/// into a service unit, which is where this would naturally lead — would put
/// IronWire in the business of storing keys, which it is not (`docs/TRUST.md`
/// §5).
fn detect_agent_keys() -> Vec<AgentKey> {
    let mut keys = Vec::new();
    if let Some(path) = claude_settings_path() {
        keys.extend(claude_settings_keys(&path));
    }
    if let Some(path) = codex_config_path() {
        keys.extend(codex_config_keys(&path));
    }
    keys.sort_by(|a, b| a.var.cmp(&b.var));
    keys.dedup_by(|a, b| a.var == b.var);
    keys
}

/// Variables named in Claude Code's `env` block.
fn claude_settings_keys(path: &std::path::Path) -> Vec<AgentKey> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    root.get("env")
        .and_then(serde_json::Value::as_object)
        .map(|env| {
            env.keys()
                .filter(|name| looks_like_a_key(name))
                .map(|name| AgentKey {
                    agent: "Claude Code",
                    var: name.clone(),
                    in_environment: has_env(name),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Variables named by `env_key` under Codex's `[model_providers]`.
fn codex_config_keys(path: &std::path::Path) -> Vec<AgentKey> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    table
        .get("model_providers")
        .and_then(toml::Value::as_table)
        .map(|providers| {
            providers
                .values()
                .filter_map(|provider| provider.get("env_key")?.as_str())
                .map(|name| AgentKey {
                    agent: "Codex",
                    var: name.to_string(),
                    in_environment: has_env(name),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a variable name is plausibly a credential rather than a setting.
///
/// A conservative shape match, because the alternative is listing every
/// variable in someone's `env` block back at them as though it were capacity.
fn looks_like_a_key(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name.ends_with("_API_KEY") || name.ends_with("_AUTH_TOKEN") || name.ends_with("_TOKEN")
}

fn claude_settings_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir).join("settings.json"));
    }
    Some(dirs::home_dir()?.join(".claude").join("settings.json"))
}

fn codex_config_path() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.is_empty()
    {
        return Some(PathBuf::from(home).join("config.toml"));
    }
    Some(dirs::home_dir()?.join(".codex").join("config.toml"))
}

/// Say what was found, and what each thing is worth.
fn report(found: &Found, local: &[(u16, &'static str)]) {
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

    for (port, name) in local {
        println!(
            "  found    {:<28}free, private, and yours",
            format!("{name} on :{port}")
        );
    }

    for key in &found.agent_keys {
        // The unreadable case is the one worth the words: it looks like
        // capacity, and it is not, until the variable is where the daemon runs.
        let note = if key.in_environment {
            format!("used by {}", key.agent)
        } else {
            format!("used by {} — not in this environment", key.agent)
        };
        println!("  found    {:<28}{note}", key.var);
    }

    if !found.anything() && local.is_empty() {
        println!("  nothing yet");
    }
}

/// Local model servers worth looking for, on the ports they ship with.
///
/// Reported, never written: `init` without `--write` changes nothing, and a
/// discovered endpoint is the user's decision to make (`docs/DESIGN.md`).
const LOCAL_SERVERS: &[(u16, &str)] = &[(11434, "Ollama"), (1234, "LM Studio"), (8000, "vLLM")];

/// Ask each loopback port whether an OpenAI-compatible server is there.
///
/// Short timeout and total silence when nothing answers: this runs on every
/// `init`, including on machines that will never run a local model, and a
/// second of latency or a line of "not found" noise would be a tax on everyone
/// for a feature most people do not use.
async fn find_local_servers() -> Vec<(u16, &'static str)> {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
    {
        Ok(client) => client,
        Err(_) => return Vec::new(),
    };
    let probes = LOCAL_SERVERS.iter().map(|(port, name)| {
        let client = client.clone();
        async move {
            let url = format!("http://127.0.0.1:{port}/v1/models");
            match client.get(url).send().await {
                Ok(response) if response.status().is_success() => Some((*port, *name)),
                _ => None,
            }
        }
    });
    futures_util::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// Run `ironwire init`.
pub(crate) async fn run(
    port: Option<u16>,
    write: bool,
    dry_run: bool,
    no_service: bool,
) -> Result<()> {
    let paths = paths()?;
    let port = port
        .or_else(|| Config::load(&paths).ok().map(|c| c.server.port))
        .unwrap_or(DEFAULT_PORT);

    println!("IronWire — one local endpoint for the AI capacity you already have");
    println!();
    if dry_run {
        println!("[dry run] nothing on this machine will be changed.");
        println!();
    }
    println!("Looking at this machine…");
    println!();

    let found = Found::detect();
    let local = find_local_servers().await;
    report(&found, &local);

    if !found.anything() && local.is_empty() {
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

    if write {
        println!();
        if dry_run {
            println!("[dry run] would write {}", paths.config_file().display());
        } else {
            write_config(&paths, port)?;
        }
    }

    let enabled = ask_for_subscriptions(&paths, &found, dry_run)?;
    let wired = wire_agents(port, dry_run)?;
    let daemon = start_daemon(port, no_service, dry_run).await?;

    summarise(port, &found, &enabled, &wired, &daemon);
    Ok(())
}

/// Ask once, for everything found.
///
/// `docs/TRUST.md` §2 requires that the risk be stated in plain language and
/// the answer recorded against the wording it answered. It does not require
/// that the question be asked twice when two subscriptions carry the same risk
/// — and asking twice makes the second one feel like a formality, which is the
/// opposite of what a consent gate is for.
///
/// Answering no is a complete answer: the metered keys and local models found
/// above still work, which is what makes saying no cheap enough to mean
/// something.
fn ask_for_subscriptions(
    paths: &ironwire_core::config::PathsConfig,
    found: &Found,
    dry_run: bool,
) -> Result<Vec<&'static str>> {
    let path = paths.consent_file();
    let mut ledger = ConsentLedger::load(&path);
    let pending = found.ungranted_subscriptions(&ledger);
    if pending.is_empty() {
        return Ok(Vec::new());
    }

    let products = join(pending.iter().map(|s| s.product), "and");
    let clients = join(pending.iter().map(|s| s.client), "and");
    let vendors = join(pending.iter().map(|s| s.vendor), "and");
    let hosts = join(pending.iter().map(|s| s.host), "and");
    let many = pending.len() > 1;

    println!();
    if many {
        println!("  IronWire can use the {products} subscriptions above, by replaying");
        println!("  the OAuth tokens {clients} have already stored on this machine.");
        println!("  Each token goes only to its own provider ({hosts}),");
        println!("  from this computer.");
    } else {
        println!("  IronWire can use your {products} subscription, by replaying the");
        println!("  OAuth token {clients} has already stored on this machine. It goes");
        println!("  only to {hosts}, from this computer.");
    }
    println!();
    println!(
        "  · {vendors} {} not document this authentication path and may",
        if many { "do" } else { "does" }
    );
    println!("    change or block it at any time.");
    println!("  · Using it from a third-party proxy may fall outside your");
    println!("    subscription's intended use. If they object, it is your account");
    println!("    that is affected.");
    println!(
        "  · You can use {} instead —",
        join(pending.iter().map(|s| s.alternative), "or")
    );
    println!("    fully supported, no ambiguity.");
    println!();

    let question = if many {
        format!("  Use the {products} subscriptions? [Y/n] ")
    } else {
        format!("  Use the {products} subscription? [Y/n] ")
    };
    if !ask(&question)? {
        println!();
        println!("  Left disabled. IronWire will use API keys and local models only.");
        println!("  `ironwire connect claude --subscription` asks again, one at a time.");
        return Ok(Vec::new());
    }

    let enabled: Vec<&'static str> = pending.iter().map(|s| s.backend_id).collect();
    if dry_run {
        println!(
            "  [dry run] would record consent for: {}",
            enabled.join(", ")
        );
        return Ok(enabled);
    }
    for id in &enabled {
        ledger.grant(id, Utc::now());
    }
    ledger.save(&path).context("writing the consent ledger")?;
    println!("  Recorded in {}.", path.display());
    Ok(enabled)
}

/// Read a yes/no answer, defaulting to yes.
///
/// Not a terminal: no answer is coming, and blocking forever inside a script
/// would be worse than declining. The safe default when nobody is there to ask
/// is the one that grants nothing.
fn ask(question: &str) -> Result<bool> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        println!("{question}");
        println!("  (not a terminal — leaving subscriptions disabled)");
        return Ok(false);
    }
    print!("{question}");
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("reading your answer")?;
    Ok(!matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "n" | "no"
    ))
}

/// Point every agent that is installed at IronWire.
///
/// Only the ones that are installed: writing a settings file nothing will read
/// is how a setup step turns into litter in someone's home directory.
fn wire_agents(port: u16, dry_run: bool) -> Result<Vec<String>> {
    let mut wired = Vec::new();
    println!();
    if connect::claude_installed() {
        connect::wire_claude(port, dry_run)?;
        wired.push("Claude Code".to_string());
    }
    if connect::codex_installed() {
        connect::wire_codex(port, dry_run)?;
        wired.push("Codex".to_string());
    }
    wired.extend(wire_catalog_agents(port, dry_run));
    if wired.is_empty() {
        println!("No coding agent found here yet. Once one is installed, run");
        println!("`ironwire init` again and it will be pointed at IronWire.");
    }
    Ok(wired)
}

/// Tools the signed catalog taught us about since the last release.
///
/// One tool failing is not the run failing. These arrive from a document rather
/// than from this binary, so a config shape we did not anticipate is a thing to
/// report and move past — the two built-in agents above, and the daemon itself,
/// have nothing to do with it.
fn wire_catalog_agents(port: u16, dry_run: bool) -> Vec<String> {
    let Ok(paths) = ironwire_core::config::PathsConfig::resolve() else {
        return Vec::new();
    };
    let store = ironwire_catalog::CatalogStore::load(
        ironwire_catalog::CATALOG_PUBLIC_KEY,
        &paths.catalog_file(),
    );
    let catalog = store.current();

    for (id, problem) in catalog.rejected_agents() {
        // Said out loud rather than dropped silently: a tool the document meant
        // to ship and we would not touch is exactly the thing worth knowing.
        tracing::warn!(agent = id, %problem, "catalog entry ignored");
    }

    let mut wired = Vec::new();
    for agent in catalog.agents() {
        if !connect::catalog_agent_installed(agent) {
            continue;
        }
        match connect::wire_catalog_agent(agent, port, dry_run) {
            Ok(()) => wired.push(agent.name.clone()),
            Err(error) => println!("  {} could not be wired: {error:#}", agent.name),
        }
    }
    wired
}

/// Where the daemon ended up.
enum Daemon {
    /// Running in the background, and it survives a reboot.
    Service,
    /// Already listening before we got here.
    AlreadyUp,
    /// Nothing to install to, or the user asked us not to.
    Foreground,
}

/// Leave the daemon running.
async fn start_daemon(port: u16, no_service: bool, dry_run: bool) -> Result<Daemon> {
    println!();
    if listening(port).await {
        println!("IronWire is already listening on 127.0.0.1:{port}.");
        return Ok(Daemon::AlreadyUp);
    }
    if dry_run {
        println!("[dry run] would install and start the background service.");
        return Ok(Daemon::Service);
    }
    if no_service {
        return Ok(Daemon::Foreground);
    }

    match service::install_and_start(Some(port))? {
        service::Outcome::Running(_) => {
            // Started is not the same as listening, and the difference is a
            // port already taken or a unit that dies on start. Waiting a moment
            // and looking is the only honest way to say "it is running".
            if wait_until_listening(port).await {
                Ok(Daemon::Service)
            } else {
                println!("The service started but nothing is listening on {port} yet.");
                println!("Check it with: ironwire service status");
                Ok(Daemon::Service)
            }
        }
        service::Outcome::Installed { retry } => {
            println!();
            println!("The unit is installed but did not start. Finish with:");
            println!("    {retry}");
            Ok(Daemon::Foreground)
        }
        service::Outcome::Unsupported => {
            println!("No user-scoped service manager here — common in containers and");
            println!("over a bare SSH session. Run it in the foreground instead.");
            Ok(Daemon::Foreground)
        }
    }
}

/// Whether anything is accepting connections on the loopback port.
async fn listening(port: u16) -> bool {
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
}

/// Give a just-started daemon a moment to bind.
async fn wait_until_listening(port: u16) -> bool {
    for _ in 0..20 {
        if listening(port).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    false
}

/// Say what is true now, and the one command that comes next.
fn summarise(
    port: u16,
    found: &Found,
    enabled: &[&'static str],
    wired: &[String],
    daemon: &Daemon,
) {
    println!();
    match daemon {
        Daemon::Service | Daemon::AlreadyUp => {
            println!("Ready. IronWire is on http://127.0.0.1:{port}.");
        }
        Daemon::Foreground => {
            println!("Almost there. Start the daemon and leave it running:");
            println!();
            println!("    ironwire serve");
        }
    }
    println!();

    if !wired.is_empty() {
        println!("  wired    {}", wired.join(", "));
    }
    if !enabled.is_empty() {
        println!("  enabled  {}", enabled.join(", "));
    }

    // A key an agent uses that the daemon cannot see is the one failure this
    // command can predict, so it says so here rather than letting `doctor`
    // discover it later.
    for key in found.agent_keys.iter().filter(|k| !k.in_environment) {
        println!(
            "  note     {} is set for {}, but not where IronWire runs",
            key.var, key.agent
        );
    }

    println!();
    // Only once there is something to talk to. "Run `claude`" while the daemon
    // is down sends the user at a connection error, which is a worse first
    // impression than the extra step they still have to take.
    if matches!(daemon, Daemon::Service | Daemon::AlreadyUp) {
        if wired.iter().any(|w| w == "Claude Code") {
            println!("Start a new terminal and run `claude` — it goes through IronWire now.");
        } else if wired.iter().any(|w| w == "Codex") {
            println!("Start a new `codex` session — it goes through IronWire now.");
        }
    }
    println!("    ironwire doctor    confirm it end to end, with a real request");
    println!("    ironwire status    what capacity you have, and what is left");
    println!("    ironwire watch     live routing, quiet unless something changes");
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
# Request and response bodies, exactly as they crossed the wire, under
# $IRONWIRE_HOME/bodies. Off by default — these contain your source.
#
# Only the *current* exchange of each session is held: the next turn releases
# the previous one's bodies, so a finished session leaves exactly one — its
# final call, the only one anything downstream attests. The SHA-256 of both
# bodies stays on every row, because that is what a provider's own per-request
# receipt is a signature over, and a hash is not the content.
bodies = false
# Days of history to keep. Pruned daily by the daemon; 0 keeps everything,
# which is a real choice but not a good default for a file nobody watches.
retain_days = 90

[usage]
# The session section on `ironwire status`: burn rate, and where this window
# ends up at that rate. Measured from the ledger above — your own traffic,
# never a provider's quota, which is reported or `unknown` and never guessed.
enabled = true
# Length of a session window, in hours. Five is Claude Code's.
session_hours = 5
# How far back to look when calibrating against your own past windows.
history_hours = 192
# Your plan: pro, max5, max20, or team. Deliberately unset by default —
# per-window token limits are not published, and IronWire will not assert one
# on your behalf. Set it and the ceiling becomes your claim about your own
# subscription, labelled as such. Left unset, the comparison is against your
# own completed sessions, which needs no table at all.
# plan = "max5"

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
# off         — no substitution; requests are forwarded byte-identical.
# credentials — API keys, tokens, private keys, and any named_values below.
# pii         — credentials, plus emails, IP addresses and phone numbers.
#               Deterministic classes only: names need a classifier that has
#               to publish its precision and recall before it ships.
# full        — pii, and requests are routed only to the backends named in
#               trusted_backends below. Nothing else is tried: when none of
#               them can serve a request, IronWire refuses it rather than
#               falling back. There is no default set — which operators you
#               trust with your data is not IronWire's call to make.
mode = "off"
# Required when mode = "full"; ignored otherwise.
# trusted_backends = ["nearai", "ollama"]
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

# [limits]
# Spend caps, in dollars per day, measured over your local calendar day and
# only against *metered* backends — a subscription is already paid for, and
# capping it would cap capacity you bought. No cap unless you set one.
#
# daily_spend_usd = 10.0
# on_breach = "descend"   # keep working on free capacity (default), or
#                         # "refuse" to stop so you find out immediately
#
# [[limits.backends]]
# id = "anthropic-key"
# daily_spend_usd = 5.0
#
# A cap needs the trace ledger, which is where spend is measured; `ironwire
# serve` refuses to start with a cap set and capture off, rather than leaving
# you believing in a limit that can never fire.

# Backends are discovered from your logins and environment, so this section is
# usually unnecessary. Declare one to override how a discovered backend is
# built, to switch one off, or to add an endpoint IronWire cannot discover.
# Commented out because an empty `[[backends]]` block is not the default —
# declaring nothing and declaring an entry are different states.
#
# [[backends]]
# id = "anthropic-key"          # matches a discovered backend: configures it
# kind = "anthropic-api"        # claude-subscription, anthropic-api,
#                               # codex-subscription, openai-api, nearai,
#                               # openai-compatible
# enabled = false               # a real kill switch: it is never registered
# api_key_env = "ANTHROPIC_API_KEY_WORK"   # which variable holds the key
#
# [[backends]]
# id = "local"
# kind = "openai-compatible"    # a hosted endpoint speaking OpenAI chat
# base_url = "https://example.internal/v1" # required for this kind
# api_key_env = "LOCAL_API_KEY"            # required; no secrets in this file
#
# [[backends]]
# id = "ollama"                 # an id discovery does not produce: adds one
# kind = "local"                # a model on this machine: free, private, and
#                               # the cheapest capacity there is
# base_url = "http://127.0.0.1:11434/v1"   # required, and must include /v1 —
#                               # Ollama's native /api/* is a different protocol
# api_key_env = "LOCAL_API_KEY" # optional here; most local servers take no auth
# # A bare slug on a local backend counts as `fast`, whatever its name suggests,
# # because local capacity sorts cheapest and a model that reads as frontier-tier
# # would take work you asked a frontier model for. Say so explicitly to opt one
# # up the ladder:
# models = ["qwen3-coder:30b", {{ name = "llama3.3:70b", tier = "balanced" }}]
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

        let mut loaded = Config::load(&paths).expect("the generated config parses");
        // The template states `mode = "off"` where the struct default leaves it
        // unset. That is the one intentional difference: the file exists to
        // *document* the ladder, and a commented-out mode teaches nothing. It
        // must still mean off, which is what this checks before comparing the
        // rest field by field.
        assert_eq!(
            loaded.privacy.mode,
            Some(ironwire_core::config::PrivacyMode::Off)
        );
        assert_eq!(loaded.privacy.mode(), Config::default().privacy.mode());
        loaded.privacy.mode = None;
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
