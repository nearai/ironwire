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
    match std::env::var("ANTHROPIC_BASE_URL") {
        Ok(value) if value.trim_end_matches('/') == expected.trim_end_matches('/') => {
            println!("claude code   pointed here");
        }
        Ok(value) if value.contains("127.0.0.1") || value.contains("localhost") => {
            // The commonest near-miss: an old port, or a `--port` override the
            // shell profile never learned about.
            println!("claude code   pointed at {value} — that is not this daemon");
            println!("              fix:  export ANTHROPIC_BASE_URL={expected}");
        }
        Ok(value) => {
            println!("claude code   pointed at {value} — bypassing IronWire");
            println!("              fix:  export ANTHROPIC_BASE_URL={expected}");
        }
        Err(_) => {
            println!("claude code   not pointed here (ANTHROPIC_BASE_URL is unset)");
            println!("              fix:  eval \"$(ironwire env)\"");
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
