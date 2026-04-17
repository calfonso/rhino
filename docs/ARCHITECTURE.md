# Architecture

This document describes how Rhino is structured, how data flows through the system, and the design decisions behind it.

## Overview

Rhino is a translation layer. It accepts etcd v3 gRPC requests, converts them into SQL operations, and returns etcd-compatible responses. It is not a database — it delegates storage entirely to a pluggable SQL backend.

```
┌──────────────┐       gRPC v3        ┌──────────────┐         SQL          ┌──────────────────────┐
│  etcd Client │  ──────────────────▶  │    Rhino     │  ──────────────────▶  │  SQLite / Postgres  │
│  (K8s, etc.) │  ◀──────────────────  │  (API shim)  │  ◀──────────────────  │  / MySQL            │
└──────────────┘                       └──────────────┘                       └──────────────────────┘
```

## Module Structure

```
src/
├── lib.rs                    # Public API re-exports
├── backend.rs                # Backend trait, error types, core data structures
├── bin/
│   └── server.rs             # Standalone server binary (auto-detects backend from --endpoint)
├── drivers/
│   ├── mod.rs
│   ├── sqlite/mod.rs         # SQLite backend
│   ├── postgres/mod.rs       # PostgreSQL backend
│   └── mysql/mod.rs          # MySQL backend
└── server/
    ├── mod.rs                # RhinoServer, KvBridge, gRPC wiring
    ├── kv.rs                 # KV service (Range, Txn, Compact)
    ├── watch.rs              # Watch service (streaming)
    ├── lease.rs              # Lease service (passthrough)
    └── maintenance.rs        # Maintenance service (Status, Defragment)
```

## Key Abstractions

### Backend Trait

The `Backend` trait (`backend.rs`) defines 11 async methods that any storage driver must implement:

| Method             | Purpose                                          |
|--------------------|--------------------------------------------------|
| `start()`          | Initialize schema and start background tasks     |
| `get()`            | Retrieve a single key, optionally at a revision  |
| `create()`         | Atomically insert a new key (fails if exists)    |
| `update()`         | Conditional update with revision check           |
| `delete()`         | Conditional delete with revision check           |
| `list()`           | Range query by prefix with pagination            |
| `count()`          | Count keys matching a prefix                     |
| `watch()`          | Subscribe to key change events                   |
| `compact()`        | Remove old revisions                             |
| `db_size()`        | Report storage size in bytes                     |
| `current_revision()` | Return the latest revision number              |

All methods return `Result<T, BackendError>`. The error variants map directly to gRPC status codes in the server layer.

### RhinoServer

`RhinoServer<B: Backend>` is the top-level entry point. It wraps a backend in an `Arc`, calls `start()`, and registers four gRPC services (KV, Watch, Lease, Maintenance) on a Tonic server.

### KvBridge

`KvBridge<B>` is the internal adapter that implements etcd's gRPC service traits by translating protobuf requests into `Backend` method calls. It handles transaction pattern detection — recognizing whether a `TxnRequest` represents a create, update, or delete — and routes accordingly.

## Data Model

