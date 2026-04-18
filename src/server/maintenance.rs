use tonic::{Request, Response, Status};

use crate::backend::{Backend, BackendError};
use crate::proto::etcdserverpb::*;
use crate::proto::etcdserverpb::maintenance_server::Maintenance;

use super::KvBridge;

fn backend_err_to_status(e: BackendError) -> Status {
    Status::internal(e.to_string())
}

#[tonic::async_trait]
impl<B: Backend> Maintenance for KvBridge<B> {
    async fn alarm(
        &self,
        _request: Request<AlarmRequest>,
    ) -> Result<Response<AlarmResponse>, Status> {
        Err(Status::unknown("alarm is not supported"))
    }

    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let db_size = self
            .backend
            .db_size()
            .await
            .map_err(backend_err_to_status)?;

        Ok(Response::new(StatusResponse {
            header: Some(ResponseHeader::default()),
            version: self.emulated_etcd_version.clone(),
            db_size,
            ..Default::default()
        }))
    }

    async fn defragment(
        &self,
        _request: Request<DefragmentRequest>,
    ) -> Result<Response<DefragmentResponse>, Status> {
        Err(Status::unknown("defragment is not supported"))
    }

    async fn hash(
        &self,
        _request: Request<HashRequest>,
    ) -> Result<Response<HashResponse>, Status> {
        Err(Status::unknown("hash is not supported"))
    }

    async fn hash_kv(
        &self,
        _request: Request<HashKvRequest>,
    ) -> Result<Response<HashKvResponse>, Status> {
        Err(Status::unknown("hash kv is not supported"))
    }

    type SnapshotStream =
        std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<SnapshotResponse, Status>> + Send>>;

    async fn snapshot(
        &self,
        _request: Request<SnapshotRequest>,
    ) -> Result<Response<Self::SnapshotStream>, Status> {
        Err(Status::unknown("snapshot is not supported"))
    }

    async fn move_leader(
        &self,
        _request: Request<MoveLeaderRequest>,
    ) -> Result<Response<MoveLeaderResponse>, Status> {
        Err(Status::unknown("move leader is not supported"))
    }

    async fn downgrade(
        &self,
        _request: Request<DowngradeRequest>,
    ) -> Result<Response<DowngradeResponse>, Status> {
        Err(Status::unknown("downgrade is not supported"))
    }
}
