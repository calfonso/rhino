use tonic::{Request, Response, Status};

use crate::backend::Backend;
use crate::proto::etcdserverpb::*;
use crate::proto::etcdserverpb::cluster_server::Cluster;

use super::KvBridge;

#[tonic::async_trait]
impl<B: Backend> Cluster for KvBridge<B> {
    async fn member_add(
        &self,
        _request: Request<MemberAddRequest>,
    ) -> Result<Response<MemberAddResponse>, Status> {
        Err(Status::unknown("member add is not supported"))
    }

    async fn member_remove(
        &self,
        _request: Request<MemberRemoveRequest>,
    ) -> Result<Response<MemberRemoveResponse>, Status> {
        Err(Status::unknown("member remove is not supported"))
    }

    async fn member_update(
        &self,
        _request: Request<MemberUpdateRequest>,
    ) -> Result<Response<MemberUpdateResponse>, Status> {
        Err(Status::unknown("member update is not supported"))
    }

    async fn member_list(
        &self,
        request: Request<MemberListRequest>,
    ) -> Result<Response<MemberListResponse>, Status> {
        // Extract authority from gRPC metadata, matching kine's behavior
        let authority = request
            .metadata()
            .get(":authority")
            .and_then(|v| v.to_str().ok())
            .map(|a| format!("http://{a}"))
            .unwrap_or_else(|| "http://127.0.0.1:2379".to_string());

        Ok(Response::new(MemberListResponse {
            header: Some(ResponseHeader::default()),
            members: vec![Member {
                name: "kine".to_string(),
                peer_ur_ls: vec![authority.clone()],
                client_ur_ls: vec![authority],
                is_learner: false,
                ..Default::default()
            }],
        }))
    }

    async fn member_promote(
        &self,
        _request: Request<MemberPromoteRequest>,
    ) -> Result<Response<MemberPromoteResponse>, Status> {
        Err(Status::unknown("member promote is not supported"))
    }
}
