use std::pin::Pin;
use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming};

use crate::backend::Backend;
use crate::proto::etcdserverpb::*;
use crate::proto::etcdserverpb::lease_server::Lease;

use super::KvBridge;

type LeaseKeepAliveStream =
    Pin<Box<dyn Stream<Item = Result<LeaseKeepAliveResponse, Status>> + Send>>;

#[tonic::async_trait]
impl<B: Backend> Lease for KvBridge<B> {
    async fn lease_grant(
        &self,
        request: Request<LeaseGrantRequest>,
    ) -> Result<Response<LeaseGrantResponse>, Status> {
        let r = request.into_inner();
        // Kine returns TTL as the lease ID (not a unique identifier).
        Ok(Response::new(LeaseGrantResponse {
            header: Some(ResponseHeader::default()),
            id: r.ttl,
            ttl: r.ttl,
            error: String::new(),
        }))
    }

    async fn lease_revoke(
        &self,
        _request: Request<LeaseRevokeRequest>,
    ) -> Result<Response<LeaseRevokeResponse>, Status> {
        Err(Status::unknown("lease revoke is not supported"))
    }

    type LeaseKeepAliveStream = LeaseKeepAliveStream;

    async fn lease_keep_alive(
        &self,
        _request: Request<Streaming<LeaseKeepAliveRequest>>,
    ) -> Result<Response<Self::LeaseKeepAliveStream>, Status> {
        Err(Status::unknown("lease keep alive is not supported"))
    }

    async fn lease_time_to_live(
        &self,
        _request: Request<LeaseTimeToLiveRequest>,
    ) -> Result<Response<LeaseTimeToLiveResponse>, Status> {
        Err(Status::unknown("lease time to live is not supported"))
    }

    async fn lease_leases(
        &self,
        _request: Request<LeaseLeasesRequest>,
    ) -> Result<Response<LeaseLeasesResponse>, Status> {
        Err(Status::unknown("lease leases is not supported"))
    }
}
