//! Keeping the trace ledger from growing forever.
//!
//! Capture is on by default, which means every install accumulates a SQLite
//! file in a dotdir that nobody is watching. Without this it grows for the life
//! of the machine — not fast, but monotonically, and the first time anyone
//! notices is when they go looking for disk.
//!
//! Pruning runs in the daemon rather than as a command the user has to
//! remember, for the same reason: a maintenance step that requires knowing it
//! exists is a maintenance step that does not happen.

use std::time::Duration;

use chrono::Utc;
use ironwire_ledger::Ledger;

/// How often the daemon prunes.
///
/// Daily. The work is one `DELETE` against an indexed column and the data is
/// only interesting at day granularity anyway, so anything more frequent is
/// wasted wakeups on a laptop.
const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Delay before the first prune, so startup is never spent on housekeeping
/// while the user is waiting to make their first request.
const FIRST_DELAY: Duration = Duration::from_secs(5 * 60);

/// Start the background prune, unless retention is disabled.
pub(crate) fn spawn(ledger: Option<Ledger>, retain_days: u32) {
    // `0` means "keep everything", which is a legitimate choice for someone
    // doing long-horizon analysis. It has to be chosen, not defaulted into.
    let Some(ledger) = ledger.filter(|_| retain_days > 0) else {
        return;
    };
    let retain = chrono::Duration::days(i64::from(retain_days));

    tokio::spawn(async move {
        tokio::time::sleep(FIRST_DELAY).await;
        loop {
            match ledger.prune(Utc::now(), retain) {
                Ok(0) => tracing::debug!("trace ledger: nothing to prune"),
                Ok(removed) => tracing::info!(
                    removed,
                    retain_days,
                    "pruned exchanges older than the retention window"
                ),
                // Never fatal. A ledger problem must not stop the proxy from
                // doing its actual job.
                Err(error) => tracing::warn!(%error, "could not prune the trace ledger"),
            }
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_of_zero_starts_nothing() {
        // Asserting it does not panic is the whole point: `spawn` with no
        // runtime would, and this path must be reachable from a sync context.
        spawn(None, 0);
        spawn(None, 90);
    }
}
