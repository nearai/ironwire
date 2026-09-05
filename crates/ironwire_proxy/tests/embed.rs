//! Lifecycle contract for applications hosting their own proxy.

use ironwire_proxy::embed::{EmbedError, start};

fn home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    // No update or provider probes in a lifecycle test.
    std::fs::write(home.path().join("config.toml"), "[updates]\ncheck = false\n[[backends]]\nid = 'nearai'\nkind = 'nearai'\nenabled = false\n[[backends]]\nid = 'claude-sub'\nkind = 'claude-subscription'\nenabled = false\n[[backends]]\nid = 'codex-sub'\nkind = 'codex-subscription'\nenabled = false\n[[backends]]\nid = 'anthropic-key'\nkind = 'anthropic-api'\nenabled = false\n[[backends]]\nid = 'openai-key'\nkind = 'openai-api'\nenabled = false\n").unwrap();
    home
}

#[tokio::test]
async fn a_host_can_start_and_stop_the_proxy_on_an_ephemeral_port() {
    let home = home();
    let proxy = start(home.path(), Some(0)).await.expect("starts");
    let port = proxy.port();
    assert_ne!(port, 0);
    let response = reqwest::get(format!("http://127.0.0.1:{port}/_ironwire/health"))
        .await
        .unwrap();
    assert!(response.status().is_success());
    proxy.shutdown().await;
    assert!(
        tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_second_start_against_the_same_home_is_refused() {
    let home = home();
    let first = start(home.path(), Some(0)).await.expect("starts");
    let second = start(home.path(), Some(0)).await;
    assert!(matches!(second, Err(EmbedError::Lock { .. })));
    first.shutdown().await;
}

#[test]
fn an_empty_home_needs_no_preparation() {
    const CHILD: &str = "IRONWIRE_EMPTY_HOME_TEST";
    if let Some(home) = std::env::var_os(CHILD) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let home = std::path::Path::new(&home);
                let proxy = start(home, Some(0)).await.expect("empty home starts");
                assert!(home.join("control.token").exists());
                assert!(!home.join("config.toml").exists());
                proxy.shutdown().await;
            });
        return;
    }
    // Isolate credential discovery and background HTTP from the developer's
    // account without mutating process-global environment in parallel tests.
    let home = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "an_empty_home_needs_no_preparation",
            "--nocapture",
        ])
        .env_clear()
        .env(CHILD, home.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("CODEX_HOME", home.path())
        .env("PATH", "")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "127.0.0.1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn a_busy_port_refuses_without_stealing_another_homes_lock() {
    let first_home = home();
    let first = start(first_home.path(), Some(0)).await.unwrap();
    let second_home = home();
    assert!(matches!(
        start(second_home.path(), Some(first.port())).await,
        Err(EmbedError::PortInUse { .. })
    ));
    assert!(!second_home.path().join("endpoint.json").exists());
    let second = start(second_home.path(), Some(0))
        .await
        .expect("failed start released ownership");
    second.shutdown().await;
    first.shutdown().await;
}

#[tokio::test]
async fn home_ownership_is_atomic_during_concurrent_starts() {
    let home = home();
    let (a, b) = tokio::join!(start(home.path(), Some(0)), start(home.path(), Some(0)));
    match (a, b) {
        (Ok(proxy), Err(EmbedError::Lock { .. })) | (Err(EmbedError::Lock { .. }), Ok(proxy)) => {
            proxy.shutdown().await
        }
        _ => panic!("exactly one concurrent start must own the home"),
    }
}

#[tokio::test]
async fn shutdown_releases_ownership_and_preserves_the_control_token() {
    let home = home();
    let first = start(home.path(), Some(0)).await.unwrap();
    let token = std::fs::read(home.path().join("control.token")).unwrap();
    let endpoint =
        ironwire_core::discovery::Endpoint::read_from(&home.path().join("endpoint.json")).unwrap();
    assert_eq!(
        endpoint.control_url,
        format!("http://127.0.0.1:{}", first.port())
    );
    first.shutdown().await;
    assert!(!home.path().join("endpoint.json").exists());
    assert!(!home.path().join("daemon.lock").exists());
    let second = start(home.path(), Some(0)).await.unwrap();
    assert_eq!(
        std::fs::read(home.path().join("control.token")).unwrap(),
        token
    );
    second.shutdown().await;
}

