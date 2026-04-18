use tonic::{Request, Response, Status};

use crate::backend::Backend;
use crate::proto::etcdserverpb::*;
use crate::proto::etcdserverpb::cluster_server::Cluster;

use super::KvBridge;

/// Extract the authority URL from gRPC metadata, matching kine's authorityURL
/// (cluster.go:48-64). Handles etcd v3.5's encoded address list format.
fn authority_url(request: &Request<MemberListRequest>) -> String {
    let scheme = "http";
    let authority = request
        .metadata()
        .get(":authority")
        .and_then(|v| v.to_str().ok());

    let Some(authority) = authority else {
        return format!("{scheme}://127.0.0.1:2379");
    };

    // etcd v3.5 encodes the endpoint address list as "#initially=[ADDRESS1;ADDRESS2]"
    if let Some(inner) = authority.strip_prefix("#initially=[") {
        let inner = inner.strip_suffix(']').unwrap_or(inner);
        return inner.replace(';', ",");
    }

    format!("{scheme}://{authority}")
}

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
        let listen_url = authority_url(&request);

        Ok(Response::new(MemberListResponse {
            header: Some(ResponseHeader::default()),
            members: vec![Member {
                name: "kine".to_string(),
                peer_ur_ls: vec![listen_url.clone()],
                client_ur_ls: vec![listen_url],
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
