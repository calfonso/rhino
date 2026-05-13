//! Redis backend driver for rhino.
//!
//! Implements the [`Backend`] trait using Redis. The kine log-structured model
//! is mapped to Redis data structures:
//!
//! - **Revision counter**: `{rhino}:rev` — atomic `INCR` for monotonic revisions.
//! - **Row data**: `{rhino}:row:{id}` — hash per revision with all kine columns.
//! - **Log index**: `{rhino}:log` — sorted set scored by revision for poll loop scanning.
//! - **Current index**: `{rhino}:current` — hash mapping key name → latest revision.
//! - **Name-to-revisions**: `{rhino}:key:{name}` — sorted set of all revisions for a key
//!   (enables historical queries).
//! - **Name index**: `{rhino}:names` — sorted set with lexicographic ordering for prefix scans.
//! - **Compact revision**: `{rhino}:compact_rev` — single key storing the compaction watermark.
//!
//! All mutating operations (create, update, delete, delete_prefix) are implemented
//! as atomic Lua scripts to prevent race conditions between read and write phases.
//!
//! **Cluster compatibility**: All Redis keys use the `{rhino}` hash tag (e.g.,
//! `{rhino}:row:1`, `{rhino}:key:/test/a`) to ensure they colocate on a single
//! hash slot. This allows the Lua scripts to access dynamically constructed keys
//! in Redis Cluster. The trade-off is that all data lands on one shard — this
//! backend does not distribute load across cluster nodes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::{AsyncCommands, Client, Script};
use tokio::sync::{Mutex, Notify, mpsc};
use tracing::{debug, error, trace, warn};

use crate::backend::{Backend, BackendError, Event, KeyValue, Result, WatchResult};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const COMPACT_INTERVAL: Duration = Duration::from_secs(300);
const COMPACT_MIN_RETAIN: i64 = 1000;

// Redis key names. The `{rhino}` hash tag ensures all keys land on the same
// hash slot, which is required for Lua scripts to access them in Redis Cluster.
const REV_KEY: &str = "{rhino}:rev";
const ROW_PREFIX: &str = "{rhino}:row:";
const LOG_KEY: &str = "{rhino}:log";
const CURRENT_KEY: &str = "{rhino}:current";
const KEY_REVS_PREFIX: &str = "{rhino}:key:";
const NAMES_KEY: &str = "{rhino}:names";
const COMPACT_REV_STORE: &str = "{rhino}:compact_rev";
const UNIQ_PREFIX: &str = "{rhino}:uniq:";

/// Kine-style broadcaster: fans out events to per-subscriber channels.
/// Slow subscribers are disconnected (channel dropped) rather than lagging.
struct Broadcaster {
    subscribers: Mutex<Vec<mpsc::Sender<Arc<Vec<Event>>>>>,
}

impl Broadcaster {
    fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// Subscribe to events. Returns a receiver with capacity 1000.
    /// Large buffer prevents subscriber drops during transient query latency spikes.
    async fn subscribe(&self) -> mpsc::Receiver<Arc<Vec<Event>>> {
        let (tx, rx) = mpsc::channel(1000);
        self.subscribers.lock().await.push(tx);
        rx
    }

    /// Broadcast events to all subscribers. Slow subscribers (full channel) are dropped.
    /// Events are wrapped in Arc to avoid cloning per subscriber.
    async fn send(&self, events: Vec<Event>) {
        let events = Arc::new(events);
        let mut subs = self.subscribers.lock().await;
        subs.retain(|tx| match tx.try_send(Arc::clone(&events)) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!("dropping slow watch subscriber");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
    }
}

/// Configuration for the Redis backend.
#[derive(Debug, Clone)]
pub struct RedisConfig {
    /// Redis connection URL. Defaults to `"redis://127.0.0.1:6379"`.
    pub dsn: String,
    /// Interval between compaction runs. Set to `Duration::ZERO` to disable.
    pub compact_interval: Duration,
    /// Minimum number of recent revisions to retain during compaction.
    pub compact_min_retain: i64,
    /// Max revisions to compact per batch.
    pub compact_batch_size: i64,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            dsn: "redis://127.0.0.1:6379".to_string(),
            compact_interval: COMPACT_INTERVAL,
            compact_min_retain: COMPACT_MIN_RETAIN,
            compact_batch_size: 1000,
        }
    }
}

/// Pre-compiled Lua scripts for atomic operations.
struct Scripts {
    create: Script,
    update: Script,
    delete: Script,
    delete_prefix: Script,
    fill: Script,
    historical_revs: Script,
}

impl Scripts {
    fn new() -> Self {
        Self {
            create: Script::new(Self::CREATE_LUA),
            update: Script::new(Self::UPDATE_LUA),
            delete: Script::new(Self::DELETE_LUA),
            delete_prefix: Script::new(Self::DELETE_PREFIX_LUA),
            fill: Script::new(Self::FILL_LUA),
            historical_revs: Script::new(Self::HISTORICAL_REVS_LUA),
        }
    }

    /// Atomic create: check if key exists, reject if alive, insert with correct prev_revision.
    /// KEYS: [rev_key, log_key, current_key, names_key]
    /// ARGV: [name, lease, value]
    /// Returns: new revision id, or error
    const CREATE_LUA: &'static str = r#"
        local rev_key = KEYS[1]
        local log_key = KEYS[2]
        local current_key = KEYS[3]
        local names_key = KEYS[4]

        local name = ARGV[1]
        local lease = ARGV[2]
        local value = ARGV[3]

        -- Check if key currently exists
        local current_rev_str = redis.call("HGET", current_key, name)
        local prev_revision = "0"
        local old_value = ""

        if current_rev_str then
            local current_rev = tonumber(current_rev_str)
            local row_key = "{rhino}:row:" .. current_rev
            local row_deleted = redis.call("HGET", row_key, "deleted")

            if row_deleted == "0" or row_deleted == false then
                -- Key exists and is alive — cannot create
                return redis.error_reply("KEY_EXISTS")
            end

            -- Key was deleted — re-create is allowed
            prev_revision = tostring(current_rev)
            old_value = redis.call("HGET", row_key, "value") or ""
        end

        -- Enforce unique constraint on (name, prev_revision)
        local constraint_key = "{rhino}:uniq:" .. name .. ":" .. prev_revision
        if redis.call("EXISTS", constraint_key) == 1 then
            return redis.error_reply("KEY_EXISTS")
        end
        redis.call("SET", constraint_key, "1")

        -- Allocate revision
        local id = redis.call("INCR", rev_key)

        -- Store row
        local row_key = "{rhino}:row:" .. id
        redis.call("HSET", row_key,
            "name", name,
            "created", "1",
            "deleted", "0",
            "create_revision", "0",
            "prev_revision", prev_revision,
            "lease", lease,
            "value", value,
            "old_value", old_value)