#[tokio::test]
async fn dropping_the_handle_requests_cleanup_without_releasing_ownership_early() {
    let home = home();
    let proxy = start(home.path(), Some(0)).await.unwrap();
    drop(proxy);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match start(home.path(), Some(0)).await {
                Ok(proxy) => {
                    proxy.shutdown().await;
                    break;
                }
                Err(EmbedError::Lock { .. }) => tokio::task::yield_now().await,
                Err(error) => panic!("unexpected restart error: {error}"),
            }
        }
    })
    .await
    .expect("drop drains and releases");
}

#[tokio::test]
async fn invalid_configuration_refuses_before_publishing_or_locking() {
    let home = home();
    std::fs::write(home.path().join("config.toml"), "not valid toml").unwrap();
    assert!(matches!(
        start(home.path(), Some(0)).await,
        Err(EmbedError::Config)
    ));
    assert!(!home.path().join("daemon.lock").exists());
    assert!(!home.path().join("control.token").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn the_home_and_token_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let home = home();
    let proxy = start(home.path(), Some(0)).await.unwrap();
    assert_eq!(
        std::fs::metadata(home.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(home.path().join("control.token"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    proxy.shutdown().await;
}

#[tokio::test]
async fn shutdown_drains_the_inflight_response_before_releasing_the_home() {
    use axum::{
        Router,
        body::Body,
        routing::{get, post},
    };
    use std::sync::Arc;
    use tokio::sync::Notify;
    let release = Arc::new(Notify::new());
    let gate = release.clone();
    let upstream = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let upstream_task = tokio::spawn(async move {
        let app = Router::new()
            .route("/v1/models", get(|| async { axum::Json(serde_json::json!({"data": []})) }))
            .route("/v1/chat/completions", post(move || {
                let gate = gate.clone();
                async move {
                    axum::response::Response::builder().header("content-type", "text/event-stream")
                        .body(Body::from_stream(async_stream::stream! {
                            yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n"));
                            gate.notified().await;
                            yield Ok(bytes::Bytes::from_static(b"data: [DONE]\n\n"));
                        })).unwrap()
                }
            }));
        axum::serve(upstream, app).await.unwrap();
    });
    let home = home();
    let config_path = home.path().join("config.toml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!("\n[[backends]]\nid = 'test-local'\nkind = 'local'\nbase_url = 'http://127.0.0.1:{upstream_port}/v1'\nmodels = [{{ name = 'test-model', tier = 'frontier' }}]\n"));
    std::fs::write(&config_path, config).unwrap();
    let proxy = start(home.path(), Some(0)).await.unwrap();
    let response = reqwest::Client::new().post(format!("http://127.0.0.1:{}/openai/v1/chat/completions", proxy.port()))
        .header("X-IronWire-Route", "test-local")
        .json(&serde_json::json!({"model":"test-model", "stream":true,"messages":[{"role":"user","content":"fixture"}]}))
        .send().await.unwrap();
    assert!(response.status().is_success());
    let mut body = response;
    assert!(body.chunk().await.unwrap().is_some());
    let stop = tokio::spawn(proxy.shutdown());
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(
        !stop.is_finished(),
        "shutdown must drain the outstanding stream"
    );
    assert!(matches!(
        start(home.path(), Some(0)).await,
        Err(EmbedError::Lock { .. })
    ));
    release.notify_one();
    assert!(body.text().await.unwrap().contains("[DONE]"));
    tokio::time::timeout(std::time::Duration::from_secs(3), stop)
        .await
        .unwrap()
        .unwrap();
    assert!(!home.path().join("daemon.lock").exists());
    upstream_task.abort();
    let _ = upstream_task.await;
}

#[tokio::test]
async fn spend_limits_without_capture_refuse_before_creating_a_token() {
    let home = home();
    std::fs::write(
        home.path().join("config.toml"),
        "[capture]\nenabled = false\n[limits]\ndaily_spend_usd = 1.0\n",
    )
    .unwrap();
    assert!(matches!(
        start(home.path(), Some(0)).await,
        Err(EmbedError::Config)
    ));
    assert!(!home.path().join("control.token").exists());
}

#[tokio::test]
async fn stale_legacy_lock_files_do_not_block_a_restart() {
    let home = home();
    std::fs::write(home.path().join("daemon.lock"), "1\n").unwrap();
    let proxy = start(home.path(), Some(0)).await.unwrap();
    proxy.shutdown().await;
}
