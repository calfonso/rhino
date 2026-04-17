use rhino::{Backend, BackendError, SqliteBackend, SqliteConfig};
use std::time::Duration;
use tempfile::TempDir;

async fn test_backend() -> (SqliteBackend, TempDir) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");

    let config = SqliteConfig {
        dsn: db_path.to_string_lossy().to_string(),
        compact_interval: Duration::ZERO, // disable auto-compaction in tests
        ..Default::default()
    };

    let backend = SqliteBackend::new(config).await.unwrap();
    backend.start().await.unwrap();

    // Give poll loop a moment to start
    tokio::time::sleep(Duration::from_millis(50)).await;

    (backend, dir)
}

// ---------------------------------------------------------------------------
// Tests ported from kine: pkg/drivers/nats/backend_test.go
//
// Adapted for SQLite (revision numbering differs from NATS — SQLite uses
// autoincrement row IDs and the base revision includes internal rows like
// compact_rev_key and /registry/health from start()).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_backend_create() {
    let (b, _dir) = test_backend().await;

    let base_rev = b.current_revision().await.unwrap();

    // Create a key.
    let rev = b.create("/test/a", &[], 0).await.unwrap();
    assert_eq!(rev, base_rev + 1);

    // Attempt to create again — should fail.
    let err = b.create("/test/a", &[], 0).await;
    assert!(matches!(err, Err(BackendError::KeyExists)));

    let rev = b.create("/test/a/b", &[], 0).await.unwrap();
    assert_eq!(rev, base_rev + 2);

    let rev = b.create("/test/a/b/c", &[], 0).await.unwrap();
    assert_eq!(rev, base_rev + 3);

    // Create with lease.
    let rev = b.create("/test/b", &[], 1).await.unwrap();
    assert_eq!(rev, base_rev + 4);

    let (srev, count) = b.count("/test/", "", 0).await.unwrap();
    assert_eq!(srev, base_rev + 4);
    assert_eq!(count, 4);

    // Create another key and verify count updates.
    b.create("/test/c", &[], 0).await.unwrap();
    let (_, count) = b.count("/test/", "", 0).await.unwrap();
    assert_eq!(count, 5);
}

#[tokio::test]
async fn test_backend_get() {
    let (b, _dir) = test_backend().await;

    let base_rev = b.current_revision().await.unwrap();

    // Create with lease.
    let _rev = b.create("/test/a", b"b", 1).await.unwrap();

    let (srev, ent) = b.get("/test/a", "", 0, 0, false).await.unwrap();
    let ent = ent.expect("key should exist");
    assert_eq!(srev, base_rev + 1);
    assert_eq!(ent.key, "/test/a");
    assert_eq!(ent.value, b"b");
    assert_eq!(ent.lease, 1);
    assert_eq!(ent.mod_revision, base_rev + 1);
    assert_eq!(ent.create_revision, base_rev + 1);

    // Create it again after deleting.
    let (_, _, _) = b.delete("/test/a", ent.mod_revision).await.unwrap();

    let rev = b.create("/test/a", b"c", 0).await.unwrap();

    let (_, _, _) = b.update("/test/a", b"d", rev, 0).await.unwrap();

    // Get at current (latest) revision.
    let (rev, ent) = b.get("/test/a", "", 0, 0, false).await.unwrap();
    let ent = ent.expect("key should exist");
    assert!(rev > base_rev);
    assert_eq!(ent.value, b"d");

    // Get nonexistent key returns None.
    let (_, ent) = b.get("/test/doesnotexist", "", 0, 0, false).await.unwrap();
    assert!(ent.is_none());
}

