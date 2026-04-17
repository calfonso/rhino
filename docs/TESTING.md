# Testing

This document covers everything a contributor needs to run and write tests for rhino.

## Prerequisites

- **Rust** 1.80+ (stable)
- **protoc** (protobuf compiler) — required by `tonic-build` at compile time
- **PostgreSQL** 14+ — only needed for Postgres backend tests
- **MySQL** 8.0+ or **MariaDB** 10.6+ — only needed for MySQL backend tests

### Installing protoc

```sh
# macOS
brew install protobuf

# Debian/Ubuntu
apt-get install -y protobuf-compiler

# Fedora
dnf install -y protobuf-compiler
```

## Running Tests

### Quick check (SQLite only)

```sh
cargo test
```

This runs all 16 SQLite backend tests using temporary databases. No external services needed.

### Full suite (all backends)

```sh
# Start Postgres and MySQL (example using Docker)
docker run -d --name rhino-pg \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_DB=rhino_test \
  -p 5432:5432 \
  postgres:16

docker run -d --name rhino-mysql \
  -e MYSQL_ROOT_PASSWORD=root \
  -e MYSQL_DATABASE=rhino_test \
  -p 3306:3306 \
  mysql:8

# Run all tests
RHINO_POSTGRES_DSN="postgres://postgres:postgres@localhost/rhino_test" \
RHINO_MYSQL_DSN="mysql://root:root@localhost/rhino_test" \
cargo test
```

### Targeting a specific test file

```sh
# SQLite tests only
cargo test --test sqlite_backend

# Postgres tests only
RHINO_POSTGRES_DSN="postgres://postgres:postgres@localhost/rhino_test" \
  cargo test --test postgres_backend

# MySQL tests only
RHINO_MYSQL_DSN="mysql://root:root@localhost/rhino_test" \
  cargo test --test mysql_backend

# Single test by name
cargo test test_backend_create
```

### Linting

```sh
cargo clippy --all-targets
```

The CI standard is zero warnings. All clippy lints must pass.

## Test Architecture

### SQLite Tests (`tests/sqlite_backend.rs`)

Each test creates its own temporary SQLite database via `tempfile::TempDir`, so tests run in parallel without interference. The directory is cleaned up when the test ends.

Auto-compaction is disabled (`compact_interval: Duration::ZERO`) in tests so that background compaction doesn't interfere with assertions about row counts and revisions.

### PostgreSQL Tests (`tests/postgres_backend.rs`)

All tests are gated behind the `RHINO_POSTGRES_DSN` environment variable. When the variable is unset, every test returns early and is reported as "passed" (not skipped — this is a Rust test framework limitation).

Each test truncates the `kine` table (`TRUNCATE kine RESTART IDENTITY`) before running to ensure isolation. Tests share a single database but never run concurrently against it because truncation resets state.

**Important:** The Postgres tests will create the `kine` table and indexes automatically on first run. You only need to create the database itself.

### MySQL Tests (`tests/mysql_backend.rs`)

Gated behind the `RHINO_MYSQL_DSN` environment variable. Same early-return pattern as Postgres.

Each test truncates the `kine` table (`TRUNCATE TABLE kine`) before running. MySQL's `TRUNCATE` resets the `AUTO_INCREMENT` counter, providing clean revision numbering per test.

**Important:** The MySQL tests will create the `kine` table and indexes automatically on first run. You only need to create the database itself.

## Test Inventory

### SQLite — 16 tests

| Test | Origin | What it verifies |
|------|--------|-----------------|
| `test_backend_create` | kine | Create, duplicate rejection (`KeyExists`), lease, count |
| `test_backend_get` | kine | Get, get after delete+recreate, nonexistent key returns `None` |
| `test_backend_update` | kine | Update value/lease, wrong revision rejected, `create_revision` preserved |
| `test_backend_delete` | kine | Delete with correct rev, wrong rev fails, unconditional delete (rev=0) |
| `test_backend_list` | kine | List all, historical revision, limit, sorted order |
| `test_backend_watch` | kine | Historical watch (5 events), prefix-filtered watch (2 events) |
| `test_create_and_get` | rhino | Basic create + get round-trip, `create_revision == mod_revision` |
| `test_create_after_delete` | rhino | Re-create a key after deletion |
| `test_revision_increases` | rhino | Revisions are strictly monotonically increasing |
| `test_list_returns_latest_version_only` | rhino | MVCC: list returns only the latest version of each key |
| `test_keys_only` | rhino | `keys_only` mode returns empty values for get and list |
| `test_watch_live_events` | rhino | Live events arrive via poll loop broadcast |
| `test_watch_sees_updates_and_deletes` | rhino | Watch delivers both update and delete events |
| `test_compact` | rhino | Compaction completes without error |
| `test_compact_removes_old_rows` | rhino | Compaction reduces row count, data remains readable |
| `test_db_size` | rhino | `db_size()` returns a positive value |

### PostgreSQL — 12 tests

