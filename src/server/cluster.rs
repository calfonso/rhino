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
        _request: Request<MemberListRequest>,
    ) -> Result<Response<MemberListResponse>, Status> {
        Ok(Response::new(MemberListResponse {
            header: Some(ResponseHeader::default()),
            members: vec![Member {
                id: 1,
                name: "rhino".to_string(),
                peer_ur_ls: vec!["http://127.0.0.1:2379".to_string()],
                client_ur_ls: vec!["http://127.0.0.1:2379".to_string()],
                is_learner: false,
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
