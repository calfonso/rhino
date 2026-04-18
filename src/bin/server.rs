use clap::Parser;
use rhino::{
    MysqlBackend, MysqlConfig, PostgresBackend, PostgresConfig, RhinoServer, SqliteBackend,
    SqliteConfig,
};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "rhino-server", about = "etcd-compatible gRPC server backed by SQL")]
struct Args {
    /// gRPC listen address
    #[arg(long, default_value = "0.0.0.0:2379")]
    listen_address: String,

    /// Storage endpoint. Use a file path for SQLite (default),
    /// postgres:// for PostgreSQL, or mysql:// for MySQL.
    #[arg(long, default_value = "./db/state.db")]
    endpoint: String,

    /// Compaction interval in seconds (0 to disable)
    #[arg(long, default_value = "300")]
    compact_interval: u64,

    /// Minimum number of recent revisions to retain during compaction
    #[arg(long, default_value = "1000")]
    compact_retention: i64,

    /// Maximum database connection pool size
    #[arg(long, default_value = "10")]
    max_connections: u32,

    /// Watch progress notify interval in seconds
    #[arg(long, default_value = "5")]
    watch_progress_notify_interval: u64,

    /// Emulated etcd version string returned by the Status RPC
    #[arg(long, default_value = "3.5.13")]
    emulated_etcd_version: String,
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
    let compact_interval = Duration::from_secs(args.compact_interval);
    let notify_interval = Duration::from_secs(args.watch_progress_notify_interval);

    if args.endpoint.starts_with("postgres://") || args.endpoint.starts_with("postgresql://") {
        let config = PostgresConfig {
            dsn: args.endpoint,
            compact_interval,
            compact_min_retain: args.compact_retention,
            max_connections: args.max_connections,
            ..Default::default()
        };
        let backend = PostgresBackend::new(config).await?;
        RhinoServer::new(backend)
            .with_notify_interval(notify_interval)
            .with_emulated_etcd_version(args.emulated_etcd_version.clone())
            .serve(&args.listen_address)
            .await
    } else if args.endpoint.starts_with("mysql://") || args.endpoint.starts_with("mariadb://") {
        let config = MysqlConfig {
            dsn: args.endpoint,
            compact_interval,
            compact_min_retain: args.compact_retention,
            max_connections: args.max_connections,
            ..Default::default()
        };
        let backend = MysqlBackend::new(config).await?;
        RhinoServer::new(backend)
            .with_notify_interval(notify_interval)
            .with_emulated_etcd_version(args.emulated_etcd_version.clone())
            .serve(&args.listen_address)
            .await
    } else {
        let config = SqliteConfig {
            dsn: args.endpoint,
            compact_interval,
            compact_min_retain: args.compact_retention,
            max_connections: args.max_connections,
            ..Default::default()
        };
        let backend = SqliteBackend::new(config).await?;
        RhinoServer::new(backend)
            .with_notify_interval(notify_interval)
            .with_emulated_etcd_version(args.emulated_etcd_version.clone())
            .serve(&args.listen_address)
            .await
    }
}