        -- Update indexes
        redis.call("ZADD", log_key, id, tostring(id))
        redis.call("HSET", current_key, name, id)
        redis.call("ZADD", "{rhino}:key:" .. name, id, tostring(id))
        redis.call("ZADD", names_key, 0, name)

        return id
    "#;

    /// Atomic update: verify mod_revision matches, then insert new version.
    /// KEYS: [rev_key, log_key, current_key, names_key]
    /// ARGV: [name, expected_revision, lease, value]
    /// Returns: {new_rev, prev_mod_rev, prev_create_rev, prev_value, prev_lease} or
    ///          {0, current_mod_rev, current_create_rev, current_value, current_lease} on mismatch
    ///          {-1} if key doesn't exist
    const UPDATE_LUA: &'static str = r#"
        local rev_key = KEYS[1]
        local log_key = KEYS[2]
        local current_key = KEYS[3]
        local names_key = KEYS[4]

        local name = ARGV[1]
        local expected_rev = tonumber(ARGV[2])
        local lease = ARGV[3]
        local value = ARGV[4]

        -- Look up current state
        local current_rev_str = redis.call("HGET", current_key, name)
        if not current_rev_str then
            return {-1}
        end

        local current_rev = tonumber(current_rev_str)
        local row_key = "{rhino}:row:" .. current_rev
        local row = redis.call("HMGET", row_key, "deleted", "create_revision", "value", "lease", "created")
        local row_deleted = row[1]
        local row_create_rev = row[2]
        local row_value = row[3]
        local row_lease = row[4]
        local row_created = row[5]

        -- If key is deleted, treat as non-existent
        if row_deleted == "1" then
            return {-1}
        end

        -- Compute actual create_revision (if this was a create, create_rev = mod_rev)
        local actual_create_rev = row_create_rev
        if row_created == "1" then
            actual_create_rev = tostring(current_rev)
        end

        -- Check revision match
        if current_rev ~= expected_rev then
            return {0, tostring(current_rev), actual_create_rev, row_value or "", row_lease or "0"}
        end

        -- Enforce unique constraint
        local constraint_key = "{rhino}:uniq:" .. name .. ":" .. tostring(current_rev)
        if redis.call("EXISTS", constraint_key) == 1 then
            return {0, tostring(current_rev), actual_create_rev, row_value or "", row_lease or "0"}
        end
        redis.call("SET", constraint_key, "1")

        local old_value = row_value or ""

        -- Allocate revision
        local id = redis.call("INCR", rev_key)

        -- Store row
        local new_row_key = "{rhino}:row:" .. id
        redis.call("HSET", new_row_key,
            "name", name,
            "created", "0",
            "deleted", "0",
            "create_revision", actual_create_rev,
            "prev_revision", tostring(current_rev),
            "lease", lease,
            "value", value,
            "old_value", old_value)

        -- Update indexes
        redis.call("ZADD", log_key, id, tostring(id))
        redis.call("HSET", current_key, name, id)
        redis.call("ZADD", "{rhino}:key:" .. name, id, tostring(id))

        return {id, tostring(current_rev), actual_create_rev, value, lease}
    "#;

    /// Atomic delete: verify key exists/alive, optionally check revision, insert delete marker.
    /// KEYS: [rev_key, log_key, current_key, names_key]
    /// ARGV: [name, expected_revision ("0" for unconditional)]
    /// Returns: {status, new_rev_or_current_rev, prev_key, prev_value, prev_lease,
    ///           prev_create_rev, prev_mod_rev}
    ///   status: 1 = deleted, 0 = failed, 2 = never existed
    const DELETE_LUA: &'static str = r#"
        local rev_key = KEYS[1]
        local log_key = KEYS[2]
        local current_key = KEYS[3]
        local names_key = KEYS[4]

        local name = ARGV[1]
        local expected_rev = tonumber(ARGV[2])

        -- Get current revision counter for return value
        local global_rev = tonumber(redis.call("GET", rev_key) or "0")

        -- Look up current state
        local current_rev_str = redis.call("HGET", current_key, name)
        if not current_rev_str then
            return {2, global_rev, "", "", "0", "0", "0"}
        end

        local current_rev = tonumber(current_rev_str)
        local row_key = "{rhino}:row:" .. current_rev
        local row = redis.call("HMGET", row_key, "deleted", "create_revision", "value", "lease", "created")
        local row_deleted = row[1]
        local row_create_rev = row[2]
        local row_value = row[3] or ""
        local row_lease = row[4] or "0"
        local row_created = row[5]

        local actual_create_rev = row_create_rev
        if row_created == "1" then
            actual_create_rev = tostring(current_rev)
        end

        if row_deleted == "1" then
            return {0, global_rev, "", "", "0", "0", "0"}
        end

        if expected_rev ~= 0 and current_rev ~= expected_rev then
            return {0, global_rev, name, row_value, row_lease, actual_create_rev, tostring(current_rev)}
        end

        -- Enforce unique constraint
        local constraint_key = "{rhino}:uniq:" .. name .. ":" .. tostring(current_rev)
        if redis.call("EXISTS", constraint_key) == 1 then
            return {0, global_rev, name, row_value, row_lease, actual_create_rev, tostring(current_rev)}
        end
        redis.call("SET", constraint_key, "1")

        -- Allocate revision
        local id = redis.call("INCR", rev_key)

        -- Store delete marker
        local new_row_key = "{rhino}:row:" .. id
        redis.call("HSET", new_row_key,
            "name", name,
            "created", "0",
            "deleted", "1",
            "create_revision", actual_create_rev,
            "prev_revision", tostring(current_rev),
            "lease", row_lease,
            "value", row_value,
            "old_value", row_value)

        -- Update indexes
        redis.call("ZADD", log_key, id, tostring(id))
        redis.call("HSET", current_key, name, id)
        redis.call("ZADD", "{rhino}:key:" .. name, id, tostring(id))

        return {1, id, name, row_value, row_lease, actual_create_rev, tostring(current_rev)}
    "#;

    /// Atomic prefix delete: scan names by lex range, delete all live keys in one shot.
    /// KEYS: [rev_key, log_key, current_key, names_key]
    /// ARGV: [range_min, range_max]
    /// Returns: array of flattened results:
    ///   [total_deleted, last_rev, name1, value1, create_rev1, mod_rev1, lease1, ...]
    const DELETE_PREFIX_LUA: &'static str = r#"
        local rev_key = KEYS[1]
        local log_key = KEYS[2]
        local current_key = KEYS[3]
        local names_key = KEYS[4]

        local range_min = ARGV[1]
        local range_max = ARGV[2]

        -- Get all names matching the prefix
        local names = redis.call("ZRANGEBYLEX", names_key, range_min, range_max)

