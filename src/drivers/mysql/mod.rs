//! MySQL/MariaDB backend driver for rhino.
//!
//! Implements the [`Backend`] trait using MySQL with the same schema and
//! log-structured approach as kine. Uses `BIGINT UNSIGNED AUTO_INCREMENT`
//! for revision IDs, `LAST_INSERT_ID()` for inserts, and `INNER JOIN`
//! for compaction deletes.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions};
use sqlx::Row;
use tokio::sync::{broadcast, mpsc, Notify};
use tracing::{debug, error, trace, warn};

use crate::backend::{Backend, BackendError, Event, KeyValue, Result, WatchResult};

const COMPACT_REV_KEY: &str = "compact_rev_key";
const COMPACT_MIN_RETAIN: i64 = 1000;
const COMPACT_BATCH_SIZE: i64 = 1000;
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const COMPACT_INTERVAL: Duration = Duration::from_secs(300);

/// Configuration for the MySQL backend.
#[derive(Debug, Clone)]
pub struct MysqlConfig {
    /// MySQL connection string. Defaults to `"mysql://root@localhost/kubernetes"`.
    pub dsn: String,
    /// Interval between compaction runs. Set to `Duration::ZERO` to disable.
    pub compact_interval: Duration,
    /// Minimum number of recent revisions to retain during compaction.
    pub compact_min_retain: i64,
    /// Max revisions to compact per transaction batch.
    pub compact_batch_size: i64,
    /// Maximum number of connections in the pool.
    pub max_connections: u32,
}

impl Default for MysqlConfig {
    fn default() -> Self {
        Self {
            dsn: "mysql://root@localhost/kubernetes".to_string(),
            compact_interval: COMPACT_INTERVAL,
            compact_min_retain: COMPACT_MIN_RETAIN,
            compact_batch_size: COMPACT_BATCH_SIZE,
            max_connections: 5,
        }
    }
}

/// MySQL-backed implementation of the rhino [`Backend`] trait.
pub struct MysqlBackend {
    pool: MySqlPool,
    current_rev: AtomicI64,
    notify: Notify,
    broadcast_tx: broadcast::Sender<Vec<Event>>,
    /// Broadcasts the revision that the poll loop has processed up to.
    polled_rev: Arc<tokio::sync::watch::Sender<i64>>,
    config: MysqlConfig,
}

impl MysqlBackend {
    /// Create a new MySQL backend with the given configuration.
    pub async fn new(config: MysqlConfig) -> std::result::Result<Self, BackendError> {
        let opts: MySqlConnectOptions = config
            .dsn
            .parse()
            .map_err(|e| BackendError::Internal(format!("invalid connection string: {e}")))?;

        let pool = MySqlPoolOptions::new()
            .max_connections(config.max_connections)
            .connect_with(opts)
            .await
            .map_err(|e| BackendError::Internal(format!("failed to connect to database: {e}")))?;

        let (broadcast_tx, _) = broadcast::channel(1024);
        let (polled_rev_tx, _) = tokio::sync::watch::channel(0i64);

        Ok(Self {
            pool,
            current_rev: AtomicI64::new(0),
            notify: Notify::new(),
            broadcast_tx,
            polled_rev: Arc::new(polled_rev_tx),
            config,
        })
    }

    async fn setup_schema(&self) -> Result<()> {
        // Create table
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS kine (
                id BIGINT UNSIGNED AUTO_INCREMENT,
                name VARCHAR(630) CHARACTER SET ascii,
                created INTEGER,
                deleted INTEGER,
                create_revision BIGINT UNSIGNED,
                prev_revision BIGINT UNSIGNED,
                lease INTEGER,
                value MEDIUMBLOB,
                old_value MEDIUMBLOB,
                PRIMARY KEY (id)
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| BackendError::Internal(format!("schema setup failed: {e}")))?;

        // Create indexes — ignore error 1061 (duplicate key name) for idempotency
        let indexes = [
            "CREATE INDEX kine_name_index ON kine (name)",
            "CREATE INDEX kine_name_id_index ON kine (name, id)",
            "CREATE INDEX kine_id_deleted_index ON kine (id, deleted)",
            "CREATE INDEX kine_prev_revision_index ON kine (prev_revision)",
            "CREATE UNIQUE INDEX kine_name_prev_revision_uindex ON kine (name, prev_revision)",
        ];

        for stmt in &indexes {
            match sqlx::query(stmt).execute(&self.pool).await {
                Ok(_) => {}
                Err(e) => {
                    // MySQL error 1061 = duplicate key name (index already exists)
                    if !is_duplicate_key_name(&e) {
                        return Err(BackendError::Internal(format!(
                            "index creation failed: {e}"
                        )));
                    }
                }
            }
        }

        debug!("database schema and indexes are up to date");
        Ok(())
    }

