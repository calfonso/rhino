# Getting Started

This guide walks you through installing Rhino, running your first server, and connecting with standard etcd clients.

## Prerequisites

- **Rust 1.80+** (stable, 2024 edition)
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
git clone https://github.com/calfonso/rhino.git
cd rhino
cargo build --release
```

The server binary will be at `target/release/rhino-server`.

## Running the Server

### Standalone binary

The simplest way to start is with the included binary:

```sh
cargo run --bin rhino-server
```

This starts a gRPC server on `0.0.0.0:2379` with a SQLite database at `./db/state.db`.

### Choosing a backend

The `--endpoint` flag auto-detects the backend from the URL scheme:

```sh
# SQLite (default — any file path)
rhino-server --endpoint ./db/state.db

# PostgreSQL
rhino-server --endpoint postgres://user:pass@localhost/kubernetes

# MySQL
rhino-server --endpoint mysql://root:root@localhost/kubernetes
```

All flags:

```
--listen-address <ADDR>      gRPC listen address [default: 0.0.0.0:2379]
--endpoint <ENDPOINT>        Storage endpoint [default: ./db/state.db]
--compact-interval <SECS>    Compaction interval, 0 to disable [default: 300]
```

### With Docker

```sh
docker build -t rhino .
docker run -p 2379:2379 -v rhino-data:/data rhino
```

The container stores its database at `/data/db/state.db`. Override with arguments:

```sh
# Use Postgres instead
docker run -p 2379:2379 rhino \
  --endpoint postgres://user:pass@db-host/kubernetes
```

### Logging

Control verbosity with `RUST_LOG`:

```sh
RUST_LOG=info  rhino-server          # default
RUST_LOG=debug rhino-server          # connection and query details
RUST_LOG=rhino=trace rhino-server    # everything including poll loop events
```

## Embed as a library

### SQLite

```rust
use rhino::{RhinoServer, SqliteBackend, SqliteConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let backend = SqliteBackend::new(SqliteConfig::default()).await?;
    let server = RhinoServer::new(backend);
    server.serve("0.0.0.0:2379").await?;
    Ok(())
}
```

Rhino will create the database file and parent directories if they don't exist.

### PostgreSQL

```rust
use rhino::{RhinoServer, PostgresBackend, PostgresConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = PostgresConfig {
        dsn: "postgres://user:pass@localhost/kubernetes".to_string(),
        ..Default::default()
    };
    let backend = PostgresBackend::new(config).await?;
    RhinoServer::new(backend).serve("0.0.0.0:2379").await
}
```

The database must already exist. Rhino creates the `kine` table and indexes automatically.

### MySQL

```rust
use rhino::{RhinoServer, MysqlBackend, MysqlConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = MysqlConfig {
        dsn: "mysql://root:root@localhost/kubernetes".to_string(),
        ..Default::default()
    };
    let backend = MysqlBackend::new(config).await?;
    RhinoServer::new(backend).serve("0.0.0.0:2379").await
}
```

### Custom configuration

All backends share the same compaction knobs:

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
RhinoServer::new(backend).serve("0.0.0.0:2379").await?;
```

Set `compact_interval` to `Duration::ZERO` to disable automatic compaction entirely.

## Using with etcd clients

Once Rhino is running, any etcd v3 client can connect to it.

### etcdctl

```sh
# Set the endpoint
export ETCDCTL_ENDPOINTS=http://localhost:2379

# Write
etcdctl put /app/config/db_host "postgres.internal"

# Read
etcdctl get /app/config/db_host

# List by prefix
etcdctl put /app/config/db_port "5432"
etcdctl put /app/config/cache_ttl "300"
etcdctl get /app/config/ --prefix

# Watch for changes (blocks and streams)
etcdctl watch /app/config/ --prefix

# Delete
etcdctl del /app/config/cache_ttl
```

### Go client

```go
cli, _ := clientv3.New(clientv3.Config{
    Endpoints: []string{"localhost:2379"},
})
defer cli.Close()

cli.Put(ctx, "/mykey", "myvalue")
resp, _ := cli.Get(ctx, "/mykey")
```

### Rust client (etcd-client crate)

```rust
let mut client = etcd_client::Client::connect(["localhost:2379"], None).await?;
client.put("/mykey", "myvalue", None).await?;
let resp = client.get("/mykey", None).await?;
```

## Implementing a custom backend

Rhino's `Backend` trait lets you plug in any storage engine:

```rust
use rhino::backend::{Backend, BackendError, KeyValue, WatchResult};
use async_trait::async_trait;

pub struct MyBackend { /* ... */ }

#[async_trait]
impl Backend for MyBackend {
    async fn start(&self) -> Result<(), BackendError> { todo!() }
    async fn get(&self, key: &str, range_end: &str, limit: i64,
                 revision: i64, keys_only: bool)
        -> Result<(i64, Option<KeyValue>), BackendError> { todo!() }
    async fn create(&self, key: &str, value: &[u8], lease: i64)
        -> Result<i64, BackendError> { todo!() }
    async fn delete(&self, key: &str, revision: i64)
        -> Result<(i64, Option<KeyValue>, bool), BackendError> { todo!() }
    async fn list(&self, prefix: &str, start_key: &str, limit: i64,
                  revision: i64, keys_only: bool)
        -> Result<(i64, Vec<KeyValue>), BackendError> { todo!() }
    async fn count(&self, prefix: &str, start_key: &str, revision: i64)
        -> Result<(i64, i64), BackendError> { todo!() }
    async fn update(&self, key: &str, value: &[u8], revision: i64, lease: i64)
        -> Result<(i64, Option<KeyValue>, bool), BackendError> { todo!() }
    async fn watch(&self, key: &str, revision: i64)
        -> Result<WatchResult, BackendError> { todo!() }
    async fn db_size(&self) -> Result<i64, BackendError> { todo!() }
    async fn current_revision(&self) -> Result<i64, BackendError> { todo!() }
    async fn compact(&self, revision: i64)
        -> Result<i64, BackendError> { todo!() }
}
```

Then pass it to `RhinoServer`:

```rust
let server = RhinoServer::new(MyBackend::new());
server.serve("0.0.0.0:2379").await?;
```

## Next steps

- Read the [Architecture](ARCHITECTURE.md) doc to understand the data model and internals
- Read the [Testing](TESTING.md) doc for how to run and write tests
- Check [issues](https://github.com/calfonso/rhino/issues) for planned work
