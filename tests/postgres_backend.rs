//! PostgreSQL backend integration tests.
//!
//! These tests require a running PostgreSQL instance. Set the `RHINO_POSTGRES_DSN`
//! environment variable to enable them:
//!
//!   RHINO_POSTGRES_DSN="postgres://postgres:postgres@localhost/rhino_test" cargo test
//!
//! The test database will be created automatically if it doesn't exist. Each test
//! truncates the kine table to ensure isolation.

use rhino::{Backend, BackendError, PostgresBackend, PostgresConfig};
use std::time::Duration;

/// Return the DSN from the environment, or skip the test.
fn get_dsn() -> Option<String> {
    std::env::var("RHINO_POSTGRES_DSN").ok()
}

/// Create a backend for testing, truncating the table to ensure a clean state.
async fn test_backend() -> Option<PostgresBackend> {
    let dsn = get_dsn()?;

    let config = PostgresConfig {
        dsn: dsn.clone(),
        compact_interval: Duration::ZERO,
        compact_min_retain: 5,
        compact_batch_size: 100,
        ..Default::default()
    };

    let backend = PostgresBackend::new(config).await.unwrap();

    // Set up schema first so the table exists
    backend.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Truncate for test isolation and re-start
    let pool = sqlx::postgres::PgPool::connect(&dsn).await.unwrap();
    sqlx::query("TRUNCATE kine RESTART IDENTITY")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    // Reconnect with a fresh backend after truncate
    let config2 = PostgresConfig {
        dsn,
        compact_interval: Duration::ZERO,
        compact_min_retain: 5,
        compact_batch_size: 100,
        ..Default::default()
    };
    let backend = PostgresBackend::new(config2).await.unwrap();
    backend.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    Some(backend)
}

// ---------------------------------------------------------------------------
// Tests ported from kine: pkg/drivers/nats/backend_test.go
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_backend_create() {
    let Some(b) = test_backend().await else {
        return;
    };

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
    let Some(b) = test_backend().await else {
        return;
    };

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
    let Some(b) = test_backend().await else {
        return;
    };

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
    let Some(b) = test_backend().await else {
        return;
    };

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
    let Some(b) = test_backend().await else {
        return;
    };

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

    let (_, ents) = b
        .list("/test/", "", 0, base_rev + 3, false)
        .await
        .unwrap();
    assert_eq!(ents.len(), 3);
    assert_keys_sorted(&ents);
    assert_eq_keys(&["/test/a", "/test/a/b/c", "/test/b"], &ents);

    let (_, ents) = b.list("/test/", "", 4, 0, false).await.unwrap();
    assert_eq!(ents.len(), 4);
    assert_keys_sorted(&ents);
}

#[tokio::test]
async fn test_backend_watch() {
    let Some(b) = test_backend().await else {
        return;
    };

    let base_rev = b.current_revision().await.unwrap();

    let rev1 = b.create("/test/a", &[], 0).await.unwrap();
    let rev2 = b.create("/test/a/1", &[], 0).await.unwrap();
    let (rev1, _, _) = b.update("/test/a", &[], rev1, 0).await.unwrap();
    let (_, _, _) = b.delete("/test/a", rev1).await.unwrap();
    let (_, _, _) = b.update("/test/a/1", &[], rev2, 0).await.unwrap();

    let wr = b.watch("/", base_rev + 1).await.unwrap();
    let mut events_rx = wr.events;

    let mut all_events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while all_events.len() < 5 && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), events_rx.recv()).await {
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while prefix_events.len() < 2 && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), events_rx.recv()).await {
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
async fn test_create_after_delete() {
    let Some(b) = test_backend().await else {
        return;
    };

    let rev = b.create("/test/recreate", b"v1", 0).await.unwrap();
    b.delete("/test/recreate", rev).await.unwrap();

    let rev2 = b.create("/test/recreate", b"v2", 0).await.unwrap();
    assert!(rev2 > rev);

    let (_, kv) = b.get("/test/recreate", "", 0, 0, false).await.unwrap();
    assert_eq!(kv.unwrap().value, b"v2");
}

#[tokio::test]
async fn test_revision_increases() {
    let Some(b) = test_backend().await else {
        return;
    };

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
    let Some(b) = test_backend().await else {
        return;
    };

    let r1 = b.create("/mvcc/k", b"v1", 0).await.unwrap();
    let (r2, _, _) = b.update("/mvcc/k", b"v2", r1, 0).await.unwrap();
    b.update("/mvcc/k", b"v3", r2, 0).await.unwrap();

    let (_, kvs) = b.list("/mvcc/", "", 0, 0, false).await.unwrap();
    assert_eq!(kvs.len(), 1);
    assert_eq!(kvs[0].value, b"v3");
}

#[tokio::test]
async fn test_keys_only() {
    let Some(b) = test_backend().await else {
        return;
    };

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
async fn test_compact_removes_old_rows() {
    let Some(dsn) = get_dsn() else {
        return;
    };

    let config = PostgresConfig {
        dsn: dsn.clone(),
        compact_interval: Duration::ZERO,
        compact_min_retain: 5,
        compact_batch_size: 100,
        ..Default::default()
    };
    let backend = PostgresBackend::new(config).await.unwrap();
    backend.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Truncate for isolation
    let pool = sqlx::postgres::PgPool::connect(&dsn).await.unwrap();
    sqlx::query("TRUNCATE kine RESTART IDENTITY")
        .execute(&pool)
        .await
        .unwrap();

    // Re-initialize after truncate
    let config2 = PostgresConfig {
        dsn: dsn.clone(),
        compact_interval: Duration::ZERO,
        compact_min_retain: 5,
        compact_batch_size: 100,
        ..Default::default()
    };
    let backend = PostgresBackend::new(config2).await.unwrap();
    backend.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

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

    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM kine")
        .fetch_one(&pool)
        .await
        .unwrap();

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

    let (_, kv) = backend.get("/comp/k", "", 0, 0, false).await.unwrap();
    assert_eq!(kv.unwrap().value, b"v20");

    pool.close().await;
}

#[tokio::test]
async fn test_db_size() {
    let Some(b) = test_backend().await else {
        return;
    };
    let size = b.db_size().await.unwrap();
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
    assert_eq!(expected, &got[..], "key mismatch");
}
