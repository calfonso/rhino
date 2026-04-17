use clap::Parser;
use rhino::{RhinoServer, SqliteBackend, SqliteConfig};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "rhino-server", about = "etcd-compatible gRPC server backed by SQLite")]
struct Args {
    /// gRPC listen address
    #[arg(long, default_value = "0.0.0.0:2379")]
    listen_address: String,

    /// Path to the SQLite database file
    #[arg(long, default_value = "./db/state.db")]
    db_path: String,

    /// Compaction interval in seconds (0 to disable)
    #[arg(long, default_value = "300")]
    compact_interval: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let config = SqliteConfig {
        dsn: args.db_path,
        compact_interval: Duration::from_secs(args.compact_interval),
        ..Default::default()
    };

    let backend = SqliteBackend::new(config).await?;
    let server = RhinoServer::new(backend);

    server.serve(&args.listen_address).await
}