        local deleted = 0
        local last_rev = tonumber(redis.call("GET", rev_key) or "0")
        local results = {0, last_rev}

        for _, name in ipairs(names) do
            local current_rev_str = redis.call("HGET", current_key, name)
            if current_rev_str then
                local current_rev = tonumber(current_rev_str)
                local row_key = "{rhino}:row:" .. current_rev
                local row = redis.call("HMGET", row_key, "deleted", "create_revision", "value", "lease", "created")
                local row_deleted = row[1]
                local row_create_rev = row[2]
                local row_value = row[3] or ""
                local row_lease = row[4] or "0"
                local row_created = row[5]

                -- Skip already-deleted keys
                if row_deleted ~= "1" then
                    local actual_create_rev = row_create_rev
                    if row_created == "1" then
                        actual_create_rev = tostring(current_rev)
                    end

                    -- Set unique constraint
                    local constraint_key = "{rhino}:uniq:" .. name .. ":" .. tostring(current_rev)
                    if redis.call("EXISTS", constraint_key) == 0 then
                        redis.call("SET", constraint_key, "1")

                        -- Allocate revision
                        local id = redis.call("INCR", rev_key)

                        -- Store delete marker
                        local new_row_key = "{rhino}:row:" .. id
                        redis.call("HSET", new_row_key,
                            "name", name,
                            "created", "0",
                            "deleted", "1",
                            "create_revision", actual_create_rev,
                            "prev_revision", tostring(current_rev),
                            "lease", "0",
                            "value", row_value,
                            "old_value", row_value)

                        -- Update indexes
                        redis.call("ZADD", log_key, id, tostring(id))
                        redis.call("HSET", current_key, name, id)
                        redis.call("ZADD", "{rhino}:key:" .. name, id, tostring(id))

                        deleted = deleted + 1
                        last_rev = id

                        -- Append prev kv info
                        table.insert(results, name)
                        table.insert(results, row_value)
                        table.insert(results, actual_create_rev)
                        table.insert(results, tostring(current_rev))
                        table.insert(results, row_lease)
                    end
                end
            end
        end

        results[1] = deleted
        results[2] = last_rev
        return results
    "#;

    /// Fill a gap at a revision with a placeholder record.
    /// KEYS: [log_key, rev_key]
    /// ARGV: [revision, gap_name]
    const FILL_LUA: &'static str = r#"
        local row_key = "{rhino}:row:" .. ARGV[1]
        if redis.call("EXISTS", row_key) == 1 then
            return 0
        end

        redis.call("HSET", row_key,
            "name", ARGV[2],
            "created", "0",
            "deleted", "1",
            "create_revision", "0",
            "prev_revision", "0",
            "lease", "0",
            "value", "",
            "old_value", "")
        redis.call("ZADD", KEYS[1], tonumber(ARGV[1]), ARGV[1])

        -- Ensure rev counter is at least past this fill
        local current = tonumber(redis.call("GET", KEYS[2]) or "0")
        if current < tonumber(ARGV[1]) then
            redis.call("SET", KEYS[2], ARGV[1])
        end

        return 1
    "#;

