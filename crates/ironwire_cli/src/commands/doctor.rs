//! `ironwire doctor` — verify every connection end to end.
//!
//! The point of this command is that it makes a *real* request. A config that
//! parses and a credential that exists prove nothing; the failure modes that
//! matter (an expired token, a beta flag the provider stopped accepting, a
//! model the account is not entitled to) only show up on the wire.

use anyhow::Result;

use super::control_client::ControlClient;

/// Check the daemon and each backend.
pub(crate) async fn run(port: Option<u16>) -> Result<()> {
    let status = ControlClient::new(port)?.status().await?;
    println!("daemon        ok — 127.0.0.1:{}", status.port);

    if status.backends.is_empty() {
        println!("backends      none configured");
        println!();
        println!("Run `ironwire connect claude --subscription`, or set ANTHROPIC_API_KEY");
        println!("and restart the daemon.");
        return Ok(());
    }

    for backend in &status.backends {
        let verdict = if !backend.authenticated {
            format!(
                "no credential — {}",
                backend.detail.as_deref().unwrap_or("unknown")
            )
        } else if !backend.consented {
            "awaiting consent".to_string()
        } else {
            "ok".to_string()
        };
        println!("{:<14}{verdict}", backend.id);
    }

    println!();
    println!("Live probe (a real 1-token request per backend) lands with the");
    println!("conformance harness in M1 — see docs/PROTOCOL.md §7.4.");
    Ok(())
}
