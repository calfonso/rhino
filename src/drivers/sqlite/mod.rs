//! SQLite backend driver for rhino.
//!
//! Implements the [`Backend`] trait using SQLite with the same schema and
//! log-structured approach as kine. Revision IDs are monotonically increasing
//! row IDs; all mutations are appended as new rows. A background poll loop
//! detects new rows and broadcasts them to watchers.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteLockingMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous};
use sqlx::Row;
use tokio::sync::{mpsc, Mutex, Notify};
use tracing::{debug, error, trace, warn};

use crate::backend::{Backend, BackendError, Event, KeyValue, Result, WatchResult};

const COMPACT_REV_KEY: &str = "compact_rev_key";
const COMPACT_MIN_RETAIN: i64 = 1000;
const COMPACT_BATCH_SIZE: i64 = 1000;
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const COMPACT_INTERVAL: Duration = Duration::from_secs(300);

/// Configuration for the SQLite backend.
#[derive(Debug, Clone)]
pub struct SqliteConfig {
    /// Path to the SQLite database file. Defaults to `"./db/state.db"`.
    pub dsn: String,
    /// Interval between compaction runs. Set to `Duration::ZERO` to disable.
    pub compact_interval: Duration,
    /// Minimum number of recent revisions to retain during compaction.
    pub compact_min_retain: i64,
    /// Max revisions to compact per transaction batch.
    pub compact_batch_size: i64,
    /// Maximum number of connections in the pool. Defaults to 20.
    pub max_connections: u32,
}

impl Default for SqliteConfig {
    fn default() -> Self {
        Self {
            dsn: "./db/state.db".to_string(),
            compact_interval: COMPACT_INTERVAL,
            compact_min_retain: COMPACT_MIN_RETAIN,
            compact_batch_size: COMPACT_BATCH_SIZE,
            max_connections: 10,
        }
    }
}

/// Kine-style broadcaster: fans out events to per-subscriber channels.
/// Slow subscribers are disconnected (channel dropped) rather than lagging.
struct Broadcaster {
    subscribers: Mutex<Vec<mpsc::Sender<Vec<Event>>>>,
    started: AtomicBool,
}

