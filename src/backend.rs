use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("key already exists")]
    KeyExists,
    #[error("revision has been compacted")]
    Compacted,
    #[error("revision is in the future")]
    FutureRev,
    #[error("{0}")]
    Internal(String),
}

#[derive(Debug, Clone)]
pub struct KeyValue {
    pub key: String,
    pub value: Vec<u8>,
    pub version: i64,
    pub create_revision: i64,
    pub mod_revision: i64,
    pub lease: i64,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub delete: bool,
    pub create: bool,
    pub kv: KeyValue,
    pub prev_kv: Option<KeyValue>,
}

pub struct WatchResult {
    pub current_revision: i64,
    pub compact_revision: i64,
    pub events: mpsc::Receiver<Vec<Event>>,
}

pub type Result<T> = std::result::Result<T, BackendError>;

/// The core backend trait that database drivers must implement.
///
/// This mirrors kine's `Backend` interface. Each method corresponds to a
/// high-level etcd operation that the gRPC server layer dispatches to.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Start the backend, performing any initialization (schema creation, etc).
    async fn start(&self) -> Result<()>;

    /// Get a single key. Returns (current_revision, Option<kv>).
    async fn get(
        &self,
        key: &str,
        range_end: &str,
        limit: i64,
        revision: i64,
        keys_only: bool,
    ) -> Result<(i64, Option<KeyValue>)>;

    /// Create a new key. Returns the revision of the create.
    async fn create(&self, key: &str, value: &[u8], lease: i64) -> Result<i64>;

    /// Delete a key at the given revision. Returns (revision, prev_kv, succeeded).
    async fn delete(
        &self,
        key: &str,
        revision: i64,
    ) -> Result<(i64, Option<KeyValue>, bool)>;

    /// Delete all live keys matching a prefix in a single transaction.
    /// Returns (latest_revision, deleted_count, prev_kvs).
    async fn delete_prefix(
        &self,
        prefix: &str,
    ) -> Result<(i64, i64, Vec<KeyValue>)>;

    /// List keys matching prefix. Returns (current_revision, kvs).
    async fn list(
        &self,
        prefix: &str,
        start_key: &str,
        limit: i64,
        revision: i64,
        keys_only: bool,
    ) -> Result<(i64, Vec<KeyValue>)>;

    /// Count keys matching prefix. Returns (current_revision, count).
    async fn count(
        &self,
        prefix: &str,
        start_key: &str,
        revision: i64,
    ) -> Result<(i64, i64)>;

    /// Update a key at the given revision. Returns (revision, prev_kv, succeeded).
    async fn update(
        &self,
        key: &str,
        value: &[u8],
        revision: i64,
        lease: i64,
    ) -> Result<(i64, Option<KeyValue>, bool)>;

    /// Watch for changes on a key starting from the given revision.
    async fn watch(&self, key: &str, revision: i64) -> Result<WatchResult>;

    /// Return the size of the database in bytes.
    async fn db_size(&self) -> Result<i64>;

    /// Return the current revision.
    async fn current_revision(&self) -> Result<i64>;

    /// Block until the poll loop has processed all rows up to (and including) the given revision.
    /// This ensures that any events at or below `revision` have been broadcast to watchers.
    async fn wait_for_sync_to(&self, revision: i64);

    /// Compact revisions up to the given revision. Returns the compacted revision.
    async fn compact(&self, revision: i64) -> Result<i64>;
}
