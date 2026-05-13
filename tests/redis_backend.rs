use rhino::{Backend, BackendError, RedisBackend, RedisConfig};
use std::time::Duration;

// These tests require a running Redis instance at localhost:6379.
// Each test uses a unique key prefix via FLUSHDB to avoid cross-test interference.
// Run with: cargo test --test redis_backend -- --test-threads=1
//
// To skip these tests when no Redis is available, set SKIP_REDIS_TESTS=1.

async fn test_backend() -> Option<RedisBackend> {
    if std::env::var("SKIP_REDIS_TESTS").is_ok() {
        return None;
    }

    let dsn = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());

    // Use a separate DB number for tests (DB 15 by convention)
    let test_dsn = if dsn.contains('/') {
        dsn.clone()
    } else {
        format!("{dsn}/15")
    };

    // Flush the test database
    let client = match redis::Client::open(test_dsn.as_str()) {
        Ok(c) => c,
        Err(_) => return None,
    };
    let mut conn = match redis::aio::ConnectionManager::new(client).await {
        Ok(c) => c,
        Err(_) => return None,
    };
    let _: std::result::Result<(), _> = redis::cmd("FLUSHDB").query_async(&mut conn).await;

    let config = RedisConfig {
        dsn: test_dsn,
        compact_interval: Duration::ZERO,
        ..Default::default()
    };

    let backend = match RedisBackend::new(config).await {
        Ok(b) => b,
        Err(_) => return None,
    };
    backend.start().await.unwrap();

    // Give poll loop a moment to start
    tokio::time::sleep(Duration::from_millis(50)).await;

    Some(backend)
}