impl Broadcaster {
    fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            started: AtomicBool::new(false),
        }
    }

    /// Subscribe to events. Returns a receiver with capacity 100 (matching kine).
    async fn subscribe(&self) -> mpsc::Receiver<Vec<Event>> {
        let (tx, rx) = mpsc::channel(100);
        self.subscribers.lock().await.push(tx);
        rx
    }

    /// Broadcast events to all subscribers. Slow subscribers (full channel) are dropped.
    async fn send(&self, events: Vec<Event>) {
        let mut subs = self.subscribers.lock().await;
        subs.retain(|tx| {
            // try_send: if the subscriber's channel is full, drop it
            match tx.try_send(events.clone()) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!("dropping slow watch subscriber");
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }
}

/// SQLite-backed implementation of the rhino [`Backend`] trait.
pub struct SqliteBackend {
    pool: SqlitePool,
    current_rev: Arc<AtomicI64>,
    notify: Arc<Notify>,
    broadcaster: Arc<Broadcaster>,
    config: SqliteConfig,
}

impl SqliteBackend {
    /// Create a new SQLite backend with the given configuration.
    /// The database file and pool are created immediately, but schema
    /// initialization and background tasks require calling [`Backend::start`].
    pub async fn new(config: SqliteConfig) -> std::result::Result<Self, BackendError> {
        // Ensure parent directory exists
        if let Some(parent) = std::path::Path::new(&config.dsn).parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                BackendError::Internal(format!("failed to create database directory: {e}"))
            })?;
        }

        let opts = SqliteConnectOptions::new()
            .filename(&config.dsn)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .locking_mode(SqliteLockingMode::Normal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(30))
            .pragma("cache_size", "-2000")
            .pragma("auto_vacuum", "incremental")
            .pragma("txlock", "immediate");

        // Run startup VACUUM on a single connection before creating the pool.
        // VACUUM requires exclusive access and can't run with pooled connections.
        {
            use sqlx::ConnectOptions;
            if let Ok(mut conn) = opts.clone().connect().await {
                if let Err(e) = sqlx::query("VACUUM").execute(&mut conn).await {
                    warn!("startup VACUUM failed (non-fatal): {e}");
                }
            }
        }

        let pool = SqlitePoolOptions::new()
            .max_connections(config.max_connections)
            .connect_with(opts)
            .await
            .map_err(|e| BackendError::Internal(format!("failed to open database: {e}")))?;

        Ok(Self {
            pool,
            current_rev: Arc::new(AtomicI64::new(0)),
            notify: Arc::new(Notify::new()),
            broadcaster: Arc::new(Broadcaster::new()),
            config,
        })
    }

    async fn setup_schema(&self) -> Result<()> {
        let statements = [
            "CREATE TABLE IF NOT EXISTS kine (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name INTEGER,
                created INTEGER,
                deleted INTEGER,
                create_revision INTEGER,
                prev_revision INTEGER,
                lease INTEGER,
                value BLOB,
                old_value BLOB
            )",
            "CREATE TABLE IF NOT EXISTS kine_current (
                name TEXT PRIMARY KEY,
                id INTEGER NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS kine_name_index ON kine (name)",
            "CREATE INDEX IF NOT EXISTS kine_name_id_index ON kine (name, id)",
            "CREATE INDEX IF NOT EXISTS kine_id_deleted_index ON kine (id, deleted)",
            "CREATE INDEX IF NOT EXISTS kine_prev_revision_index ON kine (prev_revision)",
            "CREATE UNIQUE INDEX IF NOT EXISTS kine_name_prev_revision_uindex ON kine (name, prev_revision)",
            "CREATE INDEX IF NOT EXISTS kine_id_compact_rev_key_with_prev_revision_index ON kine(id, name, prev_revision) WHERE name != 'compact_rev_key' AND prev_revision != 0",
        ];

        for stmt in &statements {
            sqlx::query(stmt)
                .execute(&self.pool)
                .await
                .map_err(|e| BackendError::Internal(format!("schema setup failed: {e}")))?;
        }

        // Populate kine_current from existing data (migration for existing databases)
        sqlx::query(
            "INSERT OR REPLACE INTO kine_current (name, id)
             SELECT name, MAX(id) FROM kine GROUP BY name",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| BackendError::Internal(format!("kine_current migration failed: {e}")))?;

        debug!("database schema and indexes are up to date");
        Ok(())
    }

    async fn ensure_compact_rev_key(&self) -> Result<()> {
        // Count compact_rev_key entries
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM kine WHERE name = ?")
            .bind(COMPACT_REV_KEY)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        if count.0 == 0 {
            self.insert(COMPACT_REV_KEY, true, false, 0, 0, 0, b"", b"")
                .await?;
        } else if count.0 > 1 {
            // Clean up duplicate compact_rev_key rows (matching kine's compactStart).
            // Keep only the one with the highest prev_revision.
            sqlx::query(
                "DELETE FROM kine WHERE name = ? AND id NOT IN (
                    SELECT id FROM kine WHERE name = ? ORDER BY prev_revision DESC LIMIT 1
                )",
            )
            .bind(COMPACT_REV_KEY)
            .bind(COMPACT_REV_KEY)
            .execute(&self.pool)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;
            debug!("cleaned up duplicate compact_rev_key rows");
        }
        Ok(())
    }

    /// Insert a row into the kine table, returning the new revision (row id).
    #[allow(clippy::too_many_arguments)]
    async fn insert(
        &self,
        key: &str,
        create: bool,
        delete: bool,
        create_revision: i64,
        prev_revision: i64,
        lease: i64,
        value: &[u8],
        old_value: &[u8],
    ) -> Result<i64> {
        let c = if create { 1i32 } else { 0 };
        let d = if delete { 1i32 } else { 0 };

        let result = sqlx::query(
            "INSERT INTO kine(name, created, deleted, create_revision, prev_revision, lease, value, old_value)
             VALUES(?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(key)
        .bind(c)
        .bind(d)
        .bind(create_revision)
        .bind(prev_revision)
        .bind(lease)
        .bind(value)
        .bind(old_value)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            // SQLite UNIQUE constraint violation → key already exists
            let msg = e.to_string();
            if msg.contains("UNIQUE constraint failed") {
                BackendError::KeyExists
            } else {
                BackendError::Internal(msg)
            }
        })?;

        let id = result.last_insert_rowid();

        // Update kine_current to point to this latest revision
        sqlx::query(
            "INSERT INTO kine_current (name, id) VALUES (?, ?)
             ON CONFLICT(name) DO UPDATE SET id = excluded.id",
        )
        .bind(key)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| BackendError::Internal(format!("kine_current update failed: {e}")))?;

        self.current_rev.store(id, Ordering::Release);
        self.notify.notify_waiters();
        Ok(id)
    }

    /// Fill a gap at the given revision with a placeholder record.
    async fn fill(&self, revision: i64) -> Result<()> {
        let name = format!("gap-{revision}");
        sqlx::query(
            "INSERT INTO kine(id, name, created, deleted, create_revision, prev_revision, lease, value, old_value)
             VALUES(?, ?, 0, 1, 0, 0, 0, NULL, NULL)",
        )
        .bind(revision)
        .bind(&name)
        .execute(&self.pool)
        .await
        .map_err(|e| BackendError::Internal(e.to_string()))?;
        Ok(())
    }

    /// Get the current (max) revision from the database.
    async fn db_current_revision(&self) -> Result<i64> {
        let row: (Option<i64>,) = sqlx::query_as("SELECT MAX(id) FROM kine")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;
        Ok(row.0.unwrap_or(0))
    }

    /// Get the compact revision from the database.
    async fn get_compact_revision(&self) -> Result<i64> {
        let row: (Option<i64>,) =
            sqlx::query_as("SELECT MAX(prev_revision) FROM kine WHERE name = ?")
                .bind(COMPACT_REV_KEY)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| BackendError::Internal(e.to_string()))?;
        Ok(row.0.unwrap_or(0))
    }

    /// Internal get: fetch a single key (the latest version).
    /// `include_deleted` controls whether deleted keys are returned.
    async fn get_internal(
        &self,
        key: &str,
        revision: i64,
        include_deleted: bool,
        keys_only: bool,
    ) -> Result<(i64, Option<Event>)> {
        let escaped = key.replace('_', "^_");
        let (rev, events) = self
            .list_internal(&escaped, "", 1, revision, include_deleted, keys_only)
            .await?;
        Ok((rev, events.into_iter().next()))
    }

    /// Core list query. Returns (current_revision, events).
    /// For current-revision queries (revision == 0), uses kine_current for O(keys) lookup.
    /// For historical queries (revision > 0), falls back to MAX(id) GROUP BY subquery.
    async fn list_internal(
        &self,
        prefix: &str,
        start_key: &str,
        limit: i64,
        revision: i64,
        include_deleted: bool,
        keys_only: bool,
    ) -> Result<(i64, Vec<Event>)> {
        let value_cols = if keys_only {
            ""
        } else {
            ", kv.value, kv.old_value"
        };

        let limit_clause = if limit > 0 {
            format!("LIMIT {limit}")
        } else {
            String::new()
        };

        let include_deleted_val: i32 = if include_deleted { 1 } else { 0 };

        let rows = if revision == 0 {
            // Current-revision query: use kine_current table (O(matching_keys))
            let start_where = if start_key.is_empty() {
                String::new()
            } else {
                "AND cur.name >= ?".to_string()
            };

            let sql = format!(
                "SELECT
                    (SELECT MAX(rkv.id) FROM kine AS rkv),
                    (SELECT MAX(crkv.prev_revision) FROM kine AS crkv WHERE crkv.name = '{COMPACT_REV_KEY}'),
                    kv.id AS theid,
                    kv.name AS thename,
                    kv.created,
                    kv.deleted,
                    kv.create_revision,
                    kv.prev_revision,
                    kv.lease
                    {value_cols}
                FROM kine AS kv
                JOIN kine_current AS cur ON cur.id = kv.id
                WHERE cur.name LIKE ? ESCAPE '^'
                {start_where}
                AND (kv.deleted = 0 OR ?)
                ORDER BY kv.name ASC
                {limit_clause}"
            );

            if start_key.is_empty() {
                sqlx::query(&sql)
                    .bind(prefix)
                    .bind(include_deleted_val)
                    .fetch_all(&self.pool)
                    .await
            } else {
                sqlx::query(&sql)
                    .bind(prefix)
                    .bind(start_key)
                    .bind(include_deleted_val)
                    .fetch_all(&self.pool)
                    .await
            }
        } else {
            // Historical query: must scan revisions up to the requested revision
            let start_where = if start_key.is_empty() {
                String::new()
            } else {
                "AND mkv.name >= ?".to_string()
            };

            let sql = format!(
                "SELECT
                    (SELECT MAX(rkv.id) FROM kine AS rkv),
                    (SELECT MAX(crkv.prev_revision) FROM kine AS crkv WHERE crkv.name = '{COMPACT_REV_KEY}'),
                    kv.id AS theid,
                    kv.name AS thename,
                    kv.created,
                    kv.deleted,
                    kv.create_revision,
                    kv.prev_revision,
                    kv.lease
                    {value_cols}
                FROM kine AS kv
                JOIN (
                    SELECT MAX(mkv.id) AS id
                    FROM kine AS mkv
                    WHERE mkv.name LIKE ? ESCAPE '^'
                    {start_where}
                    AND mkv.id <= ?
                    GROUP BY mkv.name
                ) AS maxkv ON maxkv.id = kv.id
                WHERE (kv.deleted = 0 OR ?)
                ORDER BY kv.name ASC
                {limit_clause}"
            );

            if start_key.is_empty() {
                sqlx::query(&sql)
                    .bind(prefix)
                    .bind(revision)
                    .bind(include_deleted_val)
                    .fetch_all(&self.pool)
                    .await
            } else {
                sqlx::query(&sql)
                    .bind(prefix)
                    .bind(start_key)
                    .bind(revision)
                    .bind(include_deleted_val)
                    .fetch_all(&self.pool)
                    .await
            }
        }
        .map_err(|e| BackendError::Internal(e.to_string()))?;

        let mut current_rev: i64 = 0;
        let mut compact_rev: i64 = 0;
        let mut events = Vec::with_capacity(rows.len());

        for row in &rows {
            let rev: Option<i64> = row.get(0);
            let compact: Option<i64> = row.get(1);
            current_rev = rev.unwrap_or(current_rev);
            compact_rev = compact.unwrap_or(compact_rev);

            let event = self.row_to_event(row, keys_only, false)?;
            events.push(event);
        }

        if current_rev == 0 {
            current_rev = self.cached_revision().await?;
        }

        if revision > current_rev {
            return Err(BackendError::FutureRev);
        }
        if revision > 0 && revision < compact_rev {
            return Err(BackendError::Compacted);
        }

        Ok((current_rev, events))
    }

    /// Query rows after a given revision (for poll/watch).
    async fn after(
        &self,
        prefix: &str,
        revision: i64,
        limit: i64,
    ) -> Result<(i64, i64, Vec<Event>)> {
        let limit_clause = if limit > 0 {
            format!("LIMIT {limit}")
        } else {
            String::new()
        };

        let sql = format!(
            "SELECT
                (SELECT MAX(rkv.id) FROM kine AS rkv),
                (SELECT MAX(crkv.prev_revision) FROM kine AS crkv WHERE crkv.name = '{COMPACT_REV_KEY}'),
                kv.id AS theid,
                kv.name AS thename,
                kv.created,
                kv.deleted,
                kv.create_revision,
                kv.prev_revision,
                kv.lease,
                kv.value,
                kv.old_value
            FROM kine AS kv
            WHERE kv.name LIKE ? ESCAPE '^' AND kv.id > ?
            ORDER BY kv.id ASC
            {limit_clause}"
        );

        let rows = sqlx::query(&sql)
            .bind(prefix)
            .bind(revision)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        let mut current_rev: i64 = 0;
        let mut compact_rev: i64 = 0;
        let mut events = Vec::with_capacity(rows.len());

        for row in &rows {
            let rev: Option<i64> = row.get(0);
            let compact: Option<i64> = row.get(1);
            current_rev = rev.unwrap_or(current_rev);
            compact_rev = compact.unwrap_or(compact_rev);

            let event = self.row_to_event(row, false, true)?;
            events.push(event);
        }

        Ok((current_rev, compact_rev, events))
    }

    /// Convert a database row to an Event.
    fn row_to_event(
        &self,
        row: &sqlx::sqlite::SqliteRow,
        keys_only: bool,
        with_old_value: bool,
    ) -> Result<Event> {
        let mod_revision: i64 = row.get(2);
        let name: String = row.get(3);
        let created: i32 = row.get(4);
        let deleted: i32 = row.get(5);
        let create_revision: i64 = row.get(6);
        let prev_revision: i64 = row.get(7);
        let lease: i64 = row.get(8);

        let value: Vec<u8> = if keys_only {
            vec![]
        } else {
            row.try_get::<Vec<u8>, _>(9).unwrap_or_default()
        };

        let old_value: Vec<u8> = if !keys_only && with_old_value {
            row.try_get::<Vec<u8>, _>(10).unwrap_or_default()
        } else {
            vec![]
        };

        let is_create = created != 0;
        let is_delete = deleted != 0;

        let actual_create_rev = if is_create { mod_revision } else { create_revision };

        let kv = KeyValue {
            key: name.clone(),
            value,
            version: 0, // computed by callers if needed
            create_revision: actual_create_rev,
            mod_revision,
            lease,
        };

        let prev_kv = if is_create {
            None
        } else {
            Some(KeyValue {
                key: name,
                value: old_value,
                version: 0,
                create_revision: actual_create_rev,
                mod_revision: prev_revision,
                lease,
            })
        };

        Ok(Event {
            create: is_create,
            delete: is_delete,
            kv,
            prev_kv,
        })
    }

    /// Return the cached current revision, fetching from DB if not yet cached.
    async fn cached_revision(&self) -> Result<i64> {
        let cached = self.current_rev.load(Ordering::Acquire);
        if cached != 0 {
            return Ok(cached);
        }
        let rev = self.db_current_revision().await?;
        self.current_rev
            .compare_exchange(0, rev, Ordering::AcqRel, Ordering::Acquire)
            .ok();
        Ok(self.current_rev.load(Ordering::Acquire))
    }

    /// Start background tasks lazily on first watch subscription (matching kine's startWatch).
    fn ensure_background_tasks(&self) {
        if self
            .broadcaster
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // Create shared references for background tasks
            let pool = self.pool.clone();
            let current_rev = self.current_rev.clone();
            let notify = self.notify.clone();
            let broadcaster = self.broadcaster.clone();
            let config = self.config.clone();

            let make_backend = move || SqliteBackend {
                pool: pool.clone(),
                current_rev: current_rev.clone(),
                notify: notify.clone(),
                broadcaster: broadcaster.clone(),
                config: config.clone(),
            };

            let poll_self = Arc::new(make_backend());
            let compact_self = Arc::new(make_backend());
            let ttl_self = Arc::new(make_backend());

            tokio::spawn(async move { poll_self.poll_loop().await });
            tokio::spawn(async move { compact_self.compact_loop().await });
            tokio::spawn(async move { ttl_self.ttl_loop().await });
            debug!("background tasks started (first watch subscription)");
        }
    }

    /// Background poll loop: detects new revisions and broadcasts events to watchers.
    async fn poll_loop(self: Arc<Self>) {
        let mut poll_revision = match self.db_current_revision().await {
            Ok(rev) => rev,
            Err(e) => {
                error!("poll loop failed to get initial revision: {e}");
                return;
            }
        };

        let mut interval = tokio::time::interval(POLL_INTERVAL);
        let mut skip: i64 = 0;
        let mut skip_time = tokio::time::Instant::now();

        loop {
            tokio::select! {
                _ = interval.tick() => {},
                _ = self.notify.notified() => {},
            }

            let (_, _, events) = match self.after("%", poll_revision, 500).await {
                Ok(r) => r,
                Err(e) => {
                    error!("poll error: {e}");
                    continue;
                }
            };

            if events.is_empty() {
                continue;
            }

            trace!("poll: {} events after rev {}", events.len(), poll_revision);

            let mut rev = poll_revision;
            let mut sequential = Vec::new();

            for event in &events {
                let next = rev + 1;
                if event.kv.mod_revision != next {
                    trace!(
                        "revision gap: expected {next}, got {}",
                        event.kv.mod_revision
                    );
                    if skip == next && skip_time.elapsed() > Duration::from_secs(1) {
                        // Gap persisted too long, skip it
                        error!(
                            "skipping revision gap at {next}, current event rev={}",
                            event.kv.mod_revision
                        );
                    } else if skip != next {
                        // First time seeing gap — record and retry
                        // (kine's FillRetryDuration is 0 for SQLite — no sleep)
                        skip = next;
                        skip_time = tokio::time::Instant::now();
                        self.notify.notify_waiters();
                        break;
                    } else {
                        // Second attempt — fill the gap
                        if let Err(e) = self.fill(next).await {
                            warn!("fill revision {next} failed: {e}");
                        } else {
                            trace!("filled revision gap at {next}");
                            self.notify.notify_waiters();
                        }
                        break;
                    }
                }

                rev = event.kv.mod_revision;
                // Filter out gap-fill records
                if !event.kv.key.starts_with("gap-") {
                    sequential.push(event.clone());
                }
            }

            if rev > poll_revision {
                self.current_rev
                    .compare_exchange(
                        poll_revision,
                        rev,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .ok();
                poll_revision = rev;

                if !sequential.is_empty() {
                    self.broadcaster.send(sequential).await;
                }
            }
        }
    }

    /// Background TTL expiration loop: deletes keys whose lease has expired.
    /// Tracks expiry times in memory, matching kine's approach.
    async fn ttl_loop(self: Arc<Self>) {
        use std::collections::HashMap;
        use tokio::time::Instant;

        // Wait for initial data to settle
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Map: key_name -> (mod_revision, expires_at)
        let mut expiries: HashMap<String, (i64, Instant)> = HashMap::new();

        // Seed from existing leased keys
        let rows = sqlx::query_as::<_, (String, i64, i64)>(
            "SELECT kv.name, kv.lease, kv.id
             FROM kine AS kv
             JOIN kine_current AS cur ON cur.id = kv.id
             WHERE kv.lease > 0 AND kv.deleted = 0",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let now = Instant::now();
        for (name, lease, mod_rev) in rows {
            expiries.insert(
                name,
                (mod_rev, now + Duration::from_secs(lease as u64)),
            );
        }

        // Subscribe to broadcaster to track new leased keys
        let mut broadcast_rx = self.broadcaster.subscribe().await;

        let mut check_interval = tokio::time::interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                _ = check_interval.tick() => {
                    // Check for expired keys
                    let now = Instant::now();
                    let expired: Vec<(String, i64)> = expiries
                        .iter()
                        .filter(|(_, (_, exp))| now >= *exp)
                        .map(|(k, (rev, _))| (k.clone(), *rev))
                        .collect();

                    for (key, mod_rev) in expired {
                        match self.delete(&key, mod_rev).await {
                            Ok((_, _, true)) => {
                                trace!("ttl: deleted expired key {key}");
                                expiries.remove(&key);
                            }
                            Ok((_, _, false)) => {
                                // Key was updated (different revision) — remove stale tracking
                                expiries.remove(&key);
                            }
                            Err(e) => {
                                warn!("ttl: failed to delete expired key {key}: {e}");
                            }
                        }
                    }
                }
                result = broadcast_rx.recv() => {
                    match result {
                        Some(events) => {
                            let now = Instant::now();
                            for event in &events {
                                if event.delete {
                                    expiries.remove(&event.kv.key);
                                } else if event.kv.lease > 0 {
                                    expiries.insert(
                                        event.kv.key.clone(),
                                        (event.kv.mod_revision, now + Duration::from_secs(event.kv.lease as u64)),
                                    );
                                } else {
                                    // Key updated without lease — stop tracking
                                    expiries.remove(&event.kv.key);
                                }
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    }

    /// Background compaction loop.
    async fn compact_loop(self: Arc<Self>) {
        if self.config.compact_interval.is_zero() {
            debug!("automatic compaction disabled");
            return;
        }

        // Apply jitter (5% of interval, matching kine's default)
        let jitter_range = self.config.compact_interval.as_millis() as i64 / 20;
        let jitter = if jitter_range > 0 {
            let jitter_ms = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64)
                % (jitter_range * 2)
                - jitter_range;
            Duration::from_millis(jitter_ms.unsigned_abs())
        } else {
            Duration::ZERO
        };
        let interval_with_jitter = self.config.compact_interval + jitter;

        let mut interval = tokio::time::interval(interval_with_jitter);
        interval.tick().await; // skip the first immediate tick

        loop {
            interval.tick().await;

            if let Err(e) = self.compact_once().await
                && !matches!(e, BackendError::Compacted) {
                    error!("compaction error: {e}");
                }
        }
    }

    /// Run a single compaction pass.
    async fn compact_once(&self) -> Result<()> {
        let compact_rev = self.get_compact_revision().await?;
        let current_rev = self.cached_revision().await?;

        let mut target = current_rev - self.config.compact_min_retain;
        if target <= compact_rev {
            return Err(BackendError::Compacted);
        }
        if target < 0 {
            target = 0;
        }

        // Break into batches
        let mut iter_rev = compact_rev;
        while iter_rev < target {
            iter_rev += self.config.compact_batch_size;
            if iter_rev > target {
                iter_rev = target;
            }

            let mut tx = self
                .pool
                .begin()
                .await
                .map_err(|e| BackendError::Internal(e.to_string()))?;

            // Verify compact_rev hasn't changed (another compactor may have run)
            let db_compact: (Option<i64>,) =
                sqlx::query_as("SELECT MAX(prev_revision) FROM kine WHERE name = ?")
                    .bind(COMPACT_REV_KEY)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(|e| BackendError::Internal(e.to_string()))?;
            if db_compact.0.unwrap_or(0) != compact_rev {
                return Err(BackendError::Compacted);
            }

            // Delete superseded revisions and deleted entries up to iter_rev
            let deleted = sqlx::query(
                "DELETE FROM kine WHERE id IN (
                    SELECT kp.prev_revision AS id
                    FROM kine AS kp
                    WHERE kp.name != ? AND kp.prev_revision != 0 AND kp.id <= ?
                    UNION
                    SELECT kd.id AS id
                    FROM kine AS kd
                    WHERE kd.deleted != 0 AND kd.id <= ?
                )",
            )
            .bind(COMPACT_REV_KEY)
            .bind(iter_rev)
            .bind(iter_rev)
            .execute(&mut *tx)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

            // Clean up kine_current: remove entries whose kine rows were deleted
            let cleaned = sqlx::query(
                "DELETE FROM kine_current WHERE name IN (
                    SELECT cur.name FROM kine_current AS cur
                    LEFT JOIN kine AS kv ON cur.id = kv.id
                    WHERE kv.id IS NULL
                )",
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

            if cleaned.rows_affected() > 0 {
                debug!(
                    "cleaned {} dangling kine_current entries",
                    cleaned.rows_affected()
                );
            }

            // Update compact_rev_key
            sqlx::query("UPDATE kine SET prev_revision = ? WHERE name = ?")
                .bind(iter_rev)
                .bind(COMPACT_REV_KEY)
                .execute(&mut *tx)
                .await
                .map_err(|e| BackendError::Internal(e.to_string()))?;

            tx.commit()
                .await
                .map_err(|e| BackendError::Internal(e.to_string()))?;

            debug!(
                "compacted {} rows up to revision {}/{}",
                deleted.rows_affected(),
                iter_rev,
                current_rev
            );
        }

        // Post-compact: checkpoint WAL and reclaim disk space
        sqlx::query("PRAGMA wal_checkpoint(FULL)")
            .execute(&self.pool)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        sqlx::query("PRAGMA incremental_vacuum")
            .execute(&self.pool)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        Ok(())
    }
}

#[async_trait]
impl Backend for SqliteBackend {
    async fn start(&self) -> Result<()> {
        self.setup_schema().await?;
        self.ensure_compact_rev_key().await?;

        // Create health check key (like kine does)
        match self.create("/registry/health", b"{\"health\":\"true\"}", 0).await {
            Ok(_) | Err(BackendError::KeyExists) => {}
            Err(e) => warn!("failed to create health check key: {e}"),
        }

        debug!("sqlite backend started");
        Ok(())
    }

    async fn get(
        &self,
        key: &str,
        _range_end: &str,
        _limit: i64,
        revision: i64,
        keys_only: bool,
    ) -> Result<(i64, Option<KeyValue>)> {
        let (rev, event) = self.get_internal(key, revision, false, keys_only).await?;
        Ok((rev, event.map(|e| e.kv)))
    }

    async fn create(&self, key: &str, value: &[u8], lease: i64) -> Result<i64> {
        // Check if key exists (including deleted — so we can get prev_revision)
        let (_rev, existing) = self.get_internal(key, 0, true, false).await?;

        if let Some(ref event) = existing
            && !event.delete {
                return Err(BackendError::KeyExists);
            }

        let prev_revision = existing
            .as_ref()
            .map(|e| e.kv.mod_revision)
            .unwrap_or(0);

        let old_value = existing
            .as_ref()
            .map(|e| e.kv.value.as_slice())
            .unwrap_or(b"");

        self.insert(key, true, false, 0, prev_revision, lease, value, old_value)
            .await
    }

    async fn delete(&self, key: &str, revision: i64) -> Result<(i64, Option<KeyValue>, bool)> {
        let (rev, event) = self.get_internal(key, 0, true, false).await?;

        let Some(event) = event else {
            // Key never existed
            return Ok((rev, None, true));
        };

        if event.delete {
            // Already deleted
            return Ok((rev, None, false));
        }

        if revision != 0 && event.kv.mod_revision != revision {
            // Revision mismatch
            return Ok((rev, Some(event.kv), false));
        }

        let prev_kv = event.kv.clone();
        let old_value = event.kv.value.clone();

        match self
            .insert(
                key,
                false,
                true,
                event.kv.create_revision,
                event.kv.mod_revision,
                event.kv.lease,
                &prev_kv.value,
                &old_value,
            )
            .await
        {
            Ok(new_rev) => Ok((new_rev, Some(prev_kv), true)),
            Err(_) => {
                // On insert failure (e.g. unique constraint), re-fetch and report failure
                let (rev, latest) = self.get_internal(key, 0, true, false).await?;
                Ok((rev, latest.map(|e| e.kv), false))
            }
        }
    }

    async fn list(
        &self,
        prefix: &str,
        start_key: &str,
        limit: i64,
        revision: i64,
        keys_only: bool,
    ) -> Result<(i64, Vec<KeyValue>)> {
        // Match kine's prefix/startKey handling:
        // - If prefix ends with '/' and startKey == prefix, clear startKey
        // - If prefix doesn't end with '/', clear startKey entirely
        let (like_prefix, effective_start) = if prefix.ends_with('/') {
            let sk = if start_key == prefix { "" } else { start_key };
            (format!("{}%", prefix.replace('_', "^_")), sk)
        } else {
            (prefix.replace('_', "^_"), "")
        };

        let (rev, events) = self
            .list_internal(&like_prefix, effective_start, limit, revision, false, keys_only)
            .await?;

        let kvs = events.into_iter().map(|e| e.kv).collect();
        Ok((rev, kvs))
    }

    async fn count(&self, prefix: &str, start_key: &str, revision: i64) -> Result<(i64, i64)> {
        let like_prefix = if prefix.ends_with('/') {
            format!("{}%", prefix.replace('_', "^_"))
        } else {
            prefix.replace('_', "^_")
        };

        // Match kine's prefix/startKey handling
        let effective_start = if prefix.ends_with('/') {
            if start_key == prefix { "" } else { start_key }
        } else {
            ""
        };

        if revision == 0 {
            // Use kine_current for O(keys) count
            let start_where = if effective_start.is_empty() {
                String::new()
            } else {
                "AND cur.name >= ?".to_string()
            };

            let sql = format!(
                "SELECT
                    (SELECT MAX(rkv.id) FROM kine AS rkv),
                    COUNT(*)
                FROM kine AS kv
                JOIN kine_current AS cur ON cur.id = kv.id
                WHERE cur.name LIKE ? ESCAPE '^'
                {start_where}
                AND kv.deleted = 0"
            );

            let row = if effective_start.is_empty() {
                sqlx::query_as::<_, (Option<i64>, i64)>(&sql)
                    .bind(&like_prefix)
                    .fetch_one(&self.pool)
                    .await
            } else {
                sqlx::query_as::<_, (Option<i64>, i64)>(&sql)
                    .bind(&like_prefix)
                    .bind(effective_start)
                    .fetch_one(&self.pool)
                    .await
            }
            .map_err(|e| BackendError::Internal(e.to_string()))?;

            let rev = row.0.unwrap_or(0);
            let rev = if rev == 0 { self.cached_revision().await? } else { rev };
            Ok((rev, row.1))
        } else {
            // Historical count via GROUP BY subquery
            let start_where = if effective_start.is_empty() {
                String::new()
            } else {
                "AND mkv.name >= ?".to_string()
            };

            let sql = format!(
                "SELECT
                    (SELECT MAX(rkv.id) FROM kine AS rkv),
                    COUNT(c.theid)
                FROM (
                    SELECT kv.id AS theid
                    FROM kine AS kv
                    JOIN (
                        SELECT MAX(mkv.id) AS id
                        FROM kine AS mkv
                        WHERE mkv.name LIKE ? ESCAPE '^'
                        {start_where}
                        AND mkv.id <= ?
                        GROUP BY mkv.name
                    ) AS maxkv ON maxkv.id = kv.id
                    WHERE kv.deleted = 0
                ) c"
            );

            let row = if effective_start.is_empty() {
                sqlx::query_as::<_, (Option<i64>, i64)>(&sql)
                    .bind(&like_prefix)
                    .bind(revision)
                    .fetch_one(&self.pool)
                    .await
            } else {
                sqlx::query_as::<_, (Option<i64>, i64)>(&sql)
                    .bind(&like_prefix)
                    .bind(effective_start)
                    .bind(revision)
                    .fetch_one(&self.pool)
                    .await
            }
            .map_err(|e| BackendError::Internal(e.to_string()))?;

            let rev = row.0.unwrap_or(0);
            let rev = if rev == 0 { self.cached_revision().await? } else { rev };
            Ok((rev, row.1))
        }
    }

    async fn update(
        &self,
        key: &str,
        value: &[u8],
        revision: i64,
        lease: i64,
    ) -> Result<(i64, Option<KeyValue>, bool)> {
        let (rev, event) = self.get_internal(key, 0, false, false).await?;

        let Some(event) = event else {
            return Ok((rev, None, false));
        };

        if event.kv.mod_revision != revision {
            return Ok((rev, Some(event.kv), false));
        }

        let old_value = event.kv.value.clone();

        match self
            .insert(
                key,
                false,
                false,
                event.kv.create_revision,
                event.kv.mod_revision,
                lease,
                value,
                &old_value,
            )
            .await
        {
            Ok(new_rev) => {
                let updated_kv = KeyValue {
                    key: key.to_string(),
                    value: value.to_vec(),
                    version: 0,
                    create_revision: event.kv.create_revision,
                    mod_revision: new_rev,
                    lease,
                };
                Ok((new_rev, Some(updated_kv), true))
            }
            Err(_) => {
                let (rev, latest) = self.get_internal(key, 0, false, false).await?;
                Ok((rev, latest.map(|e| e.kv), false))
            }
        }
    }

    async fn watch(&self, key: &str, revision: i64) -> Result<WatchResult> {
        // Start background tasks lazily on first watch (matching kine's startWatch)
        self.ensure_background_tasks();

        let (tx, rx) = mpsc::channel(100);

        // Get current revision for the result
        let current_rev = self.cached_revision().await?;
        let compact_rev = self.get_compact_revision().await?;

        // Check if requested revision is compacted
        if revision > 0 && revision < compact_rev {
            return Err(BackendError::Compacted);
        }

        let mut broadcast_rx = self.broadcaster.subscribe().await;
        let prefix = key.to_string();

        // First, send any historical events since the requested revision
        let start_rev = if revision > 0 { revision - 1 } else { 0 };
        let pool = self.pool.clone();

        tokio::spawn(async move {
            // Fetch historical events
            if start_rev > 0 {
                let after_prefix = if prefix.ends_with('/') {
                    format!("{prefix}%")
                } else {
                    prefix.clone()
                };

                let sql = format!(
                    "SELECT
                        (SELECT MAX(rkv.id) FROM kine AS rkv),
                        (SELECT MAX(crkv.prev_revision) FROM kine AS crkv WHERE crkv.name = '{COMPACT_REV_KEY}'),
                        kv.id AS theid,
                        kv.name AS thename,
                        kv.created,
                        kv.deleted,
                        kv.create_revision,
                        kv.prev_revision,
                        kv.lease,
                        kv.value,
                        kv.old_value
                    FROM kine AS kv
                    WHERE kv.name LIKE ? ESCAPE '^' AND kv.id > ?
                    ORDER BY kv.id ASC"
                );

                if let Ok(rows) = sqlx::query(&sql)
                    .bind(&after_prefix)
                    .bind(start_rev)
                    .fetch_all(&pool)
                    .await
                {
                    let mut events = Vec::new();
                    for row in &rows {
                        let mod_revision: i64 = row.get(2);
                        let name: String = row.get(3);
                        let created: i32 = row.get(4);
                        let deleted: i32 = row.get(5);
                        let create_revision: i64 = row.get(6);
                        let prev_revision: i64 = row.get(7);
                        let lease: i64 = row.get(8);
                        let value: Vec<u8> = row.try_get(9).unwrap_or_default();
                        let old_value: Vec<u8> = row.try_get(10).unwrap_or_default();

                        if name.starts_with("gap-") {
                            continue;
                        }

                        let is_create = created != 0;
                        let is_delete = deleted != 0;
                        let actual_create_rev = if is_create { mod_revision } else { create_revision };

                        events.push(Event {
                            create: is_create,
                            delete: is_delete,
                            kv: KeyValue {
                                key: name.clone(),
                                value,
                                version: 0,
                                create_revision: actual_create_rev,
                                mod_revision,
                                lease,
                            },
                            prev_kv: if is_create {
                                None
                            } else {
                                Some(KeyValue {
                                    key: name,
                                    value: old_value,
                                    version: 0,
                                    create_revision: actual_create_rev,
                                    mod_revision: prev_revision,
                                    lease,
                                })
                            },
                        });
                    }

                    if !events.is_empty()
                        && tx.send(events).await.is_err() {
                            return;
                        }
                }
            }

            // Stream live events, filtering by prefix
            let check_prefix = prefix.ends_with('/');
            loop {
                match broadcast_rx.recv().await {
                    Some(events) => {
                        let filtered: Vec<Event> = events
                            .into_iter()
                            .filter(|e| {
                                if check_prefix {
                                    e.kv.key.starts_with(&prefix)
                                } else {
                                    e.kv.key == prefix
                                }
                            })
                            .collect();
                        if !filtered.is_empty()
                            && tx.send(filtered).await.is_err() {
                                return;
                            }
                    }
                    None => {
                        return;
                    }
                }
            }
        });

        Ok(WatchResult {
            current_revision: current_rev,
            compact_revision: compact_rev,
            events: rx,
        })
    }

    async fn db_size(&self) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT (page_count - freelist_count) * page_size FROM pragma_page_count(), pragma_freelist_count(), pragma_page_size()",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| BackendError::Internal(e.to_string()))?;
        Ok(row.0)
    }

    async fn current_revision(&self) -> Result<i64> {
        self.cached_revision().await
    }

    async fn compact(&self, revision: i64) -> Result<i64> {
        // Manual compact: run compaction up to the given revision
        let compact_rev = self.get_compact_revision().await?;
        let current_rev = self.cached_revision().await?;

        let mut target = revision;
        // Safety: never compact the most recent N revisions
        let safe_rev = current_rev - self.config.compact_min_retain;
        if target > safe_rev {
            target = safe_rev;
        }
        if target <= compact_rev || target < 0 {
            return Ok(current_rev);
        }

        let deleted = sqlx::query(
            "DELETE FROM kine WHERE id IN (
                SELECT kp.prev_revision AS id
                FROM kine AS kp
                WHERE kp.name != ? AND kp.prev_revision != 0 AND kp.id <= ?
                UNION
                SELECT kd.id AS id
                FROM kine AS kd
                WHERE kd.deleted != 0 AND kd.id <= ?
            )",
        )
        .bind(COMPACT_REV_KEY)
        .bind(target)
        .bind(target)
        .execute(&self.pool)
        .await
        .map_err(|e| BackendError::Internal(e.to_string()))?;

        // Clean up kine_current: remove entries whose kine rows were deleted
        sqlx::query(
            "DELETE FROM kine_current WHERE name IN (
                SELECT cur.name FROM kine_current AS cur
                LEFT JOIN kine AS kv ON cur.id = kv.id
                WHERE kv.id IS NULL
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| BackendError::Internal(e.to_string()))?;

        sqlx::query("UPDATE kine SET prev_revision = ? WHERE name = ?")
            .bind(target)
            .bind(COMPACT_REV_KEY)
            .execute(&self.pool)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        debug!(
            "manual compact: deleted {} rows up to revision {}",
            deleted.rows_affected(),
            target
        );

        self.cached_revision().await
    }
}
