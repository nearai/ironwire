//! The local trace ledger.
//!
//! Every exchange IronWire routes is recorded here, on this machine, in
//! `$IRONWIRE_HOME/ledger.sqlite`. Nothing is uploaded (`docs/TRUST.md` §4).
//!
//! The ordering matters: the ledger has to be worth having for a user who will
//! *never* share anything — `ironwire log`, cost attribution, "what did my
//! agent actually send before it did that". If the feature only paid off when
//! uploaded, the incentive would be to nudge people into uploading, and that is
//! how trust in this position gets spent.
//!
//! Bodies are **not** recorded unless `capture.bodies = true`. They contain the
//! user's source code.
#![warn(missing_docs)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Price an exchange from its observed token counts.
///
/// Delegates to `ironclaw_common::llm_costs` — the price table every NEAR AI
/// surface already shares. A second copy would drift the moment a provider
/// changes a rate, and a confidently wrong cost is worse than none.
///
/// Returns `None` when nothing was observed, so an unpriced exchange never
/// looks free.
#[must_use]
pub fn price(model: &str, usage: Option<(u32, u32, u32, u32)>) -> Option<f64> {
    let (input, output, cache_read, cache_write) = usage?;
    let cost =
        ironclaw_common::llm_costs::price_usage(model, input, output, cache_read, cache_write);
    cost.total_cost.to_string().parse().ok()
}

/// Failure reading or writing the ledger.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// SQLite said no.
    #[error("trace ledger: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, LedgerError>;

/// One routed exchange.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exchange {
    /// When the request arrived.
    pub started_at: DateTime<Utc>,
    /// Milliseconds until the first response byte reached the client.
    pub ttfb_ms: Option<i64>,
    /// Milliseconds until the response finished.
    pub total_ms: Option<i64>,
    /// Which façade received it.
    pub facade: String,
    /// Path beneath the façade.
    pub path: String,
    /// Opaque conversation key. Carries no content.
    pub conversation: String,
    /// Backend that served it.
    pub backend: String,
    /// Model requested by the client.
    pub requested_model: Option<String>,
    /// Model that actually served it, as the provider reported.
    pub served_model: Option<String>,
    /// Fidelity rung.
    pub rung: String,
    /// Backends tried and rejected before this one succeeded.
    pub attempts: i64,
    /// Uncached input tokens.
    pub input_tokens: Option<i64>,
    /// Tokens read from the prompt cache.
    pub cache_read_tokens: Option<i64>,
    /// Tokens written to the prompt cache.
    pub cache_write_tokens: Option<i64>,
    /// Output tokens.
    pub output_tokens: Option<i64>,
    /// USD this exchange cost, priced from the observed token counts.
    ///
    /// `None` when the provider reported no usage — the same rule as everywhere
    /// else: a fabricated zero would understate what the user actually spent.
    /// Subscription capacity still carries a price, because "what this would
    /// have cost on the meter" is the number that makes a subscription legible.
    pub cost_usd: Option<f64>,
    /// HTTP status returned to the client.
    pub status: i64,
    /// Error, when the exchange failed.
    pub error: Option<String>,
}

impl Exchange {
    /// Whether the provider reported any usage at all.
    ///
    /// An exchange with no usage is recorded as such rather than as zero: a
    /// fabricated zero would silently understate a user's spend.
    #[must_use]
    pub fn has_usage(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_read_tokens.is_some()
    }
}

/// Aggregate view over a window, for `ironwire log --summary`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// Exchanges in the window.
    pub exchanges: i64,
    /// Exchanges whose usage the provider never reported.
    pub without_usage: i64,
    /// Summed uncached input tokens.
    pub input_tokens: i64,
    /// Summed cache reads.
    pub cache_read_tokens: i64,
    /// Summed output tokens.
    pub output_tokens: i64,
    /// Summed USD across the window, priced from observed usage.
    pub cost_usd: f64,
    /// Per-backend exchange counts, descending.
    pub by_backend: Vec<(String, i64)>,
}

