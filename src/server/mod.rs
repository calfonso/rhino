mod kv;
mod watch;
mod lease;
mod maintenance;
mod cluster;

use std::sync::Arc;
use std::time::Duration;
use tonic::transport::Server;
use tracing::info;

use crate::backend::Backend;
use crate::proto::etcdserverpb::{
    kv_server::KvServer, watch_server::WatchServer, lease_server::LeaseServer,
    maintenance_server::MaintenanceServer, cluster_server::ClusterServer,
};

/// Default watch progress notify interval (matching kine's default).
const DEFAULT_NOTIFY_INTERVAL: Duration = Duration::from_secs(5);

/// Default emulated etcd version string.
const DEFAULT_EMULATED_VERSION: &str = "3.5.13";

/// The main rhino server that bridges the etcd gRPC API to a Backend implementation.
pub struct RhinoServer<B: Backend> {
    backend: Arc<B>,
    notify_interval: Duration,
    emulated_etcd_version: String,
}

impl<B: Backend> RhinoServer<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend: Arc::new(backend),
            notify_interval: DEFAULT_NOTIFY_INTERVAL,
            emulated_etcd_version: DEFAULT_EMULATED_VERSION.to_string(),
        }
    }

    /// Set the watch progress notify interval.
    pub fn with_notify_interval(mut self, interval: Duration) -> Self {
        self.notify_interval = interval;
        self
    }

    /// Set the emulated etcd version string returned by the Status RPC.
    pub fn with_emulated_etcd_version(mut self, version: String) -> Self {
        self.emulated_etcd_version = version;
        self
    }

    /// Start the gRPC server on the given address.
    pub async fn serve(self, addr: &str) -> Result<(), Box<dyn std::error::Error>> {
        let addr = addr.parse()?;

        self.backend.start().await.map_err(|e| {
            Box::new(e) as Box<dyn std::error::Error>
        })?;

        let bridge = KvBridge::new(
            self.backend.clone(),
            self.notify_interval,
            self.emulated_etcd_version,
        );

        // gRPC health service (matching kine)
        let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
        health_reporter.set_serving::<KvServer<KvBridge<B>>>().await;

        info!("rhino listening on {}", addr);

        Server::builder()
            .add_service(health_service)
            .add_service(KvServer::new(bridge.clone()))
            .add_service(WatchServer::new(bridge.clone()))
            .add_service(LeaseServer::new(bridge.clone()))
            .add_service(MaintenanceServer::new(bridge.clone()))
            .add_service(ClusterServer::new(bridge.clone()))
            .serve(addr)
            .await?;

        Ok(())
    }
}

/// The bridge struct that implements all etcd gRPC service traits by delegating to a Backend.
pub(crate) struct KvBridge<B: Backend> {
    backend: Arc<B>,
    notify_interval: Duration,
    emulated_etcd_version: String,
}

impl<B: Backend> Clone for KvBridge<B> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            notify_interval: self.notify_interval,
            emulated_etcd_version: self.emulated_etcd_version.clone(),
        }
    }
}

impl<B: Backend> KvBridge<B> {
    pub fn new(backend: Arc<B>, notify_interval: Duration, emulated_etcd_version: String) -> Self {
        Self { backend, notify_interval, emulated_etcd_version }
    }
}