macro_rules! skip_if_no_redis {
    ($b:expr) => {
        match $b {
            Some(b) => b,
            None => {
                eprintln!("skipping test: no Redis available");
                return;
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Tests ported from kine: same patterns as sqlite_backend.rs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_backend_create() {
    let b = skip_if_no_redis!(test_backend().await);

    let base_rev = b.current_revision().await.unwrap();

    let rev = b.create("/test/a", &[], 0).await.unwrap();
    assert_eq!(rev, base_rev + 1);

    let err = b.create("/test/a", &[], 0).await;
    assert!(matches!(err, Err(BackendError::KeyExists)));

    let rev = b.create("/test/a/b", &[], 0).await.unwrap();
    assert_eq!(rev, base_rev + 2);

    let rev = b.create("/test/a/b/c", &[], 0).await.unwrap();
    assert_eq!(rev, base_rev + 3);

    let rev = b.create("/test/b", &[], 1).await.unwrap();
    assert_eq!(rev, base_rev + 4);

    let (srev, count) = b.count("/test/", "", 0).await.unwrap();
    assert_eq!(srev, base_rev + 4);
    assert_eq!(count, 4);

    b.create("/test/c", &[], 0).await.unwrap();
    let (_, count) = b.count("/test/", "", 0).await.unwrap();
    assert_eq!(count, 5);
}

#[tokio::test]
async fn test_backend_get() {
    let b = skip_if_no_redis!(test_backend().await);

    let base_rev = b.current_revision().await.unwrap();

    let _rev = b.create("/test/a", b"b", 1).await.unwrap();

    let (srev, ent) = b.get("/test/a", "", 0, 0, false).await.unwrap();
    let ent = ent.expect("key should exist");
    assert_eq!(srev, base_rev + 1);
    assert_eq!(ent.key, "/test/a");
    assert_eq!(ent.value, b"b");
    assert_eq!(ent.lease, 1);
    assert_eq!(ent.mod_revision, base_rev + 1);
    assert_eq!(ent.create_revision, base_rev + 1);

    let (_, _, _) = b.delete("/test/a", ent.mod_revision).await.unwrap();

    let rev = b.create("/test/a", b"c", 0).await.unwrap();

    let (_, _, _) = b.update("/test/a", b"d", rev, 0).await.unwrap();

    let (rev, ent) = b.get("/test/a", "", 0, 0, false).await.unwrap();
    let ent = ent.expect("key should exist");
    assert!(rev > base_rev);
    assert_eq!(ent.value, b"d");

    let (_, ent) = b.get("/test/doesnotexist", "", 0, 0, false).await.unwrap();
    assert!(ent.is_none());
}

#[tokio::test]
async fn test_backend_update() {
    let b = skip_if_no_redis!(test_backend().await);

    let base_rev = b.current_revision().await.unwrap();

    let rev = b.create("/test/a", b"b", 1).await.unwrap();
    assert_eq!(rev, base_rev + 1);

    let (rev, ent, ok) = b.update("/test/a", b"c", rev, 0).await.unwrap();
    assert_eq!(rev, base_rev + 2);
    assert!(ok);
    let ent = ent.unwrap();
    assert_eq!(ent.key, "/test/a");
    assert_eq!(ent.value, b"c");
    assert_eq!(ent.lease, 0);
    assert_eq!(ent.mod_revision, base_rev + 2);
    assert_eq!(ent.create_revision, base_rev + 1);

    let (rev, ent, ok) = b.update("/test/a", b"d", rev, 1).await.unwrap();
    assert_eq!(rev, base_rev + 3);
    assert!(ok);
    let ent = ent.unwrap();
    assert_eq!(ent.key, "/test/a");
    assert_eq!(ent.value, b"d");
    assert_eq!(ent.lease, 1);
    assert_eq!(ent.mod_revision, base_rev + 3);
    assert_eq!(ent.create_revision, base_rev + 1);

    let (rev, _, ok) = b.update("/test/a", b"e", 2, 1).await.unwrap();
    assert_eq!(rev, base_rev + 3);
    assert!(!ok);
}

#[tokio::test]
async fn test_backend_delete() {
    let b = skip_if_no_redis!(test_backend().await);

    let base_rev = b.current_revision().await.unwrap();

    let rev = b.create("/test/a", b"b", 1).await.unwrap();
    assert_eq!(rev, base_rev + 1);

    let (rev, ent, ok) = b.delete("/test/a", base_rev + 1).await.unwrap();
    assert!(ok);
    let ent = ent.unwrap();
    assert_eq!(ent.key, "/test/a");
    assert_eq!(ent.value, b"b");
    assert_eq!(ent.lease, 1);
    assert_eq!(ent.mod_revision, base_rev + 1);
    assert_eq!(ent.create_revision, base_rev + 1);
    assert!(rev > base_rev + 1);

    let _rev = b.create("/test/a", b"b", 0).await.unwrap();

    let (_, _, ok) = b.delete("/test/a", base_rev + 1).await.unwrap();
    assert!(!ok);

    let (_, _, ok) = b.delete("/test/a", 0).await.unwrap();
    assert!(ok);
}

#[tokio::test]
async fn test_backend_list() {
    let b = skip_if_no_redis!(test_backend().await);

    let base_rev = b.current_revision().await.unwrap();

    b.create("/test/a/b/c", &[], 0).await.unwrap();
    b.create("/test/a", &[], 0).await.unwrap();
    b.create("/test/b", &[], 0).await.unwrap();
    b.create("/test/a/b", &[], 0).await.unwrap();
    b.create("/test/c", &[], 0).await.unwrap();
    b.create("/test/d/a", &[], 0).await.unwrap();
    b.create("/test/d/b", &[], 0).await.unwrap();

    let (_, ents) = b.list("/test/", "", 0, 0, false).await.unwrap();
    assert_eq!(ents.len(), 7);
    assert_keys_sorted(&ents);

    let (_, ents) = b.list("/test/", "", 0, base_rev + 3, false).await.unwrap();
    assert_eq!(ents.len(), 3);
    assert_keys_sorted(&ents);
    assert_eq_keys(&["/test/a", "/test/a/b/c", "/test/b"], &ents);

    let (_, ents) = b.list("/test/", "", 4, 0, false).await.unwrap();
    assert_eq!(ents.len(), 4);
    assert_keys_sorted(&ents);
}

#[tokio::test]
async fn test_backend_watch() {
    let b = skip_if_no_redis!(test_backend().await);

    let base_rev = b.current_revision().await.unwrap();

    let rev1 = b.create("/test/a", &[], 0).await.unwrap();
    let rev2 = b.create("/test/a/1", &[], 0).await.unwrap();
    let (rev1, _, _) = b.update("/test/a", &[], rev1, 0).await.unwrap();
    let (_, _, _) = b.delete("/test/a", rev1).await.unwrap();
    let (_, _, _) = b.update("/test/a/1", &[], rev2, 0).await.unwrap();

    let wr = b.watch("/", base_rev + 1).await.unwrap();
    let mut events_rx = wr.events;

    let mut all_events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while all_events.len() < 5 && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(1), events_rx.recv()).await {
            Ok(Some(batch)) => all_events.extend(batch),
            _ => break,
        }
    }
    assert_eq!(
        all_events.len(),
        5,
        "should receive 5 events, got {}",
        all_events.len()
    );

    let wr = b.watch("/test/a/", base_rev + 1).await.unwrap();
    let mut events_rx = wr.events;

    let mut prefix_events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while prefix_events.len() < 2 && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(1), events_rx.recv()).await {
            Ok(Some(batch)) => prefix_events.extend(batch),
            _ => break,
        }
    }
    assert_eq!(
        prefix_events.len(),
        2,
        "should receive 2 prefix-filtered events, got {}",
        prefix_events.len()
    );
}

