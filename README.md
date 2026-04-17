# Rhino

**etcd, backed by SQL.**

Rhino is a drop-in etcd v3 gRPC server written in Rust that stores everything in a relational database. Same API your tools already speak — simpler operations, no Raft consensus required.

## Why Rhino?

Running etcd in production means managing a distributed consensus cluster: quorum maintenance, defragmentation, backup/restore choreography, and peer TLS. For many workloads — edge deployments, single-node clusters, CI environments, development — that complexity isn't justified.

Rhino eliminates it. Your etcd clients connect to Rhino exactly as they would to etcd. Under the hood, every key-value mutation becomes a SQL row. You get the operational model of a relational database (backup with `pg_dump`, replicate with your existing tooling, inspect state with `SELECT *`) while keeping full etcd v3 API compatibility.

## Features

- **Full etcd v3 gRPC API** — KV, Watch, Lease, and Maintenance services
- **Atomic transactions** — compare-and-swap with revision-based optimistic concurrency
- **Watch streams** — real-time gRPC streaming of key changes with prefix matching and historical replay
- **Revision history** — log-structured storage with monotonic revisions; query any point in time
- **Range queries** — list and count keys by prefix with pagination
- **Auto-compaction** — background compaction removes old revisions on a configurable schedule
- **Pluggable backends** — trait-based abstraction lets you swap storage engines
- **Async-first** — built on Tokio and Tonic with non-blocking I/O throughout

## Supported Backends

| Backend    | Status  |
|------------|---------|
| SQLite     | Ready   |
| PostgreSQL | Planned |
| MySQL      | Planned |

## Quickstart

### As a library

Add Rhino to your project:

```sh
cargo add rhino
```

Embed and run:

```rust
use rhino::{RhinoServer, SqliteBackend, SqliteConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = SqliteConfig {
        dsn: "./db/state.db".to_string(),
        ..Default::default()
    };

    let backend = SqliteBackend::new(config).await?;
    let server = RhinoServer::new(backend);
    server.serve("0.0.0.0:2379").await?;
    Ok(())
}
```

### With etcdctl

Once the server is running, any standard etcd client works:

```sh
# Put a key
etcdctl put /myapp/config '{"port": 8080}'

# Read it back
etcdctl get /myapp/config

# List by prefix
etcdctl get /myapp/ --prefix

# Watch for changes
etcdctl watch /myapp/ --prefix
```

## Configuration

`SqliteConfig` controls the SQLite backend:

| Field                | Type       | Default          | Description                            |
|----------------------|------------|------------------|----------------------------------------|
| `dsn`                | `String`   | `./db/state.db`  | Path to the SQLite database file       |
| `compact_interval`   | `Duration` | 300 seconds      | How often to run background compaction |
| `compact_min_retain` | `i64`      | 1000             | Minimum revisions to keep              |
| `compact_batch_size` | `i64`      | 1000             | Rows processed per compaction batch    |

Set `compact_interval` to `Duration::ZERO` to disable automatic compaction.

## Documentation

- **[Getting Started](docs/GETTING_STARTED.md)** — installation, first steps, and common usage patterns
- **[Architecture](docs/ARCHITECTURE.md)** — system design, data model, and how the pieces fit together

## Running Tests

```sh
cargo test
```

Tests use temporary in-memory SQLite databases and cover create, read, update, delete, list, count, watch, revision ordering, compaction, and error handling.

## License

Apache 2.0 — see [LICENSE](LICENSE).
