//! `ironwire doctor` — verify every connection end to end.
//!
//! This command makes **real network calls**. A config that parses and a
//! credential file that exists prove nothing; the failures that actually bite
//! — an expired token, a beta flag the provider stopped honouring, an account
//! not entitled to a model — only appear on the wire.

use anyhow::Result;

use super::control_client::ControlClient;

/// Check the daemon, the clients pointed at it, and each backend.
pub(crate) async fn run(port: Option<u16>) -> Result<()> {
    let client = ControlClient::new(port)?;
    let status = client.status().await?;
    println!("daemon        ok — 127.0.0.1:{}", status.port);

    // The clients *before* the backends, because this is the failure people
    // actually hit: IronWire running perfectly, and the agent still going
    // straight to the provider because nothing points it here. Every backend
    // below can be green while the user sees no effect whatsoever.
    check_clients(status.port);

    if status.backends.is_empty() {
        println!("backends      none configured");
        println!();
        println!("Run `ironwire connect claude --subscription`, or set ANTHROPIC_API_KEY");
        println!("and restart the daemon.");
        return Ok(());
    }

    // Static checks first: a backend awaiting consent must not be probed, since
    // probing it would use the very credential the user has not authorised.
    let mut probeable = false;
    for backend in &status.backends {
        if !backend.authenticated {
            let why = backend.detail.as_deref().unwrap_or("no credential found");
            println!("{:<14}not connected — {why}", backend.id);
        } else if !backend.consented {
            println!(
                "{:<14}awaiting consent — `ironwire connect claude --subscription`",
                backend.id
            );
        } else {
            probeable = true;
        }
    }

    if !probeable {
        println!();
        println!("Nothing to probe: no backend is both authenticated and enabled.");
        return Ok(());
    }

    println!();
    // Said before any probe: under `full` a healthy backend that is not
    // trusted is not an answer, and a user staring at green probe lines
    // should not have to work that out.
    let privacy = super::paths()
        .ok()
        .and_then(|paths| ironwire_core::config::Config::load(&paths).ok())
        .map(|config| config.privacy)
        .unwrap_or_default();
    if privacy.mode() == ironwire_core::config::PrivacyMode::Full {
        let registered: Vec<&str> = status
            .backends
            .iter()
            .filter(|b| b.authenticated && privacy.trusted_backends.contains(&b.id))
            .map(|b| b.id.as_str())
            .collect();
        if registered.is_empty() {
            println!(
                "privacy      mode is `full`, and none of the trusted backends ({}) \n\
                 \x20            is connected — every request will be refused",
                privacy.trusted_backends.join(", ")
            );
        } else {
            println!(
                "privacy      mode is `full` — routing restricted to {}",
                registered.join(", ")
            );
        }
        println!();
    }
    // The one line that makes a shortened model list falsifiable. The ChatGPT
    // backend gates newer models on the reported client version, and a stale
    // one returns fewer models rather than an error — so without this a user
    // seeing fewer models than Codex has nothing at all to look at.
    if status.backends.iter().any(|b| b.id == "codex-sub") {
        let (version, source) = ironwire_upstream::codex_version::detect_with_source().await;
        println!("codex version {version} — {}", source.describe());
        println!();
    }

    println!("Probing backends…");
    let mut failures = 0;
    for probe in client.probe().await? {
        if probe.ok {
            println!("{:<14}ok — {} ms", probe.id, probe.latency_ms);
        } else {
            failures += 1;
            let detail = probe.error.as_deref().unwrap_or("unknown failure");
            println!("{:<14}FAILED — {detail}", probe.id);
        }
    }

    println!();
    if failures == 0 {
        println!("All connected backends answered.");
    } else {
        println!("{failures} backend(s) failed. `ironwire status` has the details.");
    }
    Ok(())
}

/// Whether the coding agents on this machine are actually pointed at us.
fn check_clients(port: u16) {
    let expected = format!("http://127.0.0.1:{port}/anthropic");
    let same = |value: &str| value.trim_end_matches('/') == expected.trim_end_matches('/');

    // Two places can point Claude Code here, and `doctor` has to look at both.
    // The settings file is where `ironwire init` writes it, and it is *not* in
    // this process's environment — checking only the variable would report a
    // correctly configured machine as broken, which is the exact failure this
    // command exists to catch.
    let settings = claude_settings_base_url();
    let variable = std::env::var("ANTHROPIC_BASE_URL").ok();

    match (settings.as_deref(), variable.as_deref()) {
        (Some(s), _) if same(s) => {
            println!("claude code   pointed here (~/.claude/settings.json)");
            // A shell variable disagreeing with the file is worth saying out
            // loud: which one wins is Claude Code's business, not ours, and a
            // machine where the two disagree will behave differently depending
            // on how the agent was started.
            if let Some(v) = variable.as_deref().filter(|v| !same(v)) {
                println!("              note: ANTHROPIC_BASE_URL is also set, to {v}");
                println!(
                    "              unset it so there is one answer:  unset ANTHROPIC_BASE_URL"
                );
            }
        }
        (_, Some(v)) if same(v) => println!("claude code   pointed here (ANTHROPIC_BASE_URL)"),
        (Some(s), _) => {
            println!("claude code   settings.json points at {s} — bypassing IronWire");
            println!("              fix:  ironwire connect claude");
        }
        // The commonest near-miss: an old port, or a `--port` override that a
        // shell profile never learned about.
        (None, Some(v)) => {
            println!("claude code   pointed at {v} — bypassing IronWire");
            println!("              fix:  unset ANTHROPIC_BASE_URL && ironwire connect claude");
        }
        (None, None) => {
            println!("claude code   not pointed here");
            println!("              fix:  ironwire init");
        }
    }

    match codex_state(port) {
        CodexState::PointedHere => println!("codex         pointed here"),
        CodexState::PointedElsewhere(provider) => {
            println!("codex         using provider `{provider}` — bypassing IronWire");
            println!("              fix:  ironwire connect codex");
        }
        CodexState::NoConfig => {
            // Not a problem: plenty of people do not use Codex. Said once,
            // quietly, rather than as a warning that needs acting on.
            println!("codex         no config found (not using Codex?)");
        }
        CodexState::Unreadable(why) => {
            println!("codex         could not read its config — {why}");
        }
    }
    println!();
}

