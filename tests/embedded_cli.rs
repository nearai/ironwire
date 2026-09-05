//! The command-line host uses the same assembly without changing its terminal contract.
#![cfg(unix)]
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn the_cli_announces_the_bound_port_and_drains_on_sigterm() {
    let home = tempfile::tempdir().unwrap();
    let runtime = home.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    let mut config = String::from("[updates]\ncheck = false\n");
    for (id, kind) in [
        ("nearai", "nearai"),
        ("claude-sub", "claude-subscription"),
        ("codex-sub", "codex-subscription"),
        ("anthropic-key", "anthropic-api"),
        ("openai-key", "openai-api"),
    ] {
        config.push_str(&format!(
            "[[backends]]\nid = '{id}'\nkind = '{kind}'\nenabled = false\n"
        ));
    }
    std::fs::write(runtime.join("config.toml"), config).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ironwire"))
        .args(["serve", "--port", "0"])
        .env_clear()
        .env("HOME", home.path())
        .env("IRONWIRE_HOME", &runtime)
        .env("CODEX_HOME", home.path())
        .env("PATH", "")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut output = String::new();
    let port = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("CLI stays alive until listening");
            output.push_str(&line);
            output.push('\n');
            if let Some(port) = line.strip_prefix("IronWire listening on http://127.0.0.1:") {
                break port.parse::<u16>().unwrap();
            }
        }
    })
    .await
    .unwrap();
    assert_ne!(port, 0);
    assert!(
        reqwest::get(format!("http://127.0.0.1:{port}/_ironwire/health"))
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let refusal = tokio::process::Command::new(env!("CARGO_BIN_EXE_ironwire"))
        .args(["serve", "--port", &port.to_string()])
        .env("IRONWIRE_HOME", &runtime)
        .output()
        .await
        .unwrap();
    assert!(!refusal.status.success());
    assert!(
        String::from_utf8_lossy(&refusal.stderr)
            .contains(&format!("IronWire is already running on port {port}"))
    );
    let result = tokio::process::Command::new("kill")
        .args(["-TERM", &child.id().unwrap().to_string()])
        .status()
        .await
        .unwrap();
    assert!(result.success());
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    while let Some(line) = lines.next_line().await.unwrap() {
        output.push_str(&line);
        output.push('\n');
    }
    assert!(output.contains("  Point your agents at it:  ironwire init\n"));
    assert!(output.contains("  Confirm they are:         ironwire doctor\n"));
    assert!(output.contains("  discoverable at: "));
    assert!(!runtime.join("endpoint.json").exists());
    assert!(!home.path().join(".ironwire/endpoint.json").exists());
    assert!(
        tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .is_ok()
    );
}