// ---------------------------------------------------------------------------
// Additional tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_and_get() {
    let b = skip_if_no_redis!(test_backend().await);

    let rev = b.create("/test/key1", b"value1", 0).await.unwrap();
    assert!(rev > 0);

    let (_, kv) = b.get("/test/key1", "", 0, 0, false).await.unwrap();
    let kv = kv.expect("key should exist");
    assert_eq!(kv.key, "/test/key1");
    assert_eq!(kv.value, b"value1");
    assert_eq!(kv.create_revision, kv.mod_revision);
}

#[tokio::test]
async fn test_create_after_delete() {
    let b = skip_if_no_redis!(test_backend().await);

    let rev = b.create("/test/recreate", b"v1", 0).await.unwrap();
    b.delete("/test/recreate", rev).await.unwrap();

    let rev2 = b.create("/test/recreate", b"v2", 0).await.unwrap();
    assert!(rev2 > rev);

    let (_, kv) = b.get("/test/recreate", "", 0, 0, false).await.unwrap();
    assert_eq!(kv.unwrap().value, b"v2");
}

#[tokio::test]
async fn test_revision_increases() {
    let b = skip_if_no_redis!(test_backend().await);

    let r1 = b.create("/rev/a", b"a", 0).await.unwrap();
    let r2 = b.create("/rev/b", b"b", 0).await.unwrap();
    let r3 = b.create("/rev/c", b"c", 0).await.unwrap();

    assert!(r2 > r1);
    assert!(r3 > r2);

    let current = b.current_revision().await.unwrap();
    assert!(current >= r3);
}

#[tokio::test]
async fn test_list_returns_latest_version_only() {
    let b = skip_if_no_redis!(test_backend().await);

    let r1 = b.create("/mvcc/k", b"v1", 0).await.unwrap();
    let (r2, _, _) = b.update("/mvcc/k", b"v2", r1, 0).await.unwrap();
    b.update("/mvcc/k", b"v3", r2, 0).await.unwrap();

    let (_, kvs) = b.list("/mvcc/", "", 0, 0, false).await.unwrap();
    assert_eq!(kvs.len(), 1);
    assert_eq!(kvs[0].value, b"v3");
}

#[tokio::test]
async fn test_keys_only() {
    let b = skip_if_no_redis!(test_backend().await);

    b.create("/ko/a", b"some-large-value", 0).await.unwrap();

    let (_, kv) = b.get("/ko/a", "", 0, 0, true).await.unwrap();
    let kv = kv.unwrap();
    assert_eq!(kv.key, "/ko/a");
    assert!(kv.value.is_empty(), "keys_only should return empty value");

    let (_, kvs) = b.list("/ko/", "", 0, 0, true).await.unwrap();
    assert_eq!(kvs.len(), 1);
    assert!(kvs[0].value.is_empty());
}

#[tokio::test]
async fn test_watch_live_events() {
    let b = skip_if_no_redis!(test_backend().await);

    let current_rev = b.current_revision().await.unwrap();

    let watch_result = b.watch("/watch/", current_rev + 1).await.unwrap();
    let mut events_rx = watch_result.events;

    b.create("/watch/key1", b"hello", 0).await.unwrap();

    let timeout = tokio::time::timeout(Duration::from_secs(5), events_rx.recv()).await;
    assert!(timeout.is_ok(), "should receive watch event within timeout");
    let events = timeout.unwrap().expect("channel should not be closed");
    assert!(!events.is_empty());
    assert_eq!(events[0].kv.key, "/watch/key1");
    assert_eq!(events[0].kv.value, b"hello");
}

#[tokio::test]
async fn test_watch_live_create_update_delete() {
    let b = skip_if_no_redis!(test_backend().await);

    let current_rev = b.current_revision().await.unwrap();

    // Start watching BEFORE mutations happen — mirrors Kubernetes apiserver pattern
    let watch_result = b.watch("/wlive/", current_rev + 1).await.unwrap();
    let mut events_rx = watch_result.events;

    // Create
    let rev1 = b.create("/wlive/cm", b"v1", 0).await.unwrap();

    // Update
    let (rev2, _, ok) = b.update("/wlive/cm", b"v2", rev1, 0).await.unwrap();
    assert!(ok);

    // Delete
    let (_, _, ok) = b.delete("/wlive/cm", rev2).await.unwrap();
    assert!(ok);

    // Collect all 3 events (create, update, delete)
    let mut all_events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while all_events.len() < 3 && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(3), events_rx.recv()).await {
            Ok(Some(batch)) => all_events.extend(batch),
            _ => break,
        }
    }
    assert_eq!(
        all_events.len(),
        3,
        "should receive create, update, and delete events; got {}",
        all_events.len()
    );
    assert!(all_events[0].create, "first event should be create");
    assert!(
        !all_events[1].create && !all_events[1].delete,
        "second event should be update"
    );
    assert!(all_events[2].delete, "third event should be delete");
}

