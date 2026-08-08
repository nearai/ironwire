//! `ironwire doctor` — verify every connection end to end.
//!
//! This command makes **real network calls**. A config that parses and a
//! credential file that exists prove nothing; the failures that actually bite
//! — an expired token, a beta flag the provider stopped honouring, an account
//! not entitled to a model — only appear on the wire.

use anyhow::Result;

use super::control_client::ControlClient;

/// Check the daemon and each backend.
pub(crate) async fn run(port: Option<u16>) -> Result<()> {
    let client = ControlClient::new(port)?;
    let status = client.status().await?;
    println!("daemon        ok — 127.0.0.1:{}", status.port);

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