    /// Batch historical revision lookup: for each key, find max revision <= target.
    /// ARGV: [key_revs_1, max_rev_1, key_revs_2, max_rev_2, ...]
    /// Returns: array of revision ids (-1 if not found)
    const HISTORICAL_REVS_LUA: &'static str = r#"
        local results = {}
        for i = 1, #ARGV, 2 do
            local key_revs = ARGV[i]
            local max_rev = tonumber(ARGV[i+1])
            local revs = redis.call("ZRANGEBYSCORE", key_revs, "-inf", max_rev)
            if #revs > 0 then
                table.insert(results, revs[#revs])
            else
                table.insert(results, "-1")
            end
        end
        return results
    "#;
}

/// Redis-backed implementation of the rhino [`Backend`] trait.
pub struct RedisBackend {
    conn: ConnectionManager,
    current_rev: Arc<AtomicI64>,
    notify: Arc<Notify>,
    broadcaster: Arc<Broadcaster>,
    polled_rev: Arc<tokio::sync::watch::Sender<i64>>,
    started: Arc<AtomicBool>,
    scripts: Arc<Scripts>,
    config: RedisConfig,
}

impl RedisBackend {
    /// Create a new Redis backend with the given configuration.
    pub async fn new(config: RedisConfig) -> std::result::Result<Self, BackendError> {
        let client = Client::open(config.dsn.as_str())
            .map_err(|e| BackendError::Internal(format!("invalid redis URL: {e}")))?;

        let conn = ConnectionManager::new(client)
            .await
            .map_err(|e| BackendError::Internal(format!("failed to connect to redis: {e}")))?;

        let (polled_rev_tx, _) = tokio::sync::watch::channel(0i64);

        Ok(Self {
            conn,
            current_rev: Arc::new(AtomicI64::new(0)),
            notify: Arc::new(Notify::new()),
            broadcaster: Arc::new(Broadcaster::new()),
            polled_rev: Arc::new(polled_rev_tx),
            started: Arc::new(AtomicBool::new(false)),
            scripts: Arc::new(Scripts::new()),
            config,
        })
    }

    /// Fill a gap at the given revision with a placeholder record.
    async fn fill(&self, revision: i64) -> Result<()> {
        let name = format!("gap-{revision}");
        let mut conn = self.conn.clone();
        let _: i64 = self
            .scripts
            .fill
            .key(LOG_KEY)
            .key(REV_KEY)
            .arg(revision)
            .arg(&name)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;
        Ok(())
    }

    /// Get the current (max) revision.
    async fn db_current_revision(&self) -> Result<i64> {
        let mut conn = self.conn.clone();
        let rev: Option<i64> = conn
            .get(REV_KEY)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;
        Ok(rev.unwrap_or(0))
    }

    /// Get the compact revision.
    async fn get_compact_revision(&self) -> Result<i64> {
        let mut conn = self.conn.clone();
        let rev: Option<i64> = conn
            .get(COMPACT_REV_STORE)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;
        Ok(rev.unwrap_or(0))
    }

    /// Fetch a single row by revision id.
    async fn get_row(&self, id: i64) -> Result<Option<RowData>> {
        let mut conn = self.conn.clone();
        let row_key = format!("{ROW_PREFIX}{id}");

        let data: redis::Value = redis::cmd("HGETALL")
            .arg(&row_key)
            .query_async(&mut conn)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        Self::parse_row_data(data, id)
    }

    /// Parse HGETALL result into RowData. Handles both RESP2 (Array) and RESP3 (Map).
    fn parse_row_data(data: redis::Value, id: i64) -> Result<Option<RowData>> {
        let map = match data {
            redis::Value::Map(pairs) => {
                let mut m = std::collections::HashMap::new();
                for (k, v) in pairs {
                    let key_str = match &k {
                        redis::Value::BulkString(b) => String::from_utf8_lossy(b).to_string(),
                        redis::Value::SimpleString(s) => s.clone(),
                        _ => continue,
                    };
                    m.insert(key_str, v);
                }
                m
            }
            redis::Value::Array(arr) if arr.len() >= 2 => {
                let mut m = std::collections::HashMap::new();
                let mut i = 0;
                while i + 1 < arr.len() {
                    let key_str = match &arr[i] {
                        redis::Value::BulkString(b) => String::from_utf8_lossy(b).to_string(),
                        redis::Value::SimpleString(s) => s.clone(),
                        _ => {
                            i += 2;
                            continue;
                        }
                    };
                    m.insert(key_str, arr[i + 1].clone());
                    i += 2;
                }
                m
            }
            _ => return Ok(None),
        };

        if map.is_empty() {
            return Ok(None);
        }

        fn get_str(map: &std::collections::HashMap<String, redis::Value>, key: &str) -> String {
            match map.get(key) {
                Some(redis::Value::BulkString(b)) => String::from_utf8_lossy(b).to_string(),
                Some(redis::Value::SimpleString(s)) => s.clone(),
                _ => String::new(),
            }
        }

        fn get_i64(map: &std::collections::HashMap<String, redis::Value>, key: &str) -> i64 {
            get_str(map, key).parse().unwrap_or(0)
        }

        fn get_bytes(map: &std::collections::HashMap<String, redis::Value>, key: &str) -> Vec<u8> {
            match map.get(key) {
                Some(redis::Value::BulkString(b)) => b.clone(),
                Some(redis::Value::SimpleString(s)) => s.as_bytes().to_vec(),
                _ => vec![],
            }
        }

        Ok(Some(RowData {
            id,
            name: get_str(&map, "name"),
            created: get_i64(&map, "created") as i32,
            deleted: get_i64(&map, "deleted") as i32,
            create_revision: get_i64(&map, "create_revision"),
            prev_revision: get_i64(&map, "prev_revision"),
            lease: get_i64(&map, "lease"),
            value: get_bytes(&map, "value"),
            old_value: get_bytes(&map, "old_value"),
        }))
    }

    /// Fetch multiple rows by revision IDs using pipelining.
    async fn get_rows_pipelined(&self, ids: &[i64]) -> Result<Vec<Option<RowData>>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        let mut conn = self.conn.clone();
        let mut pipe = redis::pipe();

        for id in ids {
            pipe.cmd("HGETALL").arg(format!("{ROW_PREFIX}{id}"));
        }

        let results: Vec<redis::Value> = pipe
            .query_async(&mut conn)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        let mut rows = Vec::with_capacity(ids.len());
        for (i, data) in results.into_iter().enumerate() {
            rows.push(Self::parse_row_data(data, ids[i])?);
        }

        Ok(rows)
    }

    /// Convert a RowData to an Event.
    fn row_to_event(row: &RowData, keys_only: bool, with_old_value: bool) -> Event {
        let is_create = row.created != 0;
        let is_delete = row.deleted != 0;
        let actual_create_rev = if is_create {
            row.id
        } else {
            row.create_revision
        };

        let value = if keys_only { vec![] } else { row.value.clone() };

        let old_value = if !keys_only && with_old_value {
            row.old_value.clone()
        } else {
            vec![]
        };

        Event {
            create: is_create,
            delete: is_delete,
            kv: KeyValue {
                key: row.name.clone(),
                value,
                version: 0,
                create_revision: actual_create_rev,
                mod_revision: row.id,
                lease: row.lease,
            },
            prev_kv: if is_create {
                None
            } else {
                Some(KeyValue {
                    key: row.name.clone(),
                    value: old_value,
                    version: 0,
                    create_revision: actual_create_rev,
                    mod_revision: row.prev_revision,
                    lease: row.lease,
                })
            },
        }
    }

    /// Compute the ZRANGEBYLEX upper bound for a prefix.
    /// Returns exclusive `(` bound by incrementing the last byte, or `+` if
    /// the prefix is empty or ends with 0xFF.
    fn prefix_range_max(prefix: &str) -> String {
        if prefix.is_empty() {
            return "+".to_string();
        }
        let bytes = prefix.as_bytes();
        if let Some(&last) = bytes.last() {
            if last == 0xFF {
                // Can't increment past 0xFF — use unbounded max
                return "+".to_string();
            }
            let mut upper = bytes.to_vec();
            *upper.last_mut().unwrap() = last + 1;
            format!("({}", String::from_utf8_lossy(&upper))
        } else {
            "+".to_string()
        }
    }

    /// Internal get: fetch a single key (the latest version).
    async fn get_internal(
        &self,
        key: &str,
        revision: i64,
        include_deleted: bool,
        keys_only: bool,
    ) -> Result<(i64, Option<Event>)> {
        let current_rev = self.cached_revision().await?;
        let compact_rev = self.get_compact_revision().await?;

        if revision > current_rev {
            return Err(BackendError::FutureRev);
        }
        if revision > 0 && revision < compact_rev {
            return Err(BackendError::Compacted);
        }

        let mut conn = self.conn.clone();

        let target_rev = if revision == 0 {
            let rev: Option<i64> = conn
                .hget(CURRENT_KEY, key)
                .await
                .map_err(|e| BackendError::Internal(e.to_string()))?;
            match rev {
                Some(r) => r,
                None => return Ok((current_rev, None)),
            }
        } else {
            let key_revs = format!("{KEY_REVS_PREFIX}{key}");
            let revs: Vec<i64> = redis::cmd("ZRANGEBYSCORE")
                .arg(&key_revs)
                .arg("-inf")
                .arg(revision)
                .query_async(&mut conn)
                .await
                .map_err(|e| BackendError::Internal(e.to_string()))?;
            match revs.last() {
                Some(&r) => r,
                None => return Ok((current_rev, None)),
            }
        };

        let row = self.get_row(target_rev).await?;
        match row {
            Some(row) => {
                if !include_deleted && row.deleted != 0 {
                    return Ok((current_rev, None));
                }
                let event = Self::row_to_event(&row, keys_only, false);
                Ok((current_rev, Some(event)))
            }
            None => Ok((current_rev, None)),
        }
    }

    /// Core list query for prefix scans. Returns (current_revision, events).
    async fn list_internal(
        &self,
        prefix: &str,
        start_key: &str,
        limit: i64,
        revision: i64,
        include_deleted: bool,
        keys_only: bool,
    ) -> Result<(i64, Vec<Event>)> {
        let current_rev = self.cached_revision().await?;
        let compact_rev = self.get_compact_revision().await?;

        if revision > current_rev {
            return Err(BackendError::FutureRev);
        }
        if revision > 0 && revision < compact_rev {
            return Err(BackendError::Compacted);
        }

        let mut conn = self.conn.clone();

        let range_min = format!("[{prefix}");
        let range_max = Self::prefix_range_max(prefix);

        let names: Vec<String> = redis::cmd("ZRANGEBYLEX")
            .arg(NAMES_KEY)
            .arg(&range_min)
            .arg(&range_max)
            .query_async(&mut conn)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        let filtered_names: Vec<&String> = names
            .iter()
            .filter(|name| start_key.is_empty() || name.as_str() >= start_key)
            .collect();

        if filtered_names.is_empty() {
            return Ok((current_rev, vec![]));
        }

        // Batch-fetch the target revision for each name
        let target_revs: Vec<(String, i64)> = if revision == 0 {
            let mut pipe = redis::pipe();
            for name in &filtered_names {
                pipe.cmd("HGET").arg(CURRENT_KEY).arg(name.as_str());
            }
            let revs: Vec<Option<i64>> = pipe
                .query_async(&mut conn)
                .await
                .map_err(|e| BackendError::Internal(e.to_string()))?;

            filtered_names
                .iter()
                .zip(revs)
                .filter_map(|(name, rev)| rev.map(|r| ((*name).clone(), r)))
                .collect()
        } else {
            let mut invocation = self.scripts.historical_revs.prepare_invoke();
            for name in &filtered_names {
                invocation
                    .arg(format!("{KEY_REVS_PREFIX}{name}"))
                    .arg(revision);
            }
            let revs: Vec<i64> = invocation
                .invoke_async(&mut conn)
                .await
                .map_err(|e| BackendError::Internal(e.to_string()))?;

            filtered_names
                .iter()
                .zip(revs)
                .filter(|(_, r)| *r >= 0)
                .map(|(name, r)| ((*name).clone(), r))
                .collect()
        };

        if target_revs.is_empty() {
            return Ok((current_rev, vec![]));
        }

        // Batch-fetch all rows via pipeline
        let rev_ids: Vec<i64> = target_revs.iter().map(|(_, r)| *r).collect();
        let rows = self.get_rows_pipelined(&rev_ids).await?;

        let mut events = Vec::new();

        for row in rows.into_iter().flatten() {
            if !include_deleted && row.deleted != 0 {
                continue;
            }
            events.push(Self::row_to_event(&row, keys_only, false));

            if limit > 0 && events.len() as i64 >= limit {
                break;
            }
        }

        Ok((current_rev, events))
    }

    /// Query rows after a given revision for the poll loop. Uses pipelining.
    async fn after(&self, revision: i64, limit: i64) -> Result<Vec<Event>> {
        let mut conn = self.conn.clone();

        let mut cmd = redis::cmd("ZRANGEBYSCORE");
        cmd.arg(LOG_KEY).arg(format!("({revision}")).arg("+inf");
        if limit > 0 {
            cmd.arg("LIMIT").arg(0).arg(limit);
        }

        let rev_ids: Vec<i64> = cmd
            .query_async(&mut conn)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        let rows = self.get_rows_pipelined(&rev_ids).await?;

        let mut events = Vec::with_capacity(rev_ids.len());
        for row in rows.into_iter().flatten() {
            events.push(Self::row_to_event(&row, false, true));
        }

        Ok(events)
    }

    /// Return the cached current revision, fetching from Redis if not yet cached.
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

    /// Start background tasks lazily on first watch subscription.
    fn ensure_background_tasks(&self) {
        if self
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let poll_start_rev = self.current_rev.load(Ordering::Acquire);

            let make_backend = |share_rev: bool| {
                Arc::new(RedisBackend {
                    conn: self.conn.clone(),
                    current_rev: if share_rev {
                        self.current_rev.clone()
                    } else {
                        Arc::new(AtomicI64::new(0))
                    },
                    notify: if share_rev {
                        self.notify.clone()
                    } else {
                        Arc::new(Notify::new())
                    },
                    broadcaster: self.broadcaster.clone(),
                    polled_rev: self.polled_rev.clone(),
                    started: self.started.clone(),
                    scripts: self.scripts.clone(),
                    config: self.config.clone(),
                })
            };

            let poll_backend = make_backend(true);
            let compact_backend = make_backend(false);
            let ttl_backend = make_backend(true);

            tokio::spawn(async move { poll_backend.poll_loop(poll_start_rev).await });
            tokio::spawn(async move { compact_backend.compact_loop().await });
            tokio::spawn(async move { ttl_backend.ttl_loop().await });
            debug!("background tasks started (first watch subscription)");
        }
    }