    async fn ensure_compact_rev_key(&self) -> Result<()> {
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM kine WHERE name = ?")
            .bind(COMPACT_REV_KEY)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        if count.0 == 0 {
            self.insert(COMPACT_REV_KEY, true, false, 0, 0, 0, b"", b"")
                .await?;
        }
        Ok(())
    }

    /// Insert a row, returning the new revision via LAST_INSERT_ID().
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
            if is_unique_violation(&e) {
                BackendError::KeyExists
            } else {
                BackendError::Internal(e.to_string())
            }
        })?;

        let id = result.last_insert_id() as i64;
        self.current_rev.store(id, Ordering::Release);
        self.notify.notify_waiters();
        Ok(id)
    }

    async fn fill(&self, revision: i64) -> Result<()> {
        let name = format!("gap-{revision}");
        // MySQL: INSERT IGNORE for conflict-safe gap filling
        sqlx::query(
            "INSERT IGNORE INTO kine(id, name, created, deleted, create_revision, prev_revision, lease, value, old_value)
             VALUES(?, ?, 0, 1, 0, 0, 0, NULL, NULL)",
        )
        .bind(revision)
        .bind(&name)
        .execute(&self.pool)
        .await
        .map_err(|e| BackendError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn db_current_revision(&self) -> Result<i64> {
        let row: (Option<i64>,) = sqlx::query_as("SELECT MAX(id) FROM kine")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;
        Ok(row.0.unwrap_or(0))
    }

    async fn get_compact_revision(&self) -> Result<i64> {
        let row: (Option<i64>,) =
            sqlx::query_as("SELECT MAX(prev_revision) FROM kine WHERE name = ?")
                .bind(COMPACT_REV_KEY)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| BackendError::Internal(e.to_string()))?;
        Ok(row.0.unwrap_or(0))
    }

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

    /// Core list query using JOIN (SELECT MAX(id) ... GROUP BY name) — same pattern as SQLite/kine generic.
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

        let (extra_where, query) = if revision == 0 {
            if start_key.is_empty() {
                (String::new(), None)
            } else {
                ("AND mkv.name >= ?".to_string(), Some(start_key.to_string()))
            }
        } else if start_key.is_empty() {
            (format!("AND mkv.id <= {revision}"), None)
        } else {
            (
                format!("AND mkv.name >= ? AND mkv.id <= {revision}"),
                Some(start_key.to_string()),
            )
        };

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
                kv.lease
                {value_cols}
            FROM kine AS kv
            JOIN (
                SELECT MAX(mkv.id) AS id
                FROM kine AS mkv
                WHERE mkv.name LIKE ? ESCAPE '^'
                {extra_where}
                GROUP BY mkv.name
            ) AS maxkv ON maxkv.id = kv.id
            WHERE (kv.deleted = 0 OR ?)
            ORDER BY kv.name ASC
            {limit_clause}"
        );

        let include_deleted_val: i32 = if include_deleted { 1 } else { 0 };

        let rows = if let Some(ref sk) = query {
            sqlx::query(&sql)
                .bind(prefix)
                .bind(sk)
                .bind(include_deleted_val)
                .fetch_all(&self.pool)
                .await
        } else {
            sqlx::query(&sql)
                .bind(prefix)
                .bind(include_deleted_val)
                .fetch_all(&self.pool)
                .await
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

    fn row_to_event(
        &self,
        row: &sqlx::mysql::MySqlRow,
        keys_only: bool,
        with_old_value: bool,
    ) -> Result<Event> {
        let mod_revision: i64 = row.get(2);
        let name: String = row.get(3);
        let created: i32 = row.get(4);
        let deleted: i32 = row.get(5);
        let create_revision: i64 = row.get(6);
        let prev_revision: i64 = row.get(7);
        let lease: i32 = row.get(8);

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
            version: 0,
            create_revision: actual_create_rev,
            mod_revision,
            lease: lease as i64,
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
                lease: lease as i64,
            })
        };

        Ok(Event {
            create: is_create,
            delete: is_delete,
            kv,
            prev_kv,
        })
    }

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

            // Update polled revision before querying (matching kine sql.go:504-508).
            let _ = self.polled_rev.send(poll_revision);

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
                        error!(
                            "skipping revision gap at {next}, current event rev={}",
                            event.kv.mod_revision
                        );
                    } else if skip != next {
                        skip = next;
                        skip_time = tokio::time::Instant::now();
                        self.notify.notify_waiters();
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        break;
                    } else {
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
                if !event.kv.key.starts_with("gap-") {
                    sequential.push(event.clone());
                }
            }

            if rev > poll_revision {
                self.current_rev
                    .compare_exchange(poll_revision, rev, Ordering::AcqRel, Ordering::Acquire)
                    .ok();
                poll_revision = rev;

                if !sequential.is_empty() {
                    let _ = self.broadcast_tx.send(sequential);
                }
            }
        }
    }

    async fn compact_loop(self: Arc<Self>) {
        if self.config.compact_interval.is_zero() {
            debug!("automatic compaction disabled");
            return;
        }

        let mut interval = tokio::time::interval(self.config.compact_interval);
        interval.tick().await;

        loop {
            interval.tick().await;

            if let Err(e) = self.compact_once().await
                && !matches!(e, BackendError::Compacted)
            {
                error!("compaction error: {e}");
            }
        }
    }

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

            // MySQL compact uses INNER JOIN (not USING or IN subquery)
            let deleted = sqlx::query(
                "DELETE kv FROM kine AS kv
                 INNER JOIN (
                    SELECT kp.prev_revision AS id
                    FROM kine AS kp
                    WHERE kp.name != ? AND kp.prev_revision != 0 AND kp.id <= ?
                    UNION
                    SELECT kd.id AS id
                    FROM kine AS kd
                    WHERE kd.deleted != 0 AND kd.id <= ?
                 ) AS ks
                 ON kv.id = ks.id",
            )
            .bind(COMPACT_REV_KEY)
            .bind(iter_rev)
            .bind(iter_rev)
            .execute(&mut *tx)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

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

        Ok(())
    }
}