#[tokio::test]
async fn test_backend_update() {
    let (b, _dir) = test_backend().await;

    let base_rev = b.current_revision().await.unwrap();

    // Create with lease.
    let rev = b.create("/test/a", b"b", 1).await.unwrap();
    assert_eq!(rev, base_rev + 1);

    // Update, changing value and removing lease.
    let (rev, ent, ok) = b.update("/test/a", b"c", rev, 0).await.unwrap();
    assert_eq!(rev, base_rev + 2);
    assert!(ok);
    let ent = ent.unwrap();
    assert_eq!(ent.key, "/test/a");
    assert_eq!(ent.value, b"c");
    assert_eq!(ent.lease, 0);
    assert_eq!(ent.mod_revision, base_rev + 2);
    assert_eq!(ent.create_revision, base_rev + 1);

    // Update again, setting lease.
    let (rev, ent, ok) = b.update("/test/a", b"d", rev, 1).await.unwrap();
    assert_eq!(rev, base_rev + 3);
    assert!(ok);
    let ent = ent.unwrap();
    assert_eq!(ent.key, "/test/a");
    assert_eq!(ent.value, b"d");
    assert_eq!(ent.lease, 1);
    assert_eq!(ent.mod_revision, base_rev + 3);
    assert_eq!(ent.create_revision, base_rev + 1);

    // Update with wrong revision — should fail.
    let (rev, _, ok) = b.update("/test/a", b"e", 2, 1).await.unwrap();
    assert_eq!(rev, base_rev + 3);
    assert!(!ok);
}

#[tokio::test]
async fn test_backend_delete() {
    let (b, _dir) = test_backend().await;

    let base_rev = b.current_revision().await.unwrap();

    // Create with lease.
    let rev = b.create("/test/a", b"b", 1).await.unwrap();
    assert_eq!(rev, base_rev + 1);

    // Delete with correct revision.
    let (rev, ent, ok) = b.delete("/test/a", base_rev + 1).await.unwrap();
    assert!(ok);
    let ent = ent.unwrap();
    assert_eq!(ent.key, "/test/a");
    assert_eq!(ent.value, b"b");
    assert_eq!(ent.lease, 1);
    assert_eq!(ent.mod_revision, base_rev + 1);
    assert_eq!(ent.create_revision, base_rev + 1);
    assert!(rev > base_rev + 1);

    // Create again.
    let _rev = b.create("/test/a", b"b", 0).await.unwrap();

    // Fail to delete since the revision doesn't match.
    let (_, _, ok) = b.delete("/test/a", base_rev + 1).await.unwrap();
    assert!(!ok);

    // No revision (0) will delete the latest.
    let (_, _, ok) = b.delete("/test/a", 0).await.unwrap();
    assert!(ok);
}

#[tokio::test]
async fn test_backend_list() {
    let (b, _dir) = test_backend().await;

    let base_rev = b.current_revision().await.unwrap();

    // Create keys (intentionally out of alphabetical order).
    b.create("/test/a/b/c", &[], 0).await.unwrap();
    b.create("/test/a", &[], 0).await.unwrap();
    b.create("/test/b", &[], 0).await.unwrap();
    b.create("/test/a/b", &[], 0).await.unwrap();
    b.create("/test/c", &[], 0).await.unwrap();
    b.create("/test/d/a", &[], 0).await.unwrap();
    b.create("/test/d/b", &[], 0).await.unwrap();

    // List all keys under /test/.
    let (_, ents) = b.list("/test/", "", 0, 0, false).await.unwrap();
    assert_eq!(ents.len(), 7);
    assert_keys_sorted(&ents);

    // List at a historical revision (first 3 creates).
    let (_, ents) = b.list("/test/", "", 0, base_rev + 3, false).await.unwrap();
    assert_eq!(ents.len(), 3);
    assert_keys_sorted(&ents);
    assert_eq_keys(
        &["/test/a", "/test/a/b/c", "/test/b"],
        &ents,
    );

    // List with a limit.
    let (_, ents) = b.list("/test/", "", 4, 0, false).await.unwrap();
    assert_eq!(ents.len(), 4);
    assert_keys_sorted(&ents);
}