/// The ledger.
///
/// Cloneable and shareable: the connection is behind a mutex because SQLite
/// writes here are tiny, infrequent relative to inference, and always off the
/// response path.
#[derive(Clone)]
pub struct Ledger {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger").finish_non_exhaustive()
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS exchanges (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at         TEXT    NOT NULL,
    ttfb_ms            INTEGER,
    total_ms           INTEGER,
    facade             TEXT    NOT NULL,
    path               TEXT    NOT NULL,
    conversation       TEXT    NOT NULL,
    backend            TEXT    NOT NULL,
    requested_model    TEXT,
    served_model       TEXT,
    rung               TEXT    NOT NULL,
    attempts           INTEGER NOT NULL,
    input_tokens       INTEGER,
    cache_read_tokens  INTEGER,
    cache_write_tokens INTEGER,
    output_tokens      INTEGER,
    cost_usd           REAL,
    status             INTEGER NOT NULL,
    error              TEXT
);
CREATE INDEX IF NOT EXISTS exchanges_started_at ON exchanges (started_at);
CREATE INDEX IF NOT EXISTS exchanges_conversation ON exchanges (conversation);
";

impl Ledger {
    /// Open (and migrate) the ledger at `path`.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Sqlite`] when the file cannot be opened or migrated.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = rusqlite::Connection::open(path)?;
        Self::init(conn)
    }

    /// Open an in-memory ledger. Used by tests.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Sqlite`] when the schema cannot be created.
    pub fn in_memory() -> Result<Self> {
        Self::init(rusqlite::Connection::open_in_memory()?)
    }