/// Check if a sqlx error is a MySQL duplicate entry error (error 1062).
fn is_unique_violation(err: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db_err) = err {
        db_err.code().is_some_and(|code| code == "1062")
    } else {
        false
    }
}

/// Check if a sqlx error is a MySQL duplicate key name error (error 1061).
fn is_duplicate_key_name(err: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db_err) = err {
        db_err.code().is_some_and(|code| code == "1061")
    } else {
        false
    }
}

/// Translate start key: replace trailing null byte with # as MySQL
/// latin1 collation does not handle nonprinting characters correctly.
fn translate_start_key(start_key: &str) -> String {
    if let Some(s) = start_key.strip_suffix('\x00') {
        format!("{s}#")
    } else {
        start_key.to_string()
    }
}

#[async_trait]
impl Backend for MysqlBackend {
    async fn start(&self) -> Result<()> {
        self.setup_schema().await?;
        self.ensure_compact_rev_key().await?;

        match self
            .create("/registry/health", b"{\"health\":\"true\"}", 0)
            .await
        {
            Ok(_) | Err(BackendError::KeyExists) => {}
            Err(e) => warn!("failed to create health check key: {e}"),
        }

        let poll_self = Arc::new(MysqlBackend {
            pool: self.pool.clone(),
            current_rev: AtomicI64::new(self.current_rev.load(Ordering::Acquire)),
            notify: Notify::new(),
            broadcast_tx: self.broadcast_tx.clone(),
            polled_rev: self.polled_rev.clone(),
            config: self.config.clone(),
        });

        let compact_self = Arc::new(MysqlBackend {
            pool: self.pool.clone(),
            current_rev: AtomicI64::new(0),
            notify: Notify::new(),
            broadcast_tx: self.broadcast_tx.clone(),
            polled_rev: self.polled_rev.clone(),
            config: self.config.clone(),
        });

        tokio::spawn(async move { poll_self.poll_loop().await });
        tokio::spawn(async move { compact_self.compact_loop().await });

        debug!("mysql backend started");
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
        let (_rev, existing) = self.get_internal(key, 0, true, false).await?;

        if let Some(ref event) = existing
            && !event.delete
        {
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
            return Ok((rev, None, true));
        };

        if event.delete {
            return Ok((rev, None, false));
        }

        if revision != 0 && event.kv.mod_revision != revision {
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
                let (rev, latest) = self.get_internal(key, 0, true, false).await?;
                Ok((rev, latest.map(|e| e.kv), false))
            }
        }
    }

    async fn delete_prefix(&self, prefix: &str) -> Result<(i64, i64, Vec<KeyValue>)> {
        let like_prefix = format!("{}%", prefix.replace('_', "^_"));
        let (_, kvs) = self.list(&like_prefix, "", 0, 0, false).await?;

        let mut prev_kvs = Vec::new();
        let mut last_rev = 0i64;

        for kv in &kvs {
            match self.delete(&kv.key, 0).await {
                Ok((rev, prev, true)) => {
                    last_rev = rev;
                    if let Some(p) = prev {
                        prev_kvs.push(p);
                    }
                }
                Ok((rev, _, false)) => {
                    last_rev = rev;
                }
                Err(e) => return Err(e),
            }
        }

        if last_rev == 0 {
            last_rev = self.cached_revision().await?;
        }

        let deleted = prev_kvs.len() as i64;
        Ok((last_rev, deleted, prev_kvs))
    }

    async fn list(
        &self,
        prefix: &str,
        start_key: &str,
        limit: i64,
        revision: i64,
        keys_only: bool,
    ) -> Result<(i64, Vec<KeyValue>)> {
        let like_prefix = if prefix.ends_with('/') {
            format!("{}%", prefix.replace('_', "^_"))
        } else {
            prefix.replace('_', "^_")
        };

        let translated_start = translate_start_key(start_key);

        let (rev, events) = self
            .list_internal(
                &like_prefix,
                &translated_start,
                limit,
                revision,
                false,
                keys_only,
            )
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

        let translated_start = translate_start_key(start_key);

        let (rev, events) = self
            .list_internal(&like_prefix, &translated_start, 0, revision, false, true)
            .await?;

        Ok((rev, events.len() as i64))
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
        let (tx, rx) = mpsc::channel(100);

        let current_rev = self.cached_revision().await?;
        let compact_rev = self.get_compact_revision().await?;

        if revision > 0 && revision < compact_rev {
            return Err(BackendError::Compacted);
        }

        let mut broadcast_rx = self.broadcast_tx.subscribe();
        let prefix = key.to_string();
        let start_rev = if revision > 0 { revision - 1 } else { 0 };
        let pool = self.pool.clone();

        tokio::spawn(async move {
            if start_rev > 0 {
                let after_prefix = if prefix.ends_with('/') {
                    format!("{prefix}%")
                } else {
                    prefix.clone()
                };

                let sql =
                    "SELECT
                        (SELECT MAX(rkv.id) FROM kine AS rkv),
                        (SELECT MAX(crkv.prev_revision) FROM kine AS crkv WHERE crkv.name = 'compact_rev_key'),
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
                    ORDER BY kv.id ASC";

                if let Ok(rows) = sqlx::query(sql)
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
                        let lease: i32 = row.get(8);
                        let value: Vec<u8> = row.try_get(9).unwrap_or_default();
                        let old_value: Vec<u8> = row.try_get(10).unwrap_or_default();

                        if name.starts_with("gap-") {
                            continue;
                        }

                        let is_create = created != 0;
                        let is_delete = deleted != 0;
                        let actual_create_rev =
                            if is_create { mod_revision } else { create_revision };

                        events.push(Event {
                            create: is_create,
                            delete: is_delete,
                            kv: KeyValue {
                                key: name.clone(),
                                value,
                                version: 0,
                                create_revision: actual_create_rev,
                                mod_revision,
                                lease: lease as i64,
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
                                    lease: lease as i64,
                                })
                            },
                        });
                    }

                    if !events.is_empty() && tx.send(events).await.is_err() {
                        return;
                    }
                }
            }

            let check_prefix = prefix.ends_with('/');
            loop {
                match broadcast_rx.recv().await {
                    Ok(events) => {
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
                        if !filtered.is_empty() && tx.send(filtered).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("watch subscriber lagged by {n} events");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
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
        let row: (Option<i64>,) = sqlx::query_as(
            "SELECT SUM(data_length + index_length)
             FROM information_schema.TABLES
             WHERE table_schema = DATABASE() AND table_name = 'kine'",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| BackendError::Internal(e.to_string()))?;
        Ok(row.0.unwrap_or(0))
    }

    async fn current_revision(&self) -> Result<i64> {
        self.cached_revision().await
    }

    async fn wait_for_sync_to(&self, revision: i64) {
        let mut rx = self.polled_rev.subscribe();
        while *rx.borrow() < revision {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }

    async fn compact(&self, revision: i64) -> Result<i64> {
        let compact_rev = self.get_compact_revision().await?;
        let current_rev = self.cached_revision().await?;

        let mut target = revision;
        let safe_rev = current_rev - self.config.compact_min_retain;
        if target > safe_rev {
            target = safe_rev;
        }
        if target <= compact_rev || target < 0 {
            return Ok(current_rev);
        }

        // MySQL compact uses INNER JOIN
        let deleted = sqlx::query(
            "DELETE kv FROM kine AS kv
             INNER JOIN (
                SELECT kp.prev_revision AS id
                FROM kine AS kp
                WHERE kp.name != ? AND kp.prev_revision != 0 AND kp.id <= ?
                UNION
                SELECT kd.id AS id
                FROM kine AS kd
                WHERE kd.deleted != 0 AND kd.id <= ?
             ) AS ks
             ON kv.id = ks.id",
        )
        .bind(COMPACT_REV_KEY)
        .bind(target)
        .bind(target)
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