| Test | Origin | What it verifies |
|------|--------|-----------------|
| `test_backend_create` | kine | Create, duplicate rejection, lease, count |
| `test_backend_get` | kine | Get, get after delete+recreate, nonexistent key |
| `test_backend_update` | kine | Update value/lease, wrong revision, `create_revision` preserved |
| `test_backend_delete` | kine | Delete with correct rev, wrong rev, unconditional delete |
| `test_backend_list` | kine | List all, historical revision, limit, sorted order |
| `test_backend_watch` | kine | Historical watch (5 events), prefix-filtered watch (2 events) |
| `test_create_after_delete` | rhino | Re-create after deletion |
| `test_revision_increases` | rhino | Monotonic revision ordering |
| `test_list_returns_latest_version_only` | rhino | MVCC latest-version-only |
| `test_keys_only` | rhino | `keys_only` returns empty values |
| `test_compact_removes_old_rows` | rhino | Compaction reduces row count, data readable |
| `test_db_size` | rhino | `pg_total_relation_size` returns positive |

### MySQL — 12 tests

| Test | Origin | What it verifies |
|------|--------|-----------------|
| `test_backend_create` | kine | Create, duplicate rejection, lease, count |
| `test_backend_get` | kine | Get, get after delete+recreate, nonexistent key |
| `test_backend_update` | kine | Update value/lease, wrong revision, `create_revision` preserved |
| `test_backend_delete` | kine | Delete with correct rev, wrong rev, unconditional delete |
| `test_backend_list` | kine | List all, historical revision, limit, sorted order |
| `test_backend_watch` | kine | Historical watch (5 events), prefix-filtered watch (2 events) |
| `test_create_after_delete` | rhino | Re-create after deletion |
| `test_revision_increases` | rhino | Monotonic revision ordering |
| `test_list_returns_latest_version_only` | rhino | MVCC latest-version-only |
| `test_keys_only` | rhino | `keys_only` returns empty values |
| `test_compact_removes_old_rows` | rhino | Compaction reduces row count, data readable |
| `test_db_size` | rhino | `information_schema` size query returns positive |

## Writing New Tests

### Backend tests

All three test files follow the same pattern. Each test calls `test_backend()` to get an initialized backend with a clean database, then exercises the `Backend` trait methods directly.

If you add a new Backend method or behavior, add a test to **all three** files: `sqlite_backend.rs`, `postgres_backend.rs`, and `mysql_backend.rs`.

For Postgres and MySQL, gate the test body with the early-return pattern:

```rust
#[tokio::test]
async fn test_my_feature() {
    let Some(b) = test_backend().await else {
        return; // skip when DSN env var is not set
    };

    // test body
}
```

### What to test

Tests ported from kine follow its test conventions:

1. **Get a `base_rev`** at the start — the revision after `start()` inserts internal rows (`compact_rev_key`, `/registry/health`).
2. **Assert exact revision math** — `base_rev + N` for each operation, verifying the revision sequence is gapless.
3. **Assert return values** — key, value, lease, `create_revision`, `mod_revision`, and the `ok` boolean for conditional operations.
4. **Test failure paths** — wrong revision on update/delete, duplicate create, nonexistent key.

### Watch test timing

Watch tests depend on the poll loop (1-second interval). Collect events with a deadline loop instead of a single `recv()`:

```rust
let mut all_events = Vec::new();
let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
while all_events.len() < expected_count && tokio::time::Instant::now() < deadline {
    match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        Ok(Some(batch)) => all_events.extend(batch),
        _ => break,
    }
}
```

The poll loop may batch multiple events into a single broadcast message, so never assume one event per `recv()`.

## Smoke-Testing the gRPC Server

Start the server and verify it with `etcdctl`:

```sh
# Terminal 1: start rhino
cargo run --bin rhino-server

# Terminal 2: test with etcdctl
etcdctl put /test/key "hello"
etcdctl get /test/key
etcdctl get /test/ --prefix
etcdctl del /test/key
etcdctl watch /test/ --prefix   # then put from another terminal
```

With different backends:

```sh
# PostgreSQL
cargo run --bin rhino-server -- --endpoint postgres://postgres:postgres@localhost/kubernetes

# MySQL
cargo run --bin rhino-server -- --endpoint mysql://root:root@localhost/kubernetes
```

## Environment Variables

| Variable | Required for | Example |
|----------|-------------|---------|
| `RHINO_POSTGRES_DSN` | Postgres tests | `postgres://postgres:postgres@localhost/rhino_test` |
| `RHINO_MYSQL_DSN` | MySQL tests | `mysql://root:root@localhost/rhino_test` |
| `RUST_LOG` | Debug logging | `debug`, `rhino=trace` |

## Cleanup

SQLite tests clean up automatically (temporary directories are deleted).

For Postgres and MySQL, the test databases persist. To reset:

```sh
docker rm -f rhino-pg rhino-mysql
```

Or drop the table manually:

```sql
DROP TABLE IF EXISTS kine;
```