    fn init(conn: rusqlite::Connection) -> Result<Self> {
        // WAL so a `ironwire log` read never blocks the daemon's writes.
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Append an exchange.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Sqlite`] on a write failure. Callers on the response path
    /// must log and continue: a ledger problem must never fail a user's
    /// inference request.
    pub fn record(&self, exchange: &Exchange) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO exchanges (
                started_at, ttfb_ms, total_ms, facade, path, conversation, backend,
                requested_model, served_model, rung, attempts,
                input_tokens, cache_read_tokens, cache_write_tokens, output_tokens,
                cost_usd, status, error
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            rusqlite::params![
                exchange.started_at.to_rfc3339(),
                exchange.ttfb_ms,
                exchange.total_ms,
                exchange.facade,
                exchange.path,
                exchange.conversation,
                exchange.backend,
                exchange.requested_model,
                exchange.served_model,
                exchange.rung,
                exchange.attempts,
                exchange.input_tokens,
                exchange.cache_read_tokens,
                exchange.cache_write_tokens,
                exchange.output_tokens,
                exchange.cost_usd,
                exchange.status,
                exchange.error,
            ],
        )?;
        Ok(())
    }

    /// Most recent exchanges, newest first.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Sqlite`] on a read failure.
    pub fn recent(&self, limit: usize) -> Result<Vec<Exchange>> {
        let conn = self.lock();
        let mut statement = conn.prepare(
            "SELECT started_at, ttfb_ms, total_ms, facade, path, conversation, backend,
                    requested_model, served_model, rung, attempts,
                    input_tokens, cache_read_tokens, cache_write_tokens, output_tokens,
                    cost_usd, status, error
             FROM exchanges ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok(Exchange {
                started_at: parse_time(&row.get::<_, String>(0)?),
                ttfb_ms: row.get(1)?,
                total_ms: row.get(2)?,
                facade: row.get(3)?,
                path: row.get(4)?,
                conversation: row.get(5)?,
                backend: row.get(6)?,
                requested_model: row.get(7)?,
                served_model: row.get(8)?,
                rung: row.get(9)?,
                attempts: row.get(10)?,
                input_tokens: row.get(11)?,
                cache_read_tokens: row.get(12)?,
                cache_write_tokens: row.get(13)?,
                output_tokens: row.get(14)?,
                cost_usd: row.get(15)?,
                status: row.get(16)?,
                error: row.get(17)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(LedgerError::from)
    }

    /// Aggregate the last `window`.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Sqlite`] on a read failure.
    pub fn summary(&self, since: DateTime<Utc>) -> Result<Summary> {
        let conn = self.lock();
        let cutoff = since.to_rfc3339();

        let mut summary: Summary = conn.query_row(
            "SELECT COUNT(*),
                    SUM(CASE WHEN input_tokens IS NULL AND output_tokens IS NULL
                             AND cache_read_tokens IS NULL THEN 1 ELSE 0 END),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cost_usd), 0.0)
             FROM exchanges WHERE started_at >= ?1",
            [&cutoff],
            |row| {
                Ok(Summary {
                    exchanges: row.get(0)?,
                    without_usage: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    input_tokens: row.get(2)?,
                    cache_read_tokens: row.get(3)?,
                    output_tokens: row.get(4)?,
                    cost_usd: row.get(5)?,
                    by_backend: Vec::new(),
                })
            },
        )?;

        let mut statement = conn.prepare(
            "SELECT backend, COUNT(*) FROM exchanges WHERE started_at >= ?1
             GROUP BY backend ORDER BY COUNT(*) DESC",
        )?;
        summary.by_backend = statement
            .query_map([&cutoff], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(summary)
    }

    /// Drop records older than `retain`, keeping the file bounded.
    ///
    /// # Errors
    ///
    /// [`LedgerError::Sqlite`] on a write failure.
    pub fn prune(&self, now: DateTime<Utc>, retain: Duration) -> Result<usize> {
        let conn = self.lock();
        let cutoff = (now - retain).to_rfc3339();
        Ok(conn.execute("DELETE FROM exchanges WHERE started_at < ?1", [cutoff])?)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        match self.conn.lock() {
            Ok(conn) => conn,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Timestamps are written by us in RFC 3339, so a parse failure means the row
/// was hand-edited. Falling back to the epoch keeps `ironwire log` working
/// instead of failing the whole query over one bad row.
fn parse_time(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|_| DateTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(offset: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + offset, 0).expect("valid timestamp")
    }

    fn exchange(backend: &str, at_secs: i64) -> Exchange {
        Exchange {
            started_at: at(at_secs),
            ttfb_ms: Some(420),
            total_ms: Some(9_100),
            facade: "anthropic".into(),
            path: "/v1/messages".into(),
            conversation: "c-1".into(),
            backend: backend.into(),
            requested_model: Some("claude-opus-4-6".into()),
            served_model: Some("claude-opus-4-6".into()),
            rung: "preferred".into(),
            attempts: 1,
            input_tokens: Some(12),
            cache_read_tokens: Some(98_000),
            cache_write_tokens: Some(2_048),
            output_tokens: Some(137),
            cost_usd: Some(0.42),
            status: 200,
            error: None,
        }
    }

    #[test]
    fn records_round_trip() {
        let ledger = Ledger::in_memory().expect("opens");
        let written = exchange("claude-sub", 0);
        ledger.record(&written).expect("records");
        let read = ledger.recent(10).expect("reads");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0], written);
    }

    #[test]
    fn recent_is_newest_first_and_respects_the_limit() {
        let ledger = Ledger::in_memory().expect("opens");
        for i in 0..5 {
            ledger.record(&exchange("claude-sub", i)).expect("records");
        }
        let read = ledger.recent(3).expect("reads");
        assert_eq!(read.len(), 3);
        assert_eq!(read[0].started_at, at(4));
        assert_eq!(read[2].started_at, at(2));
    }

    #[test]
    fn an_exchange_with_no_reported_usage_is_counted_not_zeroed() {
        // Recording a fabricated zero would understate the user's real spend.
        let ledger = Ledger::in_memory().expect("opens");
        let mut unknown = exchange("claude-sub", 0);
        unknown.input_tokens = None;
        unknown.cache_read_tokens = None;
        unknown.cache_write_tokens = None;
        unknown.output_tokens = None;
        unknown.cost_usd = None;
        assert!(!unknown.has_usage());
        ledger.record(&unknown).expect("records");
        ledger.record(&exchange("claude-sub", 1)).expect("records");

        let summary = ledger.summary(at(-1)).expect("summarises");
        assert_eq!(summary.exchanges, 2);
        assert_eq!(summary.without_usage, 1);
        assert_eq!(summary.output_tokens, 137, "only the reported one counts");
    }

    #[test]
    fn the_summary_groups_by_backend() {
        let ledger = Ledger::in_memory().expect("opens");
        for i in 0..3 {
            ledger.record(&exchange("claude-sub", i)).expect("records");
        }
        ledger
            .record(&exchange("anthropic-key", 3))
            .expect("records");

        let summary = ledger.summary(at(-1)).expect("summarises");
        assert!((summary.cost_usd - 4.0 * 0.42).abs() < 1e-9);
        assert_eq!(
            summary.by_backend,
            vec![
                ("claude-sub".to_string(), 3),
                ("anthropic-key".to_string(), 1)
            ]
        );
        assert_eq!(summary.cache_read_tokens, 98_000 * 4);
    }

    #[test]
    fn the_summary_window_excludes_older_records() {
        let ledger = Ledger::in_memory().expect("opens");
        ledger.record(&exchange("claude-sub", 0)).expect("records");
        ledger
            .record(&exchange("claude-sub", 10_000))
            .expect("records");
        let summary = ledger.summary(at(5_000)).expect("summarises");
        assert_eq!(summary.exchanges, 1);
    }

    #[test]
    fn pruning_bounds_the_file() {
        let ledger = Ledger::in_memory().expect("opens");
        ledger.record(&exchange("claude-sub", 0)).expect("records");
        ledger
            .record(&exchange("claude-sub", 100_000))
            .expect("records");
        let removed = ledger
            .prune(at(100_000), Duration::seconds(50_000))
            .expect("prunes");
        assert_eq!(removed, 1);
        assert_eq!(ledger.recent(10).expect("reads").len(), 1);
    }

    #[test]
    fn a_failed_exchange_records_its_error() {
        let ledger = Ledger::in_memory().expect("opens");
        let mut failed = exchange("claude-sub", 0);
        failed.status = 429;
        failed.error = Some("rate limited".into());
        failed.attempts = 2;
        ledger.record(&failed).expect("records");
        let read = ledger.recent(1).expect("reads");
        assert_eq!(read[0].error.as_deref(), Some("rate limited"));
        assert_eq!(read[0].attempts, 2);
    }

    #[test]
    fn cost_comes_from_the_shared_price_table_and_is_none_without_usage() {
        // A subscription turn still gets a price: "what this would have cost on
        // the meter" is what makes the subscription's value legible.
        let priced =
            price("claude-opus-4-6", Some((12, 137, 98_000, 2_048))).expect("a known model prices");
        assert!(priced > 0.0, "a paid model must not price at zero");
        // Cache reads are discounted, so the same tokens billed fresh cost more.
        let fresh = price("claude-opus-4-6", Some((98_012, 137, 0, 2_048))).expect("prices");
        assert!(
            fresh > priced,
            "cache reads should be cheaper: {fresh} vs {priced}"
        );
        assert_eq!(price("claude-opus-4-6", None), None);
    }

    #[test]
    fn it_survives_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ledger.sqlite");
        {
            let ledger = Ledger::open(&path).expect("opens");
            ledger.record(&exchange("claude-sub", 0)).expect("records");
        }
        let ledger = Ledger::open(&path).expect("reopens");
        assert_eq!(ledger.recent(10).expect("reads").len(), 1);
    }
}