    /// Background poll loop: detects new revisions and broadcasts events to watchers.
    async fn poll_loop(self: Arc<Self>, start_revision: i64) {
        let mut poll_revision = start_revision;

        let mut interval = tokio::time::interval(POLL_INTERVAL);
        let mut skip: i64 = 0;
        let mut skip_time = tokio::time::Instant::now();

        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Enable the Notified future so notifications arriving before the
            // first poll (inside select!) are not lost.
            notified.as_mut().enable();

            let _ = self.polled_rev.send(poll_revision);

            let events = match self.after(poll_revision, 500).await {
                Ok(e) => e,
                Err(e) => {
                    error!("poll error: {e}");
                    tokio::select! {
                        _ = interval.tick() => {},
                        _ = &mut notified => {},
                    }
                    continue;
                }
            };

            if events.is_empty() {
                tokio::select! {
                    _ = interval.tick() => {},
                    _ = &mut notified => {},
                }
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
                        // Gap persisted beyond timeout — skip past it by filling
                        // and re-polling so the poll loop doesn't stall forever.
                        warn!(
                            "skipping revision gap at {next}, current event rev={}",
                            event.kv.mod_revision
                        );
                        if let Err(e) = self.fill(next).await {
                            warn!("fill revision {next} failed: {e}");
                        }
                        skip = 0;
                        self.notify.notify_waiters();
                        break;
                    } else if skip != next {
                        skip = next;
                        skip_time = tokio::time::Instant::now();
                        self.notify.notify_waiters();
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
                self.current_rev.fetch_max(rev, Ordering::AcqRel);
                poll_revision = rev;

                if !sequential.is_empty() {
                    self.broadcaster.send(sequential).await;
                }
            }
        }
    }

    /// Background TTL expiration loop.
    async fn ttl_loop(self: Arc<Self>) {
        use std::collections::HashMap;
        use tokio::time::Instant;

        tokio::time::sleep(Duration::from_secs(1)).await;

        let mut expiries: HashMap<String, (i64, Instant)> = HashMap::new();

        // Seed from existing leased keys by scanning current index.
        // Parse the raw HGETALL ourselves to extract (name, rev_id) pairs,
        // then use get_rows_pipelined (which handles RESP2/RESP3 via parse_row_data).
        let mut conn = self.conn.clone();
        let pairs = Self::parse_string_i64_hash(&mut conn, CURRENT_KEY).await;

        let now = Instant::now();
        let rev_ids: Vec<i64> = pairs.iter().map(|(_, r)| *r).collect();
        if let Ok(rows) = self.get_rows_pipelined(&rev_ids).await {
            for ((name, _), row_opt) in pairs.iter().zip(rows) {
                if let Some(row) = row_opt
                    && row.lease > 0
                    && row.deleted == 0
                {
                    expiries.insert(
                        name.clone(),
                        (row.id, now + Duration::from_secs(row.lease as u64)),
                    );
                }
            }
        }

        let mut broadcast_rx = self.broadcaster.subscribe().await;
        let mut check_interval = tokio::time::interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                _ = check_interval.tick() => {
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
                            for event in events.iter() {
                                if event.delete {
                                    expiries.remove(&event.kv.key);
                                } else if event.kv.lease > 0 {
                                    expiries.insert(
                                        event.kv.key.clone(),
                                        (event.kv.mod_revision, now + Duration::from_secs(event.kv.lease as u64)),
                                    );
                                } else {
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

    /// Parse a Redis hash (HGETALL) as Vec<(String, i64)>, handling both RESP2 and RESP3.
    async fn parse_string_i64_hash(conn: &mut ConnectionManager, key: &str) -> Vec<(String, i64)> {
        let data: redis::Value = redis::cmd("HGETALL")
            .arg(key)
            .query_async(conn)
            .await
            .unwrap_or(redis::Value::Array(vec![]));

        let kv_pairs: Vec<(String, String)> = match data {
            redis::Value::Map(pairs) => pairs
                .into_iter()
                .filter_map(|(k, v)| {
                    let ks = match k {
                        redis::Value::BulkString(b) => String::from_utf8_lossy(&b).to_string(),
                        redis::Value::SimpleString(s) => s,
                        _ => return None,
                    };
                    let vs = match v {
                        redis::Value::BulkString(b) => String::from_utf8_lossy(&b).to_string(),
                        redis::Value::SimpleString(s) => s,
                        redis::Value::Int(n) => n.to_string(),
                        _ => return None,
                    };
                    Some((ks, vs))
                })
                .collect(),
            redis::Value::Array(arr) => {
                let mut result = Vec::new();
                let mut i = 0;
                while i + 1 < arr.len() {
                    let ks = match &arr[i] {
                        redis::Value::BulkString(b) => String::from_utf8_lossy(b).to_string(),
                        redis::Value::SimpleString(s) => s.clone(),
                        _ => {
                            i += 2;
                            continue;
                        }
                    };
                    let vs = match &arr[i + 1] {
                        redis::Value::BulkString(b) => String::from_utf8_lossy(b).to_string(),
                        redis::Value::SimpleString(s) => s.clone(),
                        redis::Value::Int(n) => n.to_string(),
                        _ => String::new(),
                    };
                    result.push((ks, vs));
                    i += 2;
                }
                result
            }
            _ => vec![],
        };

        kv_pairs
            .into_iter()
            .filter_map(|(k, v)| v.parse::<i64>().ok().map(|n| (k, n)))
            .collect()
    }

    /// Background compaction loop.
    async fn compact_loop(self: Arc<Self>) {
        if self.config.compact_interval.is_zero() {
            debug!("automatic compaction disabled");
            return;
        }

        // Apply jitter (5% of interval, matching kine/SQLite default)
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

        let mut interval = tokio::time::interval(self.config.compact_interval + jitter);
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

    /// Run a single compaction pass.
    async fn compact_once(&self) -> Result<()> {
        let compact_rev = self.get_compact_revision().await?;
        let current_rev = self.cached_revision().await?;

        let target = current_rev - self.config.compact_min_retain;
        if target <= compact_rev || target < 0 {
            return Err(BackendError::Compacted);
        }

        self.compact_to(target).await?;

        debug!("compacted up to revision {}/{}", target, current_rev);

        Ok(())
    }

    /// Compact all superseded and deleted rows up to the given target revision.
    /// Processes in batches of `compact_batch_size` to avoid blocking.
    async fn compact_to(&self, target: i64) -> Result<i64> {
        let mut conn = self.conn.clone();
        let batch_size = self.config.compact_batch_size;
        let mut total_deleted = 0i64;
        let mut cursor: i64 = 0; // tracks progress through the log

        loop {
            // Fetch a batch of revision IDs from the log
            let batch_min = if cursor == 0 {
                "-inf".to_string()
            } else {
                format!("({cursor}")
            };

            let rev_ids: Vec<i64> = redis::cmd("ZRANGEBYSCORE")
                .arg(LOG_KEY)
                .arg(&batch_min)
                .arg(target)
                .arg("LIMIT")
                .arg(0)
                .arg(batch_size)
                .query_async(&mut conn)
                .await
                .map_err(|e| BackendError::Internal(e.to_string()))?;

            if rev_ids.is_empty() {
                break;
            }

            cursor = *rev_ids.last().unwrap();

            // Batch-fetch all rows
            let rows = self.get_rows_pipelined(&rev_ids).await?;

            // Collect live (non-deleted) rows that need a superseded check
            let mut candidates: Vec<(i64, RowData)> = Vec::new();
            let mut to_delete: Vec<(i64, RowData)> = Vec::new();

            for (id, row_opt) in rev_ids.iter().zip(rows) {
                let Some(row) = row_opt else { continue };

                if row.deleted != 0 {
                    to_delete.push((*id, row));
                } else {
                    candidates.push((*id, row));
                }
            }

            // Batch-check which live rows are superseded via pipelined HGET
            if !candidates.is_empty() {
                let mut pipe = redis::pipe();
                for (_, row) in &candidates {
                    pipe.cmd("HGET").arg(CURRENT_KEY).arg(&row.name);
                }
                let current_revs: Vec<Option<i64>> = pipe
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| BackendError::Internal(e.to_string()))?;

                for ((id, row), cur_rev) in candidates.into_iter().zip(current_revs) {
                    let superseded = match cur_rev {
                        Some(cur) => cur != id,
                        None => true,
                    };
                    if superseded {
                        to_delete.push((id, row));
                    }
                }
            }

            if !to_delete.is_empty() {
                let mut pipe = redis::pipe();

                for (id, row) in &to_delete {
                    pipe.cmd("DEL").arg(format!("{ROW_PREFIX}{id}"));
                    pipe.cmd("ZREM").arg(LOG_KEY).arg(id.to_string());
                    pipe.cmd("ZREM")
                        .arg(format!("{KEY_REVS_PREFIX}{}", row.name))
                        .arg(id.to_string());
                    // Clean up unique constraint key for ALL rows, including prev_revision=0
                    pipe.cmd("DEL")
                        .arg(format!("{UNIQ_PREFIX}{}:{}", row.name, row.prev_revision));
                }

                let _: Vec<redis::Value> = pipe
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| BackendError::Internal(e.to_string()))?;

                total_deleted += to_delete.len() as i64;

                // Clean up current index and names set for deleted keys with no remaining revisions
                let mut names_to_check: Vec<String> = Vec::new();
                for (_, row) in &to_delete {
                    if row.deleted != 0 && !names_to_check.contains(&row.name) {
                        names_to_check.push(row.name.clone());
                    }
                }

                if !names_to_check.is_empty() {
                    let mut check_pipe = redis::pipe();
                    for name in &names_to_check {
                        check_pipe
                            .cmd("ZCARD")
                            .arg(format!("{KEY_REVS_PREFIX}{name}"));
                    }
                    let counts: Vec<i64> = check_pipe
                        .query_async(&mut conn)
                        .await
                        .map_err(|e| BackendError::Internal(e.to_string()))?;

                    let mut cleanup_pipe = redis::pipe();
                    let mut has_cleanup = false;
                    for (name, count) in names_to_check.iter().zip(counts) {
                        if count == 0 {
                            cleanup_pipe.cmd("HDEL").arg(CURRENT_KEY).arg(name);
                            cleanup_pipe.cmd("ZREM").arg(NAMES_KEY).arg(name);
                            cleanup_pipe
                                .cmd("DEL")
                                .arg(format!("{KEY_REVS_PREFIX}{name}"));
                            has_cleanup = true;
                        }
                    }

                    if has_cleanup {
                        let _: Vec<redis::Value> = cleanup_pipe
                            .query_async(&mut conn)
                            .await
                            .map_err(|e| BackendError::Internal(e.to_string()))?;
                    }
                }
            }

            // If we got fewer than batch_size, we're done
            if (rev_ids.len() as i64) < batch_size {
                break;
            }
        }

        // Update compact revision
        let _: () = conn
            .set(COMPACT_REV_STORE, target)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        Ok(total_deleted)
    }
}

/// Internal row data structure matching the kine table columns.
struct RowData {
    id: i64,
    name: String,
    created: i32,
    deleted: i32,
    create_revision: i64,
    prev_revision: i64,
    lease: i64,
    value: Vec<u8>,
    old_value: Vec<u8>,
}

#[async_trait]
impl Backend for RedisBackend {
    async fn start(&self) -> Result<()> {
        // Sync cached revision from Redis (handles both fresh start and restart)
        let rev = self.db_current_revision().await?;
        self.current_rev.store(rev, Ordering::Release);

        // Create health check key (idempotent via atomic create script)
        match self
            .create("/registry/health", b"{\"health\":\"true\"}", 0)
            .await
        {
            Ok(_) | Err(BackendError::KeyExists) => {}
            Err(e) => warn!("failed to create health check key: {e}"),
        }

        // Start background tasks eagerly
        self.ensure_background_tasks();

        debug!("redis backend started (revision: {})", rev);
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
        let mut conn = self.conn.clone();

        let id: i64 = self
            .scripts
            .create
            .key(REV_KEY)
            .key(LOG_KEY)
            .key(CURRENT_KEY)
            .key(NAMES_KEY)
            .arg(key)
            .arg(lease)
            .arg(value)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("KEY_EXISTS") {
                    BackendError::KeyExists
                } else {
                    BackendError::Internal(msg)
                }
            })?;

        self.current_rev.store(id, Ordering::Release);
        self.notify.notify_waiters();
        Ok(id)
    }

    async fn delete(&self, key: &str, revision: i64) -> Result<(i64, Option<KeyValue>, bool)> {
        let mut conn = self.conn.clone();

        let result: Vec<redis::Value> = self
            .scripts
            .delete
            .key(REV_KEY)
            .key(LOG_KEY)
            .key(CURRENT_KEY)
            .key(NAMES_KEY)
            .arg(key)
            .arg(revision)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        let status = parse_i64_from_value(&result[0]);
        let rev = parse_i64_from_value(&result[1]);

        match status {
            2 => Ok((rev, None, true)),
            0 => {
                let name = parse_string_from_value(&result[2]);
                if name.is_empty() {
                    Ok((rev, None, false))
                } else {
                    Ok((
                        rev,
                        Some(KeyValue {
                            key: name,
                            value: parse_bytes_from_value(&result[3]),
                            version: 0,
                            create_revision: parse_i64_from_value(&result[5]),
                            mod_revision: parse_i64_from_value(&result[6]),
                            lease: parse_i64_from_value(&result[4]),
                        }),
                        false,
                    ))
                }
            }
            1 => {
                self.current_rev.store(rev, Ordering::Release);
                self.notify.notify_waiters();
                Ok((
                    rev,
                    Some(KeyValue {
                        key: parse_string_from_value(&result[2]),
                        value: parse_bytes_from_value(&result[3]),
                        version: 0,
                        create_revision: parse_i64_from_value(&result[5]),
                        mod_revision: parse_i64_from_value(&result[6]),
                        lease: parse_i64_from_value(&result[4]),
                    }),
                    true,
                ))
            }
            _ => Err(BackendError::Internal(format!(
                "unexpected delete status: {status}"
            ))),
        }
    }

    async fn delete_prefix(&self, prefix: &str) -> Result<(i64, i64, Vec<KeyValue>)> {
        let mut conn = self.conn.clone();

        let range_min = format!("[{prefix}");
        let range_max = Self::prefix_range_max(prefix);

        let result: Vec<redis::Value> = self
            .scripts
            .delete_prefix
            .key(REV_KEY)
            .key(LOG_KEY)
            .key(CURRENT_KEY)
            .key(NAMES_KEY)
            .arg(&range_min)
            .arg(&range_max)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        let deleted = parse_i64_from_value(&result[0]);
        let last_rev = parse_i64_from_value(&result[1]);

        if deleted == 0 {
            let rev = self.cached_revision().await?;
            return Ok((rev, 0, Vec::new()));
        }

        self.current_rev.store(last_rev, Ordering::Release);
        self.notify.notify_waiters();

        // Parse prev_kvs from flattened result:
        // [deleted, last_rev, name1, value1, create_rev1, mod_rev1, lease1, ...]
        let mut prev_kvs = Vec::with_capacity(deleted as usize);
        let mut i = 2;
        while i + 4 < result.len() {
            prev_kvs.push(KeyValue {
                key: parse_string_from_value(&result[i]),
                value: parse_bytes_from_value(&result[i + 1]),
                version: 0,
                create_revision: parse_i64_from_value(&result[i + 2]),
                mod_revision: parse_i64_from_value(&result[i + 3]),
                lease: parse_i64_from_value(&result[i + 4]),
            });
            i += 5;
        }

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
        if !prefix.ends_with('/') {
            let (rev, event) = self
                .get_internal(prefix, revision, false, keys_only)
                .await?;
            let kvs = event.map(|e| vec![e.kv]).unwrap_or_default();
            return Ok((rev, kvs));
        }

        let effective_start = if start_key == prefix { "" } else { start_key };

        let (rev, events) = self
            .list_internal(prefix, effective_start, limit, revision, false, keys_only)
            .await?;

        let kvs = events.into_iter().map(|e| e.kv).collect();
        Ok((rev, kvs))
    }

    async fn count(&self, prefix: &str, start_key: &str, revision: i64) -> Result<(i64, i64)> {
        if !prefix.ends_with('/') {
            let (rev, event) = self.get_internal(prefix, revision, false, true).await?;
            let count = if event.is_some() { 1 } else { 0 };
            return Ok((rev, count));
        }

        let effective_start = if start_key == prefix { "" } else { start_key };

        let (rev, events) = self
            .list_internal(prefix, effective_start, 0, revision, false, true)
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
        let mut conn = self.conn.clone();

        let result: Vec<redis::Value> = self
            .scripts
            .update
            .key(REV_KEY)
            .key(LOG_KEY)
            .key(CURRENT_KEY)
            .key(NAMES_KEY)
            .arg(key)
            .arg(revision)
            .arg(lease)
            .arg(value)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        let new_rev = parse_i64_from_value(&result[0]);

        if new_rev == -1 {
            let rev = self.cached_revision().await?;
            return Ok((rev, None, false));
        }

        if new_rev == 0 {
            let current_mod_rev = parse_i64_from_value(&result[1]);
            let create_rev = parse_i64_from_value(&result[2]);
            let current_value = parse_bytes_from_value(&result[3]);
            let current_lease = parse_i64_from_value(&result[4]);
            return Ok((
                current_mod_rev,
                Some(KeyValue {
                    key: key.to_string(),
                    value: current_value,
                    version: 0,
                    create_revision: create_rev,
                    mod_revision: current_mod_rev,
                    lease: current_lease,
                }),
                false,
            ));
        }

        self.current_rev.store(new_rev, Ordering::Release);
        self.notify.notify_waiters();

        Ok((
            new_rev,
            Some(KeyValue {
                key: key.to_string(),
                value: value.to_vec(),
                version: 0,
                create_revision: parse_i64_from_value(&result[2]),
                mod_revision: new_rev,
                lease,
            }),
            true,
        ))
    }

    async fn watch(&self, key: &str, revision: i64) -> Result<WatchResult> {
        self.ensure_background_tasks();

        let (tx, rx) = mpsc::channel(100);

        let current_rev = self.cached_revision().await?;
        let compact_rev = self.get_compact_revision().await?;

        if revision > 0 && revision < compact_rev {
            return Err(BackendError::Compacted);
        }

        let mut broadcast_rx = self.broadcaster.subscribe().await;
        let prefix = key.to_string();
        let start_rev = if revision > 0 { revision - 1 } else { 0 };

        let backend = Arc::new(RedisBackend {
            conn: self.conn.clone(),
            current_rev: self.current_rev.clone(),
            notify: self.notify.clone(),
            broadcaster: self.broadcaster.clone(),
            polled_rev: self.polled_rev.clone(),
            started: self.started.clone(),
            scripts: self.scripts.clone(),
            config: self.config.clone(),
        });

        tokio::spawn(async move {
            let mut last_seen_rev = start_rev;

            // Fetch historical events since the requested revision
            if start_rev > 0
                && let Ok(events) = backend.after(start_rev, 0).await
            {
                let check_prefix = prefix.ends_with('/');
                let filtered: Vec<Event> = events
                    .into_iter()
                    .filter(|e| {
                        !e.kv.key.starts_with("gap-")
                            && if check_prefix {
                                e.kv.key.starts_with(&prefix)
                            } else {
                                e.kv.key == prefix
                            }
                    })
                    .collect();

                last_seen_rev = filtered
                    .last()
                    .map(|e| e.kv.mod_revision)
                    .unwrap_or(last_seen_rev);

                if !filtered.is_empty() && tx.send(filtered).await.is_err() {
                    return;
                }
            }

            // Stream live events, filtering by prefix and deduplicating
            // against events already delivered from the historical query.
            let check_prefix = prefix.ends_with('/');
            loop {
                match broadcast_rx.recv().await {
                    Some(events) => {
                        let filtered: Vec<Event> = events
                            .iter()
                            .filter(|e| {
                                e.kv.mod_revision > last_seen_rev
                                    && if check_prefix {
                                        e.kv.key.starts_with(&prefix)
                                    } else {
                                        e.kv.key == prefix
                                    }
                            })
                            .cloned()
                            .collect();
                        if let Some(last) = filtered.last() {
                            last_seen_rev = last.kv.mod_revision;
                        }
                        if !filtered.is_empty() && tx.send(filtered).await.is_err() {
                            return;
                        }
                    }
                    None => return,
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
        let mut conn = self.conn.clone();
        let info: String = redis::cmd("INFO")
            .arg("memory")
            .query_async(&mut conn)
            .await
            .map_err(|e| BackendError::Internal(e.to_string()))?;

        for line in info.lines() {
            if let Some(val) = line.strip_prefix("used_memory:") {
                return val
                    .trim()
                    .parse::<i64>()
                    .map_err(|e| BackendError::Internal(format!("parse used_memory: {e}")));
            }
        }

        Ok(0)
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

        let deleted = self.compact_to(target).await?;

        debug!(
            "manual compact: deleted {} rows up to revision {}",
            deleted, target
        );

        self.cached_revision().await
    }
}

/// Helper: extract i64 from a redis::Value.
fn parse_i64_from_value(v: &redis::Value) -> i64 {
    match v {
        redis::Value::Int(n) => *n,
        redis::Value::BulkString(b) => String::from_utf8_lossy(b).parse().unwrap_or(0),
        redis::Value::SimpleString(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// Helper: extract String from a redis::Value.
fn parse_string_from_value(v: &redis::Value) -> String {
    match v {
        redis::Value::BulkString(b) => String::from_utf8_lossy(b).to_string(),
        redis::Value::SimpleString(s) => s.clone(),
        _ => String::new(),
    }
}

/// Helper: extract bytes from a redis::Value.
fn parse_bytes_from_value(v: &redis::Value) -> Vec<u8> {
    match v {
        redis::Value::BulkString(b) => b.clone(),
        redis::Value::SimpleString(s) => s.as_bytes().to_vec(),
        _ => vec![],
    }
}
