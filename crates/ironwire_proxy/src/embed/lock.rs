//! Home ownership survives startup races and is held until draining completes.
use super::EmbedError;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub(super) struct Guard {
    _file: File,
    port_file: PathBuf,
    port: Option<u16>,
}

pub(super) async fn acquire(path: &Path, port: u16) -> Result<Guard, EmbedError> {
    // Do not unlink this inode: replacing a locked inode lets another process
    // lock the replacement while the first still owns the old one.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock.guard"))
        .map_err(|_| EmbedError::Paths)?;
    file.try_lock().map_err(|_| EmbedError::Lock {
        port: read(path).unwrap_or(port),
    })?;
    // Cooperate with older CLIs that only publish a health-probed port file.
    if let Some(held) = read(path) {
        let client = reqwest::Client::builder()
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
    })
}

fn read(path: &Path) -> Option<u16> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

impl Guard {
    pub(super) fn publish(&mut self, port: u16) -> Result<(), EmbedError> {
        let mut file = File::create(&self.port_file).map_err(|_| EmbedError::Paths)?;
        writeln!(file, "{port}").map_err(|_| EmbedError::Paths)?;
        self.port = Some(port);
        Ok(())
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.port.is_some() && read(&self.port_file) == self.port {
            let _ = std::fs::remove_file(&self.port_file);
        }
    }
}