#[tokio::test]
async fn test_backend_watch() {
    let (b, _dir) = test_backend().await;

    let base_rev = b.current_revision().await.unwrap();

    // Perform operations, then watch historically.
    let rev1 = b.create("/test/a", &[], 0).await.unwrap();
    let rev2 = b.create("/test/a/1", &[], 0).await.unwrap();
    let (rev1, _, _) = b.update("/test/a", &[], rev1, 0).await.unwrap();
    let (_, _, _) = b.delete("/test/a", rev1).await.unwrap();
    let (_, _, _) = b.update("/test/a/1", &[], rev2, 0).await.unwrap();

    // Watch from base_rev+1 — should see all 5 events.
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
    assert_eq!(all_events.len(), 5, "should receive 5 events, got {}", all_events.len());

    // Watch with prefix — only /test/a/1 events (create + update = 2).
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
    assert_eq!(prefix_events.len(), 2, "should receive 2 prefix-filtered events, got {}", prefix_events.len());
}

// ---------------------------------------------------------------------------
// Additional rhino-specific tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_and_get() {
    let (backend, _dir) = test_backend().await;

    let rev = backend.create("/test/key1", b"value1", 0).await.unwrap();
    assert!(rev > 0);

    let (_, kv) = backend.get("/test/key1", "", 0, 0, false).await.unwrap();
    let kv = kv.expect("key should exist");
    assert_eq!(kv.key, "/test/key1");
    assert_eq!(kv.value, b"value1");
    assert_eq!(kv.create_revision, kv.mod_revision);
}

#[tokio::test]
async fn test_create_after_delete() {
    let (backend, _dir) = test_backend().await;

    let rev = backend.create("/test/recreate", b"v1", 0).await.unwrap();
    backend.delete("/test/recreate", rev).await.unwrap();

    // Should be able to create again after delete.
    let rev2 = backend
        .create("/test/recreate", b"v2", 0)
        .await
        .unwrap();
    assert!(rev2 > rev);

    let (_, kv) = backend
        .get("/test/recreate", "", 0, 0, false)
        .await
        .unwrap();
    assert_eq!(kv.unwrap().value, b"v2");
}

#[tokio::test]
async fn test_revision_increases() {
    let (backend, _dir) = test_backend().await;

    let r1 = backend.create("/rev/a", b"a", 0).await.unwrap();
    let r2 = backend.create("/rev/b", b"b", 0).await.unwrap();
    let r3 = backend.create("/rev/c", b"c", 0).await.unwrap();

    assert!(r2 > r1);
    assert!(r3 > r2);

    let current = backend.current_revision().await.unwrap();
    assert!(current >= r3);
}

#[tokio::test]
async fn test_list_returns_latest_version_only() {
    let (backend, _dir) = test_backend().await;

    let r1 = backend.create("/mvcc/k", b"v1", 0).await.unwrap();
    let (r2, _, _) = backend.update("/mvcc/k", b"v2", r1, 0).await.unwrap();
    backend.update("/mvcc/k", b"v3", r2, 0).await.unwrap();

    // List should return exactly one entry — the latest version.
    let (_, kvs) = backend.list("/mvcc/", "", 0, 0, false).await.unwrap();
    assert_eq!(kvs.len(), 1);
    assert_eq!(kvs[0].value, b"v3");
}

#[tokio::test]
async fn test_keys_only() {
    let (backend, _dir) = test_backend().await;

    backend
        .create("/ko/a", b"some-large-value", 0)
        .await
        .unwrap();

    let (_, kv) = backend.get("/ko/a", "", 0, 0, true).await.unwrap();
    let kv = kv.unwrap();
    assert_eq!(kv.key, "/ko/a");
    assert!(kv.value.is_empty(), "keys_only should return empty value");

    let (_, kvs) = backend.list("/ko/", "", 0, 0, true).await.unwrap();
    assert_eq!(kvs.len(), 1);
    assert!(kvs[0].value.is_empty());
}

