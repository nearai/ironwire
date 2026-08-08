//! CLI subcommands.
//!
//! Every command that inspects or changes routing goes through the daemon's
//! control API rather than reading state off disk, so the CLI and the menu bar
//! app can never disagree about what is happening (`docs/DESIGN.md` §6).

pub(crate) mod connect;
pub(crate) mod control_client;
pub(crate) mod doctor;
pub(crate) mod init;
pub(crate) mod log;
pub(crate) mod pin;
pub(crate) mod privacy;
pub(crate) mod quirks;
pub(crate) mod serve;
pub(crate) mod service;
pub(crate) mod status;
pub(crate) mod update;
pub(crate) mod watch;

use anyhow::{Context, Result};
use ironwire_core::config::PathsConfig;

/// Resolve `$IRONWIRE_HOME`, creating it with owner-only permissions.
pub(crate) fn paths() -> Result<PathsConfig> {
    let paths = PathsConfig::resolve().context("resolving $IRONWIRE_HOME")?;
    std::fs::create_dir_all(&paths.home)
        .with_context(|| format!("creating {}", paths.home.display()))?;
    restrict_permissions(&paths.home, 0o700)?;
    Ok(paths)
}

/// Read the control token, minting one on first use.
///
/// Loopback alone is not authorisation on a shared machine: this surface
/// exposes the ledger and can move a user's traffic (`docs/TRUST.md` §5).
pub(crate) fn control_token(paths: &PathsConfig) -> Result<String> {
    let path = paths.control_token_file();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let token = mint_token();
    std::fs::write(&path, &token).with_context(|| format!("writing {}", path.display()))?;
    restrict_permissions(&path, 0o600)?;
    Ok(token)
}

/// 256 bits from the OS CSPRNG, hex-encoded.
fn mint_token() -> String {
    // `getrandom` via `std` is not stable, so read the OS source directly.
    // Falling back to a time-derived value would be worse than failing, so we
    // do not: a predictable control token is a real local privilege escalation.
    let mut buf = [0u8; 32];
    #[cfg(unix)]
    {
        use std::io::Read;
        let mut f = std::fs::File::open("/dev/urandom").expect("/dev/urandom is available");
        f.read_exact(&mut buf).expect("/dev/urandom yields bytes");
    }
    #[cfg(not(unix))]
    {
        // On Windows, derive from a UUID v4, which uses the platform CSPRNG.
        for chunk in buf.chunks_mut(16) {
            let bytes = *uuid_v4_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(not(unix))]
fn uuid_v4_bytes() -> &'static [u8; 16] {
    unimplemented!("Windows token minting lands with the M4 packaging work")
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("restricting permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path, _mode: u32) -> Result<()> {
    // Windows ACLs land with the M4 packaging work; the directory still sits
    // under the user profile.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_tokens_are_long_and_not_repeated() {
        let a = mint_token();
        let b = mint_token();
        assert_eq!(a.len(), 64, "256 bits, hex-encoded");
        assert_ne!(
            a, b,
            "a predictable control token is a privilege escalation"
        );
    }
}
