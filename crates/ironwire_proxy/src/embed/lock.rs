//! Home ownership survives startup races and is held until draining completes.
use super::EmbedError;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub(super) struct Guard {
    port_file: PathBuf,
    port: Option<u16>,
    published: Option<File>,
    // Close the published handle before releasing ownership (Windows deletion
    // can remain pending until the last handle closes).
    _file: File,
}

pub(super) async fn acquire(path: &Path, port: u16) -> Result<Guard, EmbedError> {
    // Do not unlink this inode: replacing a locked inode lets another process
    // lock the replacement while the first still owns the old one.
    let file =
        open_regular(&path.with_extension("lock.guard"), true).map_err(|_| EmbedError::Paths)?;
    file.try_lock().map_err(|_| EmbedError::Lock {
        port: read(path).unwrap_or(port),
    })?;
    // Cooperate with older CLIs that only publish a health-probed port file.
    if let Some(held) = read(path) {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .map_err(|_| EmbedError::Paths)?;
        if client
            .get(format!("http://127.0.0.1:{held}/_ironwire/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return Err(EmbedError::Lock { port: held });
        }
    }
    Ok(Guard {
        _file: file,
        port_file: path.to_owned(),
        port: None,
        published: None,
    })
}

// A legacy port is decimal text, not an arbitrary document. Allow surrounding
// whitespace without letting startup or Drop allocate an unbounded file.
const MAX_PORT_BYTES: u64 = 1024;

fn read(path: &Path) -> Option<u16> {
    let file = open_regular(path, false).ok()?;
    let mut text = String::new();
    file.take(MAX_PORT_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() as u64 > MAX_PORT_BYTES {
        return None;
    }
    text.trim().parse::<u16>().ok().filter(|port| *port != 0)
}

fn open_regular(path: &Path, writable: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(writable)
        .create(writable)
        .truncate(false);
    // Refuse final-component links and avoid blocking while opening a planted
    // FIFO. These flags do not confine ancestors or bound filesystem latency.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let flags = if cfg!(target_os = "macos") {
            0x4 | 0x100
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            0x800 | 0x20000
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            0x800 | 0x8000
        } else {
            return Err(std::io::ErrorKind::Unsupported.into());
        };
        options.custom_flags(flags);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT; validate the opened object below.
        options.custom_flags(0x00200000);
    }
    #[cfg(not(any(unix, windows)))]
    return Err(std::io::ErrorKind::Unsupported.into());
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
    }
    Ok(file)
}

fn still_published(path: &Path, file: &File) -> bool {
    let Ok(current) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !current.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(original) = file.metadata() else {
            return false;
        };
        current.dev() == original.dev() && current.ino() == original.ino()
    }
    #[cfg(not(unix))]
    {
        // Stable std does not expose Windows file identity. Retain the legacy
        // content check; do not claim atomic identity-conditional deletion.
        let _ = file;
        true
    }
}

impl Guard {
    pub(super) fn publish(&mut self, port: u16) -> Result<(), EmbedError> {
        let mut file = open_regular(&self.port_file, true).map_err(|_| EmbedError::Paths)?;
        file.set_len(0).map_err(|_| EmbedError::Paths)?;
        writeln!(file, "{port}").map_err(|_| EmbedError::Paths)?;
        self.port = Some(port);
        self.published = Some(file);
        Ok(())
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self
            .published
            .as_ref()
            .is_some_and(|file| still_published(&self.port_file, file))
            && read(&self.port_file) == self.port
        {
            // Cooperative cleanup only: a non-cooperating writer can still
            // replace the path between this check and unlink.
            let _ = std::fs::remove_file(&self.port_file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_port_text_is_bounded_and_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        for text in ["0", "65536", "garbage"] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(read(&path), None);
        }
        let padded = format!("8463{}", " ".repeat(MAX_PORT_BYTES as usize - 4));
        std::fs::write(&path, &padded).unwrap();
        assert_eq!(read(&path), Some(8463));
        std::fs::write(&path, format!("{padded} ")).unwrap();
        assert_eq!(read(&path), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn planted_links_are_neither_read_nor_truncated() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "8463\n").unwrap();
        symlink(&victim, &path).unwrap();
        assert_eq!(read(&path), None);
        let mut guard = acquire(&path, 0).await.unwrap();
        assert!(guard.publish(1234).is_err());
        drop(guard);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "8463\n");
        assert!(std::fs::symlink_metadata(&path).unwrap().is_symlink());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacement_with_same_port_survives_cleanup_and_lock_inode_stays() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        let mut guard = acquire(&path, 0).await.unwrap();
        let lock_path = path.with_extension("lock.guard");
        let inode = std::fs::metadata(&lock_path).unwrap().ino();
        guard.publish(1234).unwrap();
        assert!(matches!(
            acquire(&path, 0).await,
            Err(EmbedError::Lock { port: 1234 })
        ));
        std::fs::rename(&path, dir.path().join("original")).unwrap();
        std::fs::write(&path, "1234\n").unwrap();
        drop(guard);
        assert_eq!(read(&path), Some(1234));
        assert_eq!(std::fs::metadata(lock_path).unwrap().ino(), inode);
    }

    #[tokio::test]
    async fn legacy_health_refuses_takeover_but_redirects_are_not_followed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let target_port = target.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            for response in [
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_owned(),
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/\r\nContent-Length: 0\r\n\r\n"
                ),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0; 1024];
                assert!(stream.read(&mut buf).await.unwrap() > 0);
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        std::fs::write(&path, format!(" {port}\n")).unwrap();
        assert!(
            matches!(acquire(&path, 0).await, Err(EmbedError::Lock { port: held }) if held == port)
        );
        let guard = acquire(&path, 0).await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), target.accept())
                .await
                .is_err()
        );
        drop(guard);
        server.await.unwrap();
    }
    fn bounded_child(name: &str, marker: &str, proxy: bool) {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", name, "--nocapture"])
            .env(marker, "1");
        if proxy {
            for key in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
                command.env(key, "http://127.0.0.1:1");
            }
            command.env("NO_PROXY", "").env("no_proxy", "");
        }
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "isolated regression failed");
                return;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("isolated regression blocked");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn legacy_health_ignores_environment_proxies() {
        bounded_child(
            "embed::lock::tests::legacy_health_refuses_takeover_but_redirects_are_not_followed",
            "IRONWIRE_PROBE_PROXY_CHILD",
            true,
        );
    }

    #[cfg(unix)]
    #[test]
    fn planted_fifo_cannot_block_legacy_reads_or_publication() {
        const MARKER: &str = "IRONWIRE_PROBE_FIFO_CHILD";
        if std::env::var_os(MARKER).is_none() {
            bounded_child(
                "embed::lock::tests::planted_fifo_cannot_block_legacy_reads_or_publication",
                MARKER,
                false,
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(read(&path), None);
        assert!(open_regular(&path, true).is_err());
    }
}
