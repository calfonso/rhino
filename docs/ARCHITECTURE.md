# Architecture

This document describes how Rhino is structured, how data flows through the system, and the design decisions behind it.

## Overview

Rhino is a translation layer. It accepts etcd v3 gRPC requests, converts them into SQL operations, and returns etcd-compatible responses. It is not a database — it delegates storage entirely to a pluggable SQL backend.

```
┌──────────────┐       gRPC v3        ┌──────────────┐         SQL          ┌──────────────┐
│  etcd Client │  ──────────────────▶  │    Rhino     │  ──────────────────▶  │   Database   │
│  (K8s, etc.) │  ◀──────────────────  │  (API shim)  │  ◀──────────────────  │  (SQLite/PG) │
└──────────────┘                       └──────────────┘                       └──────────────┘
```

## Module Structure

```
src/
├── lib.rs                    # Public API re-exports
├── backend.rs                # Backend trait, error types, core data structures
├── drivers/
│   └── sqlite/
│       └── mod.rs            # SQLite backend implementation
└── server/
    ├── mod.rs                # RhinoServer, KvBridge, gRPC wiring
    ├── kv.rs                 # KV service (Range, Txn, Compact)
    ├── watch.rs              # Watch service (streaming)
    ├── lease.rs              # Lease service (stub)
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

Rhino uses a single-table, log-structured schema compatible with [kine](https://github.com/k3s-io/kine):

```sql
CREATE TABLE IF NOT EXISTS kine (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT,
    created INTEGER,
    deleted INTEGER,
    create_revision INTEGER,
    prev_revision INTEGER,
    lease INTEGER,
    value BLOB,
    old_value BLOB
)
```

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

Five indexes optimize the common query patterns:

- **`kine_name_index`** — fast single-key lookups
- **`kine_name_id_index`** — prefix range scans ordered by revision
- **`kine_id_deleted_index`** — filtering deleted entries during listing
- **`kine_prev_revision_index`** — compaction queries (finding superseded rows)
- **`kine_name_prev_revision_uindex`** — unique constraint preventing duplicate creates

## Revision System

Revisions in Rhino are monotonically increasing integers derived from the SQLite `AUTOINCREMENT` primary key. Every write operation produces a new revision.

- **`create_revision`** — the revision when a key was originally created (persists across updates)
- **`mod_revision`** — the revision of the most recent mutation (the row's `id`)
- **`version`** — a per-key counter that increments with each update and resets on re-creation

Clients can read historical state by specifying a revision in `get()` or `list()` calls. The backend locates the most recent row for each key where `id <= requested_revision`.

## Watch System

Watches use a poll-broadcast architecture:

1. **Poll loop** — a background Tokio task runs every second, querying for rows with `id > last_seen_revision`
2. **Broadcast channel** — new events are sent to a `tokio::sync::broadcast` channel (capacity: 1024)
3. **Per-watcher filtering** — each watch subscription receives all events and filters by its prefix
4. **Historical replay** — when a watch starts with a specific revision, the backend first queries all events from that revision to the present, then switches to live streaming

### Gap filling

If an external writer inserts rows directly into the database (bypassing Rhino), the poll loop detects revision gaps and inserts placeholder "gap-fill" records to maintain a continuous revision sequence. These placeholders are filtered out of query results.

## Compaction

Old revisions accumulate over time. The compaction system cleans them up:

- **Automatic** — a background task runs every `compact_interval` (default: 5 minutes)
- **Batched** — processes `compact_batch_size` rows per pass to limit lock contention
- **Retention** — keeps at least `compact_min_retain` revisions (default: 1000)
- **What gets removed** — superseded rows (where a newer row exists for the same key) and deletion tombstones
- **WAL checkpoint** — after compaction, a WAL checkpoint reclaims disk space

Manual compaction is also exposed via the `Compact` gRPC RPC.

## Transaction Pattern Detection

etcd's `Txn` RPC is a general-purpose conditional operation. Rhino detects common patterns and optimizes them:

| Pattern | Detection | Optimization |
|---------|-----------|--------------|
| **Create** | `Compare: MOD == 0` + single `Put` in success | Uses atomic `INSERT` with unique constraint |
| **Update** | `Compare: MOD == rev` + `Put` in success + `Range` in failure | Uses conditional `UPDATE` with revision check |
| **Delete** | `Compare: MOD == rev` + `DeleteRange` in success | Uses conditional delete with revision check |

This avoids the overhead of a generic transaction engine while handling the patterns that Kubernetes and other etcd clients actually use.

## SQLite Configuration

The SQLite backend applies several pragmas for performance:

| Pragma        | Value      | Why                                              |
|---------------|------------|--------------------------------------------------|
| `journal_mode`| WAL        | Allows concurrent reads during writes            |
| `synchronous` | NORMAL     | Balances durability with write performance       |
| `busy_timeout`| 30000 ms   | Waits instead of failing on lock contention      |
| `cache_size`  | -2000      | ~2 MB page cache                                 |

The connection pool is capped at 5 connections.
