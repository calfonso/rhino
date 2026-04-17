# Getting Started

This guide walks you through installing Rhino, running your first server, and connecting with standard etcd clients.

## Prerequisites

- **Rust 1.85+** (2024 edition)
- **protoc** (Protocol Buffers compiler) — required to build the gRPC stubs
  ```sh
  # macOS
  brew install protobuf

  # Fedora / RHEL
  dnf install protobuf-compiler

  # Ubuntu / Debian
  apt install protobuf-compiler
  ```

## Installation

### As a dependency

Add Rhino to your Cargo project:

```sh
cargo add rhino
```

### From source

```sh
git clone https://github.com/chrisalfonso/rhino.git
cd rhino
cargo build --release
```

## Running the Server

### Embed in your application

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

Rhino will:
1. Create the database file and parent directories if they don't exist
2. Initialize the schema (table and indexes)
3. Start background compaction and watch poll loops
4. Begin accepting gRPC connections on port 2379

### Custom configuration

```rust
use std::time::Duration;
use rhino::{SqliteConfig, SqliteBackend, RhinoServer};

let config = SqliteConfig {
    dsn: "/var/lib/rhino/data.db".to_string(),
    compact_interval: Duration::from_secs(600),  // compact every 10 minutes
    compact_min_retain: 5000,                     // keep at least 5000 revisions
    compact_batch_size: 2000,                     // process 2000 rows per batch
};

let backend = SqliteBackend::new(config).await?;
let server = RhinoServer::new(backend);
server.serve("0.0.0.0:2379").await?;
```

To disable automatic compaction entirely, set `compact_interval` to `Duration::ZERO`.

## Using with etcd Clients

Once Rhino is running, any etcd v3 client can connect to it. Here are common operations using `etcdctl`:

### Write and read

```sh
# Set the endpoint
export ETCDCTL_ENDPOINTS=http://localhost:2379

# Create a key
etcdctl put /app/config/db_host "postgres.internal"

# Read it back
etcdctl get /app/config/db_host
```

### List by prefix

```sh
# Create a few keys
etcdctl put /app/config/db_host "postgres.internal"
etcdctl put /app/config/db_port "5432"
etcdctl put /app/config/cache_ttl "300"

# List all config keys
etcdctl get /app/config/ --prefix
```

### Watch for changes

In one terminal, start a watch:

```sh
etcdctl watch /app/config/ --prefix
```

In another terminal, make a change:

```sh
etcdctl put /app/config/db_host "postgres-2.internal"
```

The watch terminal will show the update in real time.

### Delete a key

```sh
etcdctl del /app/config/cache_ttl
```

## Implementing a Custom Backend

Rhino's `Backend` trait lets you plug in any storage engine. Implement these methods:

```rust
use rhino::backend::{Backend, BackendError, KeyValue, Event, WatchResult};
use async_trait::async_trait;

pub struct MyBackend { /* ... */ }

#[async_trait]
impl Backend for MyBackend {
    async fn start(&self) -> Result<(), BackendError> { /* ... */ }
    async fn get(&self, key: &str, range_end: &str, limit: i64,
                 revision: i64, keys_only: bool)
        -> Result<(i64, Option<KeyValue>), BackendError> { /* ... */ }
    async fn create(&self, key: &str, value: &[u8], lease: i64)
        -> Result<i64, BackendError> { /* ... */ }
    async fn delete(&self, key: &str, revision: i64)
        -> Result<(i64, Option<KeyValue>, bool), BackendError> { /* ... */ }
    async fn list(&self, prefix: &str, start_key: &str, limit: i64,
                  revision: i64, keys_only: bool)
        -> Result<(i64, Vec<KeyValue>), BackendError> { /* ... */ }
    async fn count(&self, prefix: &str, start_key: &str, revision: i64)
        -> Result<(i64, i64), BackendError> { /* ... */ }
    async fn update(&self, key: &str, value: &[u8], revision: i64, lease: i64)
        -> Result<(i64, Option<KeyValue>, bool), BackendError> { /* ... */ }
    async fn watch(&self, key: &str, revision: i64)
        -> Result<WatchResult, BackendError> { /* ... */ }
    async fn db_size(&self) -> Result<i64, BackendError> { /* ... */ }
    async fn current_revision(&self) -> Result<i64, BackendError> { /* ... */ }
    async fn compact(&self, revision: i64) -> Result<i64, BackendError> { /* ... */ }
}
```

Then pass it to `RhinoServer`:

```rust
let backend = MyBackend::new(/* ... */);
let server = RhinoServer::new(backend);
server.serve("0.0.0.0:2379").await?;
```

## Running Tests

```sh
cargo test
```

Tests create temporary SQLite databases in isolated temp directories and cover:

- Create, get, update, delete operations
- Duplicate key prevention
- Revision ordering and consistency
- Prefix listing with pagination
- Key counting
- Watch event streaming
- Compaction
- Database size reporting

## Next Steps

- Read the [Architecture](ARCHITECTURE.md) doc to understand the data model and internals
- Browse the [source](src/) to see the Backend trait and SQLite implementation
- Check [issues](https://github.com/chrisalfonso/rhino/issues) for planned work and contribution opportunities