Rhino uses a single-table, log-structured schema compatible with [kine](https://github.com/k3s-io/kine). The exact DDL varies by backend, but the logical structure is the same:

### Schema

| Column            | SQLite                        | PostgreSQL                     | MySQL                              |
|-------------------|-------------------------------|--------------------------------|------------------------------------|
| `id`              | `INTEGER PRIMARY KEY AUTOINCREMENT` | `BIGSERIAL PRIMARY KEY`  | `BIGINT UNSIGNED AUTO_INCREMENT`   |
| `name`            | `TEXT`                        | `TEXT COLLATE "C"`             | `VARCHAR(630) CHARACTER SET ascii` |
| `created`         | `INTEGER`                     | `INTEGER`                      | `INTEGER`                          |
| `deleted`         | `INTEGER`                     | `INTEGER`                      | `INTEGER`                          |
| `create_revision` | `INTEGER`                     | `BIGINT`                       | `BIGINT UNSIGNED`                  |
| `prev_revision`   | `INTEGER`                     | `BIGINT`                       | `BIGINT UNSIGNED`                  |
| `lease`           | `INTEGER`                     | `INTEGER`                      | `INTEGER`                          |
| `value`           | `BLOB`                        | `BYTEA`                        | `MEDIUMBLOB`                       |
| `old_value`       | `BLOB`                        | `BYTEA`                        | `MEDIUMBLOB`                       |

### How it works

Every mutation (create, update, delete) **appends a new row**. Nothing is updated in place. The `id` column serves as the global revision number.

| Column            | Role                                                        |
|-------------------|-------------------------------------------------------------|
| `id`              | Monotonic revision (auto-increment primary key)             |
| `name`            | The key path (e.g., `/registry/pods/default/nginx`)         |
| `created`         | `1` if this row represents a creation, `0` otherwise        |
| `deleted`         | `1` if this row represents a deletion, `0` otherwise        |
| `create_revision` | Revision when this key was first created                    |
| `prev_revision`   | Revision of the previous mutation for this key              |
| `lease`           | Associated lease ID                                         |
| `value`           | Current value (as bytes)                                    |
| `old_value`       | Previous value before this mutation                         |

### Indexes

All backends share these five indexes:

- **`kine_name_index`** — fast single-key lookups
- **`kine_name_id_index`** — prefix range scans ordered by revision
- **`kine_id_deleted_index`** — filtering deleted entries during listing
- **`kine_prev_revision_index`** — compaction queries (finding superseded rows)
- **`kine_name_prev_revision_uindex`** — unique constraint preventing duplicate creates

PostgreSQL adds a sixth index (`kine_list_query_index`) on `(name, id DESC, deleted)` to optimize its `DISTINCT ON` list queries.

## Revision System

Revisions are monotonically increasing integers derived from each backend's auto-increment primary key. Every write operation produces a new revision.

- **`create_revision`** — the revision when a key was originally created (persists across updates)
- **`mod_revision`** — the revision of the most recent mutation (the row's `id`)

Clients can read historical state by specifying a revision in `get()` or `list()` calls. The backend locates the most recent row for each key where `id <= requested_revision`.

## List Query Strategy

Each backend uses a different SQL strategy to efficiently find the latest version of each key:

| Backend    | Strategy                                                          |
|------------|-------------------------------------------------------------------|
| SQLite     | `JOIN (SELECT MAX(id) ... GROUP BY name)` — standard SQL subquery |
| PostgreSQL | `DISTINCT ON (name) ... ORDER BY name, id DESC` — Postgres-specific optimization |
| MySQL      | `JOIN (SELECT MAX(id) ... GROUP BY name)` — same as SQLite        |

Both strategies produce the same result: for each key name matching the prefix, return only the row with the highest `id`.

## Watch System

Watches use a poll-broadcast architecture:

1. **Poll loop** — a background Tokio task runs every second, querying for rows with `id > last_seen_revision`
2. **Broadcast channel** — new events are sent to a `tokio::sync::broadcast` channel (capacity: 1024)
3. **Per-watcher filtering** — each watch subscription receives all events and filters by its prefix
4. **Historical replay** — when a watch starts with a specific revision, the backend first queries all events from that revision to the present, then switches to live streaming

### Gap filling

If the poll loop detects a revision gap (e.g., row 5 is visible but row 4 is not yet), it:

1. Records the gap and retries after a short delay
2. If the gap persists, inserts a placeholder "fill" record at the missing revision
3. Fill records use the key name `gap-{revision}` and are filtered out of query results

This ensures the event stream is strictly sequential, which is required by etcd clients.

## Compaction

Old revisions accumulate over time. The compaction system cleans them up:

- **Automatic** — a background task runs every `compact_interval` (default: 5 minutes)
- **Batched** — processes `compact_batch_size` rows per pass to limit lock contention
- **Retention** — keeps at least `compact_min_retain` revisions (default: 1000)
- **What gets removed** — superseded rows (where a newer row exists for the same key) and deletion tombstones

The compact DELETE query varies by backend:

| Backend    | Syntax                                                |
|------------|-------------------------------------------------------|
| SQLite     | `DELETE ... WHERE id IN (SELECT ... UNION SELECT ...)` |
| PostgreSQL | `DELETE ... USING (...) AS ks WHERE kv.id = ks.id`    |
| MySQL      | `DELETE kv FROM ... INNER JOIN (...) AS ks ON kv.id = ks.id` |

After compaction, SQLite runs `PRAGMA wal_checkpoint(FULL)` to flush committed pages back to the database file. PostgreSQL and MySQL have no post-compact step.

Manual compaction is also exposed via the `Compact` gRPC RPC.

## Transaction Pattern Detection

etcd's `Txn` RPC is a general-purpose conditional operation. Rhino detects common patterns and optimizes them:

| Pattern | Detection | Optimization |
|---------|-----------|--------------|
| **Create** | `Compare: MOD == 0` + single `Put` in success | Uses atomic `INSERT` with unique constraint |
| **Update** | `Compare: MOD == rev` + `Put` in success + `Range` in failure | Uses conditional update with revision check |
| **Delete** | `Compare: MOD == rev` + `DeleteRange` in success | Uses conditional delete with revision check |

This avoids the overhead of a generic transaction engine while handling the patterns that Kubernetes and other etcd clients actually use.

## Backend-Specific Details

### SQLite

- Uses WAL journal mode for concurrent reads during writes
- `synchronous = NORMAL` for a balance of durability and performance
- `busy_timeout = 30s` to wait on lock contention instead of failing
- Connection pool capped at 5
- Insert uses `last_insert_rowid()` for the new revision
- Gap fill uses `INSERT OR IGNORE`

### PostgreSQL

- Uses `TEXT COLLATE "C"` to ensure LIKE queries use indexes correctly
- Insert uses `RETURNING id` clause for the new revision
- List queries use `DISTINCT ON (name)` for efficient latest-version lookups
- Gap fill uses `INSERT ... ON CONFLICT DO NOTHING` and resets the sequence
- Start key translation: `\x00` replaced with `\x1a` (Postgres rejects null bytes in UTF-8)
- Unique violation detected via SQLSTATE code `23505`

### MySQL

- Uses `VARCHAR(630) CHARACTER SET ascii` for key names (MySQL index length limits)
- Uses `MEDIUMBLOB` for values (up to 16 MB, vs BLOB's 64 KB)
- Insert uses `LAST_INSERT_ID()` via `last_insert_id()` in the sqlx result
- Gap fill uses `INSERT IGNORE`
- Start key translation: `\x00` replaced with `#` (MySQL latin1 collation limitation)
- Unique violation detected via MySQL error code `1062`
- Index creation ignores error `1061` (duplicate key name) for idempotent setup

## Error Mapping

| BackendError   | gRPC Status          | When                                    |
|----------------|----------------------|-----------------------------------------|
| `KeyExists`    | `FAILED_PRECONDITION`| Create called for an existing key       |
| `Compacted`    | `OUT_OF_RANGE`       | Requested revision has been compacted   |
| `FutureRev`    | `OUT_OF_RANGE`       | Requested revision hasn't happened yet  |
| `Internal`     | `INTERNAL`           | Database errors, connection failures    |