#[tokio::test]
async fn test_watch_live_events() {
    let (backend, _dir) = test_backend().await;

    let current_rev = backend.current_revision().await.unwrap();

    let watch_result = backend.watch("/watch/", current_rev + 1).await.unwrap();
    let mut events_rx = watch_result.events;

    // Create a key that matches the watch prefix.
    backend.create("/watch/key1", b"hello", 0).await.unwrap();

    let timeout = tokio::time::timeout(Duration::from_secs(5), events_rx.recv()).await;
    assert!(timeout.is_ok(), "should receive watch event within timeout");
    let events = timeout.unwrap().expect("channel should not be closed");
    assert!(!events.is_empty());
    assert_eq!(events[0].kv.key, "/watch/key1");
    assert_eq!(events[0].kv.value, b"hello");
}

#[tokio::test]
async fn test_watch_sees_updates_and_deletes() {
    let (backend, _dir) = test_backend().await;

    let rev = backend.create("/wd/k", b"v1", 0).await.unwrap();
    let current_rev = backend.current_revision().await.unwrap();

    let watch_result = backend.watch("/wd/", current_rev + 1).await.unwrap();
    let mut rx = watch_result.events;

    // Update the key.
    let (rev2, _, _) = backend.update("/wd/k", b"v2", rev, 0).await.unwrap();

    // Delete the key.
    backend.delete("/wd/k", rev2).await.unwrap();

    // Collect all events — the poll loop may batch them in one or multiple messages.
    let mut all_events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while all_events.len() < 2 && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(1500), rx.recv()).await {
            Ok(Some(batch)) => all_events.extend(batch),
            _ => break,
        }
    }

    assert!(all_events.len() >= 2, "expected at least 2 events, got {}", all_events.len());

    // Verify we got both an update event and a delete event (order depends on poll batching).
    let has_update = all_events.iter().any(|e| !e.delete && e.kv.value == b"v2");
    let has_delete = all_events.iter().any(|e| e.delete);
    assert!(has_update, "should have an update event with value v2");
    assert!(has_delete, "should have a delete event");
}

#[tokio::test]
async fn test_compact() {
    let (backend, _dir) = test_backend().await;

    for i in 0..20 {
        let key = format!("/compact/{i}");
        backend.create(&key, b"v", 0).await.unwrap();
    }

    let rev = backend.current_revision().await.unwrap();
    let result = backend.compact(rev).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_compact_removes_old_rows() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("compact_test.db");
    let dsn = db_path.to_string_lossy().to_string();

    let config = SqliteConfig {
        dsn: dsn.clone(),
        compact_interval: Duration::ZERO,
        compact_min_retain: 5,
        compact_batch_size: 100,
        ..Default::default()
    };
    let backend = SqliteBackend::new(config).await.unwrap();
    backend.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create a key and update it many times to produce old revisions.
    let r = backend.create("/comp/k", b"v0", 0).await.unwrap();
    let mut rev = r;
    for i in 1..=20 {
        let val = format!("v{i}");
        let (new_rev, _, ok) = backend
            .update("/comp/k", val.as_bytes(), rev, 0)
            .await
            .unwrap();
        assert!(ok);
        rev = new_rev;
    }

    // Count rows before compaction.
    let pool = sqlx::sqlite::SqlitePool::connect(&format!("sqlite:{dsn}"))
        .await
        .unwrap();
    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM kine")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Compact up to current revision.
    let current = backend.current_revision().await.unwrap();
    backend.compact(current).await.unwrap();

    let after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM kine")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert!(
        after.0 < before.0,
        "compaction should remove rows: before={}, after={}",
        before.0,
        after.0
    );

    // The key should still be readable.
    let (_, kv) = backend.get("/comp/k", "", 0, 0, false).await.unwrap();
    let kv = kv.unwrap();
    assert_eq!(kv.value, b"v20");

    pool.close().await;
}

#[tokio::test]
async fn test_db_size() {
    let (backend, _dir) = test_backend().await;
    let size = backend.db_size().await.unwrap();
    assert!(size > 0);
}

// ---------------------------------------------------------------------------
// Helpers (ported from kine helper_test.go)
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
    assert_eq!(
        expected, &got[..],
        "key mismatch"
    );
}