enum CodexState {
    PointedHere,
    PointedElsewhere(String),
    NoConfig,
    Unreadable(String),
}

fn codex_state(port: u16) -> CodexState {
    let Some(path) = codex_config_path() else {
        return CodexState::Unreadable("no home directory".to_string());
    };
    codex_state_at(&path, port)
}

/// The path is a parameter rather than a global read, so this is testable
/// without mutating process environment — which `unsafe_code = "forbid"` rules
/// out anyway, and which would make these tests order-dependent.
fn codex_state_at(path: &std::path::Path, port: u16) -> CodexState {
    let Ok(text) = std::fs::read_to_string(path) else {
        return CodexState::NoConfig;
    };
    let parsed: toml::Table = match text.parse() {
        Ok(parsed) => parsed,
        Err(error) => return CodexState::Unreadable(error.to_string()),
    };

    let provider = parsed
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or("openai");
    if provider != "ironwire" {
        return CodexState::PointedElsewhere(provider.to_string());
    }

    // Pointed at *an* IronWire — check it is this one. A stale port here is a
    // silent bypass, and the symptom is identical to the daemon being down.
    let base = parsed
        .get("model_providers")
        .and_then(toml::Value::as_table)
        .and_then(|providers| providers.get("ironwire"))
        .and_then(toml::Value::as_table)
        .and_then(|ours| ours.get("base_url"))
        .and_then(toml::Value::as_str)
        .unwrap_or_default();
    if base.contains(&format!(":{port}/")) {
        CodexState::PointedHere
    } else {
        CodexState::PointedElsewhere(format!("ironwire at {base}"))
    }
}

fn codex_config_path() -> Option<std::path::PathBuf> {
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.is_empty()
    {
        return Some(std::path::PathBuf::from(home).join("config.toml"));
    }
    Some(dirs::home_dir()?.join(".codex").join("config.toml"))
}

/// What Claude Code's settings file says its base URL is, if anything.
///
/// An unreadable or unparseable file is treated as saying nothing: `doctor`
/// reports on the user's setup and must not fail because of a syntax error in
/// a file it does not own.
fn claude_settings_base_url() -> Option<String> {
    let text = std::fs::read_to_string(claude_settings_path()?).ok()?;
    let root: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(
        root.get("env")?
            .get("ANTHROPIC_BASE_URL")?
            .as_str()?
            .to_string(),
    )
}

fn claude_settings_path() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR")
        && !dir.is_empty()
    {
        return Some(std::path::PathBuf::from(dir).join("settings.json"));
    }
    Some(dirs::home_dir()?.join(".claude").join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn codex_config(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(contents.as_bytes()).expect("write");
        (dir, path)
    }

    #[test]
    fn a_codex_pointed_at_this_daemon_is_recognised() {
        let (_dir, path) = codex_config(
            "model_provider = \"ironwire\"\n\n\
             [model_providers.ironwire]\n\
             base_url = \"http://127.0.0.1:8463/openai/v1\"\n",
        );
        assert!(matches!(
            codex_state_at(&path, 8463),
            CodexState::PointedHere
        ));
    }

    #[test]
    fn a_codex_pointed_at_a_different_port_is_a_silent_bypass() {
        // The symptom is identical to the daemon being down, which is why this
        // is worth naming rather than reporting as "pointed here".
        let (_dir, path) = codex_config(
            "model_provider = \"ironwire\"\n\n\
             [model_providers.ironwire]\n\
             base_url = \"http://127.0.0.1:9999/openai/v1\"\n",
        );
        assert!(matches!(
            codex_state_at(&path, 8463),
            CodexState::PointedElsewhere(_)
        ));
    }

    #[test]
    fn a_codex_using_its_own_provider_is_reported_as_bypassing_us() {
        let (_dir, path) = codex_config("model = \"gpt-5.6\"\n");
        match codex_state_at(&path, 8463) {
            CodexState::PointedElsewhere(provider) => assert_eq!(provider, "openai"),
            _ => panic!("a Codex not configured for IronWire must be reported as bypassing"),
        }
    }

    #[test]
    fn a_missing_codex_config_is_not_an_error() {
        // Plenty of people do not use Codex, and telling them something is
        // wrong would be worse than saying nothing.
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            codex_state_at(&dir.path().join("absent.toml"), 8463),
            CodexState::NoConfig
        ));
    }

    #[test]
    fn an_unparseable_codex_config_says_so_rather_than_guessing() {
        let (_dir, path) = codex_config("this is = = not toml\n");
        assert!(matches!(
            codex_state_at(&path, 8463),
            CodexState::Unreadable(_)
        ));
    }
}
