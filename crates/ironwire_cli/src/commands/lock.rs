//! One daemon per `$IRONWIRE_HOME`.
//!
//! Port collision already stops two daemons sharing a *port*. This stops two
//! sharing a *home*, which is the subtler and more damaging case: both hold the
//! consent ledger in memory and write it back on change, so a consent granted
//! in one silently disappears when the other writes. A consent vanishing is a
//! trust-relevant failure (`docs/TRUST.md` §2), and nothing about the symptom
//! points at the cause.
//!
//! Liveness is decided by asking, not by a PID. A PID check says a process
//! exists; it does not say that process is IronWire, and PIDs are reused. So
//! the lock records a port and the check is an HTTP request to it — which also
//! means a lock left behind by a crash blocks nothing, because nothing answers.

use std::path::Path;

use anyhow::{Context, Result};

/// What a held lock records.
struct Held {
    port: u16,
}

/// Take the lock for `$IRONWIRE_HOME`, or explain who has it.
///
/// # Errors
///
/// When another daemon is demonstrably alive on this home.
pub(crate) async fn acquire(path: &Path, port: u16) -> Result<Guard> {
    if let Some(held) = read(path)
        && held.port != port
        && responds(held.port).await
    {
        anyhow::bail!(
            "another IronWire is already using {}.\n\
             \n\
             It is listening on port {}. Two daemons sharing one home overwrite\n\
             each other's consent ledger, so this one will not start.\n\
             \n\
             Use it:            ironwire status --port {}\n\
             Or give this one its own home:\n\
             \n\
                 IRONWIRE_HOME=~/.ironwire-alt ironwire serve --port {port}",
            path.parent().unwrap_or(path).display(),
            held.port,
            held.port,
        );
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, format!("{port}\n"))
        .with_context(|| format!("writing {}", path.display()))?;

    Ok(Guard {
        path: path.to_path_buf(),
    })
}

fn read(path: &Path) -> Option<Held> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(Held {
        port: text.trim().parse().ok()?,
    })
}

/// Whether an IronWire is actually answering on that port.
///
/// A short timeout: this runs before the daemon binds, and a slow answer here
/// is a slow startup for everyone. Anything that does not answer promptly is
/// treated as gone, which is the right way to be wrong — refusing to start
/// because of a stale file would be worse than starting alongside a daemon
/// that is wedged.
async fn responds(port: u16) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    client
        .get(format!("http://127.0.0.1:{port}/_ironwire/health"))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

/// Releases the lock on drop.
#[derive(Debug)]
pub(crate) struct Guard {
    path: std::path::PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Best effort. A lock left behind by a hard kill is harmless: the next
        // startup asks the recorded port and gets no answer.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_fresh_home_can_be_locked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.lock");
        let _guard = acquire(&path, 8463).await.expect("acquires");
        assert!(path.exists());
    }

    #[tokio::test]
    async fn a_stale_lock_from_a_crash_blocks_nothing() {
        // Port 1 is never listening. Refusing to start because of a file left
        // behind by a hard kill would be a worse failure than the one this
        // guards against.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.lock");
        std::fs::write(&path, "1\n").expect("write");
        let _guard = acquire(&path, 8463).await.expect("a stale lock is ignored");
    }

    #[tokio::test]
    async fn an_unreadable_lock_blocks_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.lock");
        std::fs::write(&path, "not a port").expect("write");
        let _guard = acquire(&path, 8463).await.expect("garbage is ignored");
    }

    #[tokio::test]
    async fn restarting_on_the_same_port_is_not_blocked_by_our_own_lock() {
        // The lock records *this* port; a restart must not be refused by the
        // file its predecessor left behind.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.lock");
        std::fs::write(&path, "8463\n").expect("write");
        let _guard = acquire(&path, 8463).await.expect("same port is our own");
    }

    #[tokio::test]
    async fn the_lock_is_released_on_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.lock");
        {
            let _guard = acquire(&path, 8463).await.expect("acquires");
            assert!(path.exists());
        }
        assert!(
            !path.exists(),
            "a clean shutdown must not leave a lock behind"
        );
    }

    #[tokio::test]
    async fn a_live_daemon_on_another_port_refuses_the_second() {
        // The case this exists for. Served by a real listener, because the
        // liveness check is an HTTP request and mocking it would test nothing.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let held_port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                        .await;
                });
            }
        });

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.lock");
        std::fs::write(&path, format!("{held_port}\n")).expect("write");

        let error = acquire(&path, held_port + 1)
            .await
            .expect_err("a live daemon on this home must block a second");
        let message = error.to_string();
        assert!(message.contains("already using"), "got: {message}");
        assert!(
            message.contains("IRONWIRE_HOME"),
            "the error must say how to run two: {message}"
        );
    }
}
