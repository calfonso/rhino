mod kv;
mod watch;
mod lease;
mod maintenance;
mod cluster;

use std::sync::Arc;
use tonic::transport::Server;
use tracing::info;

use crate::backend::Backend;
use crate::proto::etcdserverpb::{
    kv_server::KvServer, watch_server::WatchServer, lease_server::LeaseServer,
    maintenance_server::MaintenanceServer, cluster_server::ClusterServer,
};

/// The main rhino server that bridges the etcd gRPC API to a Backend implementation.
pub struct RhinoServer<B: Backend> {
    backend: Arc<B>,
}

impl<B: Backend> RhinoServer<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    /// Start the gRPC server on the given address.
    pub async fn serve(self, addr: &str) -> Result<(), Box<dyn std::error::Error>> {
        let addr = addr.parse()?;

        self.backend.start().await.map_err(|e| {
            Box::new(e) as Box<dyn std::error::Error>
        })?;

        let bridge = KvBridge::new(self.backend.clone());

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
}

impl<B: Backend> Clone for KvBridge<B> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
        }
    }
}

impl<B: Backend> KvBridge<B> {
    pub fn new(backend: Arc<B>) -> Self {
        Self { backend }
    }
}
