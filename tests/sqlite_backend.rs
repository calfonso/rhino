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
// Compaction safety tests — verify the prev_revision=0 fix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_compact_does_not_delete_live_keys() {
    // Regression test: compaction must never delete the latest (only) revision
    // of a key. This was broken when create() set prev_revision to the current
    // max revision instead of 0, causing cross-key prev_revision references.
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("compact_live.db");
    let dsn = db_path.to_string_lossy().to_string();

    let config = SqliteConfig {
        dsn: dsn.clone(),
        compact_interval: Duration::ZERO,
        compact_min_retain: 5,
        compact_batch_size: 1000,
        ..Default::default()
    };
    let backend = SqliteBackend::new(config).await.unwrap();
    backend.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create several keys (like Kubernetes bootstrap).
    backend.create("/registry/ns/default", b"ns1", 0).await.unwrap();
    backend.create("/registry/ns/kube-system", b"ns2", 0).await.unwrap();
    backend.create("/registry/ns/kube-public", b"ns3", 0).await.unwrap();
    backend.create("/registry/sa/default", b"sa1", 0).await.unwrap();
    backend.create("/registry/sa/kube-system", b"sa2", 0).await.unwrap();

    // Generate enough revisions to push past compact_min_retain.
    let r = backend.create("/registry/padding/key", b"v0", 0).await.unwrap();
    let mut rev = r;
    for i in 1..=20 {
        let val = format!("v{i}");
        let (new_rev, _, ok) = backend
            .update("/registry/padding/key", val.as_bytes(), rev, 0)
            .await
            .unwrap();
        assert!(ok);
        rev = new_rev;
    }

    // Run compaction.
    let current = backend.current_revision().await.unwrap();
    backend.compact(current).await.unwrap();

    // ALL bootstrap keys must still be readable.
    for (key, expected) in [
        ("/registry/ns/default", b"ns1".as_slice()),
        ("/registry/ns/kube-system", b"ns2".as_slice()),
        ("/registry/ns/kube-public", b"ns3".as_slice()),
        ("/registry/sa/default", b"sa1".as_slice()),
        ("/registry/sa/kube-system", b"sa2".as_slice()),
    ] {
        let (_, kv) = backend.get(key, "", 0, 0, false).await.unwrap();
        let kv = kv.unwrap_or_else(|| panic!("key {key} should exist after compaction"));
        assert_eq!(kv.value, expected, "wrong value for {key}");
    }

    // List must return all namespace keys.
    let (_, kvs) = backend.list("/registry/ns/", "", 0, 0, false).await.unwrap();
    assert_eq!(kvs.len(), 3, "all 3 namespace keys should survive compaction");

    // The padding key should have latest value.
    let (_, kv) = backend.get("/registry/padding/key", "", 0, 0, false).await.unwrap();
    assert_eq!(kv.unwrap().value, b"v20");
}

#[tokio::test]
async fn test_compact_removes_deleted_keys_from_kine_current() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("compact_current.db");
    let dsn = db_path.to_string_lossy().to_string();

    let config = SqliteConfig {
        dsn: dsn.clone(),
        compact_interval: Duration::ZERO,
        compact_min_retain: 2,
        compact_batch_size: 1000,
        ..Default::default()
    };
    let backend = SqliteBackend::new(config).await.unwrap();
    backend.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create and delete a key.
    let rev = backend.create("/del/key", b"v1", 0).await.unwrap();
    backend.delete("/del/key", rev).await.unwrap();

    // Generate padding revisions.
    for i in 0..10 {
        let key = format!("/pad/{i}");
        backend.create(&key, b"x", 0).await.unwrap();
    }

    // Compact.
    let current = backend.current_revision().await.unwrap();
    backend.compact(current).await.unwrap();

    // The deleted key should not appear in list results.
    let (_, kvs) = backend.list("/del/", "", 0, 0, false).await.unwrap();
    assert_eq!(kvs.len(), 0, "deleted key should not appear in list");

    // Verify kine_current was cleaned up.
    let pool = sqlx::sqlite::SqlitePool::connect(&format!("sqlite:{dsn}"))
        .await
        .unwrap();
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM kine_current WHERE name = '/del/key'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.0, 0, "kine_current should not have entry for deleted+compacted key");
    pool.close().await;
}

