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
        // Rhino doesn't implement full lease management.
        // Return the requested TTL/ID back as-is (kine does the same).
        Ok(Response::new(LeaseGrantResponse {
            header: Some(ResponseHeader::default()),
            id: r.id,
            ttl: r.ttl,
            error: String::new(),
        }))
    }

    async fn lease_revoke(
        &self,
        _request: Request<LeaseRevokeRequest>,
    ) -> Result<Response<LeaseRevokeResponse>, Status> {
        Ok(Response::new(LeaseRevokeResponse {
            header: Some(ResponseHeader::default()),
        }))
    }

    type LeaseKeepAliveStream = LeaseKeepAliveStream;

    async fn lease_keep_alive(
        &self,
        _request: Request<Streaming<LeaseKeepAliveRequest>>,
    ) -> Result<Response<Self::LeaseKeepAliveStream>, Status> {
        // Minimal keepalive: echo back the request IDs.
        let in_stream = _request.into_inner();
        let output = async_stream::try_stream! {
            let mut stream = in_stream;
            while let Some(req) = tokio_stream::StreamExt::next(&mut stream).await {
                let req = req?;
                yield LeaseKeepAliveResponse {
                    header: Some(ResponseHeader::default()),
                    id: req.id,
                    ttl: 30, // default TTL
                };
            }
        };
        Ok(Response::new(Box::pin(output) as Self::LeaseKeepAliveStream))
    }

    async fn lease_time_to_live(
        &self,
        request: Request<LeaseTimeToLiveRequest>,
    ) -> Result<Response<LeaseTimeToLiveResponse>, Status> {
        let r = request.into_inner();
        Ok(Response::new(LeaseTimeToLiveResponse {
            header: Some(ResponseHeader::default()),
            id: r.id,
            ttl: 30,
            granted_ttl: 30,
            keys: vec![],
        }))
    }

    async fn lease_leases(
        &self,
        _request: Request<LeaseLeasesRequest>,
    ) -> Result<Response<LeaseLeasesResponse>, Status> {
        Ok(Response::new(LeaseLeasesResponse {
            header: Some(ResponseHeader::default()),
            leases: vec![],
        }))
    }
}
