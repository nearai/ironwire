//! Bounded verbatim evidence storage with durable, multi-owner snapshot leases.
//! Payload files belong only to this store; no API accepts a filesystem path.
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Hard ceiling for one request or response.
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Hard maximum retention, including renewed leases.
pub const MAX_LIFETIME_SECONDS: i64 = 7 * 86400;

/// Label-only errors: payloads, identities and paths never enter diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    /// Invalid parameters or corrupt state.
    #[error("token-capture-invalid")]
    Invalid,
    /// Capture absent, expired, or already being removed.
    #[error("token-capture-unavailable")]
    Unavailable,
    /// Byte/count budget reached. Inference may continue without capture.
    #[error("token-capture-capacity")]
    Capacity,
    /// Persistence failed; callers must retain their pending material.
    #[error("token-capture-storage")]
    Storage,
}
impl From<rusqlite::Error> for SpoolError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Storage
    }
}
impl From<std::io::Error> for SpoolError {
    fn from(_: std::io::Error) -> Self {
        Self::Storage
    }
}
type Result<T> = std::result::Result<T, SpoolError>;

/// Content-free reference to one immutable upstream exchange.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureDescriptor {
    /// Opaque, persistent identity of this spool database.
    pub store_id: String,
    /// Random identity; independent of reusable ledger row IDs.
    pub capture_id: String,
    /// Local ledger row, for exact association only.
    pub ledger_id: i64,
    /// Exact upstream request digest.
    pub request_digest: String,
    /// Exact upstream response digest.
    pub response_digest: String,
    /// Upstream wire name.
    pub protocol: String,
    /// Whether response bytes contain SSE.
    pub streaming: bool,
}
/// A lease grants access to an exact immutable snapshot, not a whole session.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureLease {
    /// Random release/read handle.
    pub lease_id: String,
    /// Destination-specific owner chosen by the authenticated client.
    pub owner: String,
    /// Binds ordered capture identities and digests.
    pub snapshot_digest: String,
    /// Expiration in Unix seconds.
    pub expires_at: i64,
    /// Captures pinned by this lease.
    pub captures: Vec<CaptureDescriptor>,
}
/// A single connection serializes transitions; SQLite also excludes other processes.
pub struct TokenSpool {
    dir: PathBuf,
    conn: Mutex<Connection>,
    budget: u64,
    retention: i64,
}
fn valid_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_./:".contains(&c))
}
fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(SpoolError::Invalid);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
impl TokenSpool {
    /// Open a private store. The parent must be the daemon's owned home.
    /// Windows inherits that home's user-only ACL.
    pub fn open(dir: &Path, budget: u64, retention: i64) -> Result<Self> {
        if budget == 0
            || budget > 512 * 1024 * 1024
            || !(1..=MAX_LIFETIME_SECONDS).contains(&retention)
        {
            return Err(SpoolError::Invalid);
        }
        private_dir(dir)?;
        let db = dir.join("spool.sqlite");
        if std::fs::symlink_metadata(&db).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink())
        {
            return Err(SpoolError::Invalid);
        }
        let conn = Connection::open(&db)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600))?;
        }
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA journal_mode=DELETE;
            CREATE TABLE IF NOT EXISTS identity(id TEXT NOT NULL);
            INSERT INTO identity SELECT lower(hex(randomblob(16))) WHERE NOT EXISTS(SELECT 1 FROM identity);
            CREATE TABLE IF NOT EXISTS captures(id TEXT PRIMARY KEY, session_hash TEXT NOT NULL, descriptor TEXT NOT NULL, created INTEGER NOT NULL, expires INTEGER NOT NULL, deleting INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS leases(id TEXT PRIMARY KEY, owner TEXT NOT NULL, digest TEXT NOT NULL, expires INTEGER NOT NULL, released INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS members(lease TEXT NOT NULL, capture TEXT NOT NULL, PRIMARY KEY(lease,capture));")?;
        sync_dir(dir)?;
        Ok(Self {
            dir: dir.into(),
            conn: Mutex::new(conn),
            budget,
            retention,
        })
    }
    /// Persistent identity of this spool, including after a daemon restart.
    pub fn store_id(&self) -> Result<String> {
        self.conn
            .lock()
            .map_err(|_| SpoolError::Storage)?
            .query_row("SELECT id FROM identity", [], |row| row.get(0))
            .map_err(Into::into)
    }
    fn file(&self, id: &str, suffix: &str) -> Result<PathBuf> {
        if !valid_id(id) {
            return Err(SpoolError::Invalid);
        }
        Ok(self.dir.join(format!("{id}.{suffix}")))
    }
    fn write_body(&self, id: &str, suffix: &str, body: &[u8]) -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(self.file(id, suffix)?)?;
        file.write_all(body)?;
        file.sync_all()?;
        Ok(())
    }
    fn disk_usage(&self) -> Result<u64> {
        let mut total = 0u64;
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry
                .path()
                .extension()
                .is_some_and(|e| e == "req" || e == "res")
            {
                let meta = std::fs::symlink_metadata(entry.path())?;
                if !meta.is_file() || meta.file_type().is_symlink() {
                    return Err(SpoolError::Invalid);
                }
                total = total.checked_add(meta.len()).ok_or(SpoolError::Capacity)?;
            }
        }
        Ok(total)
    }
    /// Persist exact bodies after a completed exchange. Failure never fails inference.
    pub fn record(
        &self,
        session: &str,
        ledger_id: i64,
        protocol: &str,
        streaming: bool,
        bodies: (&[u8], &[u8]),
        now: i64,
    ) -> Result<CaptureDescriptor> {
        let (request, response) = bodies;
        if session.is_empty()
            || session.len() > 4096
            || ledger_id < 1
            || !label(protocol)
            || now < 0
        {
            return Err(SpoolError::Invalid);
        }
        if request.len() > MAX_BODY_BYTES || response.len() > MAX_BODY_BYTES {
            return Err(SpoolError::Capacity);
        }
        let mut conn = self.conn.lock().map_err(|_| SpoolError::Storage)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row("SELECT count(*) FROM captures", [], |r| r.get(0))?;
        if count >= 4096
            || self
                .disk_usage()?
                .saturating_add((request.len() + response.len()) as u64)
                > self.budget
        {
            return Err(SpoolError::Capacity);
        }
        let descriptor = CaptureDescriptor {
            store_id: tx.query_row("SELECT id FROM identity", [], |r| r.get(0))?,
            capture_id: tx.query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))?,
            ledger_id,
            request_digest: digest(request),
            response_digest: digest(response),
            protocol: protocol.into(),
            streaming,
        };
        // Publish metadata only after both files are synced. Crashes leave
        // unreferenced files, counted against the budget and swept by maintenance.
        let write = self
            .write_body(&descriptor.capture_id, "req", request)
            .and_then(|()| self.write_body(&descriptor.capture_id, "res", response))
            .and_then(|()| sync_dir(&self.dir));
        if let Err(error) = write {
            for suffix in ["req", "res"] {
                let _ = std::fs::remove_file(self.file(&descriptor.capture_id, suffix)?);
            }
            return Err(error);
        }
        let encoded = serde_json::to_string(&descriptor).map_err(|_| SpoolError::Invalid)?;
        tx.execute("INSERT INTO captures(id,session_hash,descriptor,created,expires) VALUES(?1,?2,?3,?4,?5)", params![descriptor.capture_id, digest(session.as_bytes()), encoded, now, now.checked_add(self.retention).ok_or(SpoolError::Invalid)?])?;
        tx.commit()?;
        Ok(descriptor)
    }
    /// Lease exact capture IDs associated with one explicitly selected session.
    pub fn acquire(
        &self,
        session: &str,
        ids: &[String],
        owner: &str,
        now: i64,
        seconds: i64,
    ) -> Result<CaptureLease> {
        if ids.is_empty()
            || ids.len() > 128
            || !label(owner)
            || !(1..=MAX_LIFETIME_SECONDS).contains(&seconds)
        {
            return Err(SpoolError::Invalid);
        }
        let mut conn = self.conn.lock().map_err(|_| SpoolError::Storage)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row("SELECT count(*) FROM leases WHERE released=0", [], |r| {
            r.get(0)
        })?;
        if count >= 4096 {
            return Err(SpoolError::Capacity);
        }
        let mut captures = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut expires = now.checked_add(seconds).ok_or(SpoolError::Invalid)?;
        let mut hasher = Sha256::new();
        for id in ids {
            if !valid_id(id) || !seen.insert(id) {
                return Err(SpoolError::Invalid);
            }
            let (encoded, created): (String,i64) = tx.query_row("SELECT descriptor,created FROM captures WHERE id=?1 AND session_hash=?2 AND deleting=0 AND expires>?3", params![id, digest(session.as_bytes()), now], |r| Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or(SpoolError::Unavailable)?;
            let descriptor: CaptureDescriptor =
                serde_json::from_str(&encoded).map_err(|_| SpoolError::Invalid)?;
            expires = expires.min(
                created
                    .checked_add(MAX_LIFETIME_SECONDS)
                    .ok_or(SpoolError::Invalid)?,
            );
            hasher.update(descriptor.store_id.as_bytes());
            hasher.update(id.as_bytes());
            hasher.update(descriptor.request_digest.as_bytes());
            hasher.update(descriptor.response_digest.as_bytes());
            captures.push(descriptor);
        }
        if expires <= now {
            return Err(SpoolError::Unavailable);
        }
        let lease = CaptureLease {
            lease_id: tx.query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))?,
            owner: owner.into(),
            snapshot_digest: format!("{:x}", hasher.finalize()),
            expires_at: expires,
            captures,
        };
        tx.execute(
            "INSERT INTO leases(id,owner,digest,expires) VALUES(?1,?2,?3,?4)",
            params![lease.lease_id, owner, lease.snapshot_digest, expires],
        )?;
        for id in ids {
            tx.execute(
                "INSERT INTO members VALUES(?1,?2)",
                params![lease.lease_id, id],
            )?;
        }
        tx.commit()?;
        Ok(lease)
    }
    /// Extend an active lease without changing its capture snapshot. The
    /// earliest capture's hard lifetime remains an absolute upper bound.
    pub fn renew(
        &self,
        lease: &str,
        owner: &str,
        snapshot: &str,
        now: i64,
        seconds: i64,
    ) -> Result<i64> {
        if !(1..=MAX_LIFETIME_SECONDS).contains(&seconds) {
            return Err(SpoolError::Invalid);
        }
        let mut conn = self.conn.lock().map_err(|_| SpoolError::Storage)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expires: i64 = tx.query_row("SELECT expires FROM leases WHERE id=?1 AND owner=?2 AND digest=?3 AND released=0 AND expires>?4", params![lease,owner,snapshot,now], |r| r.get(0)).optional()?.ok_or(SpoolError::Unavailable)?;
        let oldest: Option<i64> = tx.query_row("SELECT min(c.created) FROM captures c JOIN members m ON m.capture=c.id WHERE m.lease=?1 AND c.deleting=0", [lease], |r| r.get(0))?;
        let ceiling = oldest
            .ok_or(SpoolError::Unavailable)?
            .checked_add(MAX_LIFETIME_SECONDS)
            .ok_or(SpoolError::Invalid)?;
        let next = expires
            .max(now.checked_add(seconds).ok_or(SpoolError::Invalid)?)
            .min(ceiling);
        if next <= now {
            return Err(SpoolError::Unavailable);
        }
        tx.execute(
            "UPDATE leases SET expires=?2 WHERE id=?1",
            params![lease, next],
        )?;
        tx.commit()?;
        Ok(next)
    }
    /// Enumerate content-free references for one exact session, oldest first.
    pub fn list(&self, session: &str, now: i64) -> Result<Vec<CaptureDescriptor>> {
        let conn = self.conn.lock().map_err(|_| SpoolError::Storage)?;
        let mut statement = conn.prepare("SELECT descriptor FROM captures WHERE session_hash=?1 AND deleting=0 AND expires>?2 ORDER BY created,id LIMIT 128")?;
        let encoded = statement
            .query_map(params![digest(session.as_bytes()), now], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        encoded
            .into_iter()
            .map(|s| serde_json::from_str(&s).map_err(|_| SpoolError::Invalid))
            .collect()
    }
    /// Read an immutable capture while holding the same lock as pruning.
    pub fn read(
        &self,
        lease: &str,
        owner: &str,
        capture: &str,
        now: i64,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut conn = self.conn.lock().map_err(|_| SpoolError::Storage)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let encoded: String = tx.query_row("SELECT c.descriptor FROM captures c JOIN members m ON m.capture=c.id JOIN leases l ON l.id=m.lease WHERE c.id=?1 AND l.id=?2 AND l.owner=?3 AND l.released=0 AND l.expires>?4 AND c.deleting=0", params![capture,lease,owner,now], |r| r.get(0)).optional()?.ok_or(SpoolError::Unavailable)?;
        let descriptor: CaptureDescriptor =
            serde_json::from_str(&encoded).map_err(|_| SpoolError::Invalid)?;
        let read = |suffix| -> Result<Vec<u8>> {
            let path = self.file(capture, suffix)?;
            let meta = std::fs::symlink_metadata(&path)?;
            if !meta.is_file()
                || meta.file_type().is_symlink()
                || meta.len() > MAX_BODY_BYTES as u64
            {
                return Err(SpoolError::Invalid);
            }
            Ok(std::fs::read(path)?)
        };
        let request = read("req")?;
        let response = read("res")?;
        if digest(&request) != descriptor.request_digest
            || digest(&response) != descriptor.response_digest
        {
            return Err(SpoolError::Invalid);
        }
        tx.commit()?;
        Ok((request, response))
    }
    /// Release only this owner/snapshot. Last-owner release queues exact files
    /// for deletion; late captures and other destinations remain untouched.
    pub fn release(&self, lease: &str, owner: &str, snapshot: &str, now: i64) -> Result<()> {
        let mut conn = self.conn.lock().map_err(|_| SpoolError::Storage)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM leases WHERE id=?1 AND owner=?2 AND digest=?3)",
            params![lease, owner, snapshot],
            |r| r.get(0),
        )?;
        if !found {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM leases WHERE id=?1)",
                [lease],
                |r| r.get(0),
            )?;
            if exists {
                return Err(SpoolError::Unavailable);
            }
            // Expired leases are eventually pruned. A retry for an absent
            // random lease is already complete and deletes no additional data.
            tx.commit()?;
            return Ok(());
        }
        tx.execute("UPDATE leases SET released=1 WHERE id=?1", [lease])?;
        tx.execute("UPDATE captures SET deleting=1 WHERE id IN(SELECT capture FROM members WHERE lease=?1) AND NOT EXISTS(SELECT 1 FROM members m JOIN leases l ON l.id=m.lease WHERE m.capture=captures.id AND l.released=0 AND l.expires>?2)", params![lease,now])?;
        tx.commit()?;
        drop(conn);
        self.prune(now).map(|_| ())
    }
    /// Retry deletion and reclaim expired/orphaned material under the DB lock.
    pub fn prune(&self, now: i64) -> Result<usize> {
        let mut conn = self.conn.lock().map_err(|_| SpoolError::Storage)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE captures SET deleting=1 WHERE expires<=?1 AND NOT EXISTS(SELECT 1 FROM members m JOIN leases l ON l.id=m.lease WHERE m.capture=captures.id AND l.released=0 AND l.expires>?1)", [now])?;
        let ids = {
            let mut q = tx.prepare("SELECT id FROM captures WHERE deleting=1")?;
            q.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for id in &ids {
            for suffix in ["req", "res"] {
                match std::fs::remove_file(self.file(id, suffix)?) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            tx.execute("DELETE FROM captures WHERE id=?1", [id])?;
        }
        // Published captures and in-flight writes share this transaction lock.
        // Orphan files left before a commit therefore cannot be live writers.
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some((id, suffix)) = name.rsplit_once('.') else {
                continue;
            };
            if !valid_id(id) || !matches!(suffix, "req" | "res") {
                continue;
            }
            let live: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM captures WHERE id=?1)",
                [id],
                |r| r.get(0),
            )?;
            if !live {
                std::fs::remove_file(entry.path())?;
            }
        }
        tx.execute(
            "DELETE FROM members WHERE lease IN(SELECT id FROM leases WHERE expires<=?1)",
            [now],
        )?;
        tx.execute("DELETE FROM leases WHERE expires<=?1", [now])?;
        sync_dir(&self.dir)?;
        tx.commit()?;
        Ok(ids.len())
    }
}