// ---------------------------------------------------------------------------
// kine_current consistency tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_kine_current_stays_consistent() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("kine_current_consistency.db");
    let dsn = db_path.to_string_lossy().to_string();

    let config = SqliteConfig {
        dsn,
        compact_interval: Duration::ZERO,
        ..Default::default()
    };
    let backend = SqliteBackend::new(config).await.unwrap();
    backend.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create, update, delete, re-create cycle.
    let r1 = backend.create("/kc/k", b"v1", 0).await.unwrap();
    let (r2, _, _) = backend.update("/kc/k", b"v2", r1, 0).await.unwrap();
    backend.delete("/kc/k", r2).await.unwrap();
    let r4 = backend.create("/kc/k", b"v3", 0).await.unwrap();

    // List at current should return v3.
    let (_, kvs) = backend.list("/kc/", "", 0, 0, false).await.unwrap();
    assert_eq!(kvs.len(), 1);
    assert_eq!(kvs[0].value, b"v3");
    assert_eq!(kvs[0].mod_revision, r4);

    // Get should also return v3.
    let (_, kv) = backend.get("/kc/k", "", 0, 0, false).await.unwrap();
    let kv = kv.unwrap();
    assert_eq!(kv.value, b"v3");

    // Count should be 1.
    let (_, count) = backend.count("/kc/", "", 0).await.unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn test_list_returns_results_immediately_after_create() {
    // Verify kine_current is updated synchronously: a list immediately
    // after create must return the key.
    let (backend, _dir) = test_backend().await;

    for i in 0..10 {
        let key = format!("/immediate/{i}");
        backend.create(&key, b"val", 0).await.unwrap();

        // List immediately — should see all keys created so far.
        let (_, kvs) = backend.list("/immediate/", "", 0, 0, false).await.unwrap();
        assert_eq!(
            kvs.len(),
            i + 1,
            "list after creating key {i} should return {} keys, got {}",
            i + 1,
            kvs.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Count tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_count_current_revision() {
    let (backend, _dir) = test_backend().await;

    backend.create("/cnt/a", b"1", 0).await.unwrap();
    backend.create("/cnt/b", b"2", 0).await.unwrap();
    backend.create("/cnt/c", b"3", 0).await.unwrap();

    let (_, count) = backend.count("/cnt/", "", 0).await.unwrap();
    assert_eq!(count, 3);

    // Delete one.
    let (_, kv) = backend.get("/cnt/b", "", 0, 0, false).await.unwrap();
    backend.delete("/cnt/b", kv.unwrap().mod_revision).await.unwrap();

    let (_, count) = backend.count("/cnt/", "", 0).await.unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn test_count_historical_revision() {
    let (backend, _dir) = test_backend().await;

    let r1 = backend.create("/ch/a", b"1", 0).await.unwrap();
    let r2 = backend.create("/ch/b", b"2", 0).await.unwrap();
    backend.create("/ch/c", b"3", 0).await.unwrap();

    // Count at revision after first 2 creates.
    let (_, count) = backend.count("/ch/", "", r2).await.unwrap();
    assert_eq!(count, 2);

    // Count at revision after first create.
    let (_, count) = backend.count("/ch/", "", r1).await.unwrap();
    assert_eq!(count, 1);
}

// ---------------------------------------------------------------------------
// Edge case tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_update_nonexistent_key() {
    let (backend, _dir) = test_backend().await;

    let (_, kv, ok) = backend.update("/noexist/k", b"v", 1, 0).await.unwrap();
    assert!(!ok, "update of nonexistent key should fail");
    assert!(kv.is_none());
}

#[tokio::test]
async fn test_delete_already_deleted() {
    let (backend, _dir) = test_backend().await;

    let rev = backend.create("/deldel/k", b"v", 0).await.unwrap();
    let (_, _, ok) = backend.delete("/deldel/k", rev).await.unwrap();
    assert!(ok);

    // Delete again — should return false (already deleted).
    let (_, kv, ok) = backend.delete("/deldel/k", 0).await.unwrap();
    assert!(!ok, "deleting already-deleted key should return false");
    assert!(kv.is_none());
}

#[tokio::test]
async fn test_delete_never_existed() {
    let (backend, _dir) = test_backend().await;

    let (_, kv, ok) = backend.delete("/never/existed", 0).await.unwrap();
    assert!(ok, "deleting non-existent key should return true");
    assert!(kv.is_none());
}

#[tokio::test]
async fn test_list_future_revision_error() {
    let (backend, _dir) = test_backend().await;

    let current = backend.current_revision().await.unwrap();
    let result = backend.list("/any/", "", 0, current + 100, false).await;
    assert!(
        matches!(result, Err(BackendError::FutureRev)),
        "listing at future revision should return FutureRev error"
    );
}

#[tokio::test]
async fn test_watch_compacted_revision_error() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("watch_compact.db");

    let config = SqliteConfig {
        dsn: db_path.to_string_lossy().to_string(),
        compact_interval: Duration::ZERO,
        compact_min_retain: 2,
        compact_batch_size: 1000,
        ..Default::default()
    };
    let backend = SqliteBackend::new(config).await.unwrap();
    backend.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create enough data to compact.
    let r = backend.create("/wc/key", b"v0", 0).await.unwrap();
    let mut rev = r;
    for i in 1..=10 {
        let (new_rev, _, _) = backend
            .update("/wc/key", format!("v{i}").as_bytes(), rev, 0)
            .await
            .unwrap();
        rev = new_rev;
    }

    // Compact.
    let current = backend.current_revision().await.unwrap();
    backend.compact(current).await.unwrap();

    // Watch at a compacted revision should return error.
    let result = backend.watch("/wc/", 1).await;
    assert!(
        matches!(result, Err(BackendError::Compacted)),
        "watching at compacted revision should return Compacted error"
    );
}

#[tokio::test]
async fn test_create_after_delete_preserves_old_value() {
    // Verify C2 fix: old_value stores the deleted key's value on re-create.
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("old_value.db");
    let dsn = db_path.to_string_lossy().to_string();

    let config = SqliteConfig {
        dsn: dsn.clone(),
        compact_interval: Duration::ZERO,
        ..Default::default()
    };
    let backend = SqliteBackend::new(config).await.unwrap();
    backend.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create, delete, re-create.
    let rev = backend.create("/ov/k", b"original", 0).await.unwrap();
    backend.delete("/ov/k", rev).await.unwrap();
    let new_rev = backend.create("/ov/k", b"new_value", 0).await.unwrap();

    // Check the raw kine row for old_value.
    let pool = sqlx::sqlite::SqlitePool::connect(&format!("sqlite:{dsn}"))
        .await
        .unwrap();
    let row: (Vec<u8>,) = sqlx::query_as("SELECT old_value FROM kine WHERE id = ?")
        .bind(new_rev)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.0, b"original",
        "old_value should contain the deleted key's value"
    );
    pool.close().await;
}

#[tokio::test]
async fn test_prev_revision_zero_for_new_keys() {
    // Verify the root-cause fix: new keys get prev_revision=0, not current rev.
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("prev_rev.db");
    let dsn = db_path.to_string_lossy().to_string();

    let config = SqliteConfig {
        dsn: dsn.clone(),
        compact_interval: Duration::ZERO,
        ..Default::default()
    };
    let backend = SqliteBackend::new(config).await.unwrap();
    backend.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let rev = backend.create("/prev/new_key", b"val", 0).await.unwrap();

    let pool = sqlx::sqlite::SqlitePool::connect(&format!("sqlite:{dsn}"))
        .await
        .unwrap();
    let row: (i64,) = sqlx::query_as("SELECT prev_revision FROM kine WHERE id = ?")
        .bind(rev)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.0, 0,
        "prev_revision for a brand-new key must be 0"
    );
    pool.close().await;
}

#[tokio::test]
async fn test_list_with_start_key_equals_prefix() {
    // Verify D78: when startKey == prefix, startKey is cleared.
    let (backend, _dir) = test_backend().await;

    backend.create("/sk/a", b"1", 0).await.unwrap();
    backend.create("/sk/b", b"2", 0).await.unwrap();
    backend.create("/sk/c", b"3", 0).await.unwrap();

    // List with startKey == prefix — should return all keys.
    let (_, kvs) = backend.list("/sk/", "/sk/", 0, 0, false).await.unwrap();
    assert_eq!(kvs.len(), 3, "startKey == prefix should list all keys");

    // List with startKey after some keys.
    let (_, kvs) = backend.list("/sk/", "/sk/b", 0, 0, false).await.unwrap();
    assert_eq!(kvs.len(), 2, "startKey /sk/b should skip /sk/a");
    assert_eq!(kvs[0].key, "/sk/b");
    assert_eq!(kvs[1].key, "/sk/c");
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