#[tokio::test]
async fn test_compact() {
    let b = skip_if_no_redis!(test_backend().await);

    for i in 0..20 {
        let key = format!("/compact/{i}");
        b.create(&key, b"v", 0).await.unwrap();
    }

    let rev = b.current_revision().await.unwrap();
    let result = b.compact(rev).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_db_size() {
    let b = skip_if_no_redis!(test_backend().await);
    let size = b.db_size().await.unwrap();
    assert!(size > 0);
}

#[tokio::test]
async fn test_count_current_revision() {
    let b = skip_if_no_redis!(test_backend().await);

    b.create("/cnt/a", b"1", 0).await.unwrap();
    b.create("/cnt/b", b"2", 0).await.unwrap();
    b.create("/cnt/c", b"3", 0).await.unwrap();

    let (_, count) = b.count("/cnt/", "", 0).await.unwrap();
    assert_eq!(count, 3);

    let (_, kv) = b.get("/cnt/b", "", 0, 0, false).await.unwrap();
    b.delete("/cnt/b", kv.unwrap().mod_revision).await.unwrap();

    let (_, count) = b.count("/cnt/", "", 0).await.unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn test_count_historical_revision() {
    let b = skip_if_no_redis!(test_backend().await);

    let r1 = b.create("/ch/a", b"1", 0).await.unwrap();
    let r2 = b.create("/ch/b", b"2", 0).await.unwrap();
    b.create("/ch/c", b"3", 0).await.unwrap();

    let (_, count) = b.count("/ch/", "", r2).await.unwrap();
    assert_eq!(count, 2);

    let (_, count) = b.count("/ch/", "", r1).await.unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn test_update_nonexistent_key() {
    let b = skip_if_no_redis!(test_backend().await);

    let (_, kv, ok) = b.update("/noexist/k", b"v", 1, 0).await.unwrap();
    assert!(!ok, "update of nonexistent key should fail");
    assert!(kv.is_none());
}

#[tokio::test]
async fn test_delete_already_deleted() {
    let b = skip_if_no_redis!(test_backend().await);

    let rev = b.create("/deldel/k", b"v", 0).await.unwrap();
    let (_, _, ok) = b.delete("/deldel/k", rev).await.unwrap();
    assert!(ok);

    let (_, kv, ok) = b.delete("/deldel/k", 0).await.unwrap();
    assert!(!ok, "deleting already-deleted key should return false");
    assert!(kv.is_none());
}

#[tokio::test]
async fn test_delete_never_existed() {
    let b = skip_if_no_redis!(test_backend().await);

    let (_, kv, ok) = b.delete("/never/existed", 0).await.unwrap();
    assert!(ok, "deleting non-existent key should return true");
    assert!(kv.is_none());
}

#[tokio::test]
async fn test_list_future_revision_error() {
    let b = skip_if_no_redis!(test_backend().await);

    let current = b.current_revision().await.unwrap();
    let result = b.list("/any/", "", 0, current + 100, false).await;
    assert!(
        matches!(result, Err(BackendError::FutureRev)),
        "listing at future revision should return FutureRev error"
    );
}

#[tokio::test]
async fn test_list_with_start_key_equals_prefix() {
    let b = skip_if_no_redis!(test_backend().await);

    b.create("/sk/a", b"1", 0).await.unwrap();
    b.create("/sk/b", b"2", 0).await.unwrap();
    b.create("/sk/c", b"3", 0).await.unwrap();

    let (_, kvs) = b.list("/sk/", "/sk/", 0, 0, false).await.unwrap();
    assert_eq!(kvs.len(), 3, "startKey == prefix should list all keys");

    let (_, kvs) = b.list("/sk/", "/sk/b", 0, 0, false).await.unwrap();
    assert_eq!(kvs.len(), 2, "startKey /sk/b should skip /sk/a");
    assert_eq!(kvs[0].key, "/sk/b");
    assert_eq!(kvs[1].key, "/sk/c");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn assert_keys_sorted(kvs: &[rhino::KeyValue]) {
    for window in kvs.windows(2) {
        assert!(
            window[0].key <= window[1].key,
            "keys not sorted: {:?} > {:?}",
            window[0].key,
            window[1].key
        );
    }
}

fn assert_eq_keys(expected: &[&str], kvs: &[rhino::KeyValue]) {
    let got: Vec<&str> = kvs.iter().map(|kv| kv.key.as_str()).collect();
    assert_eq!(expected, &got[..], "key mismatch");
}
