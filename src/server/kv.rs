use tonic::{Request, Response, Status};

use crate::backend::{Backend, BackendError, KeyValue as BackendKv};
use crate::proto::etcdserverpb::*;
use crate::proto::etcdserverpb::kv_server::Kv;
use crate::proto::mvccpb;

use super::KvBridge;

fn response_header(rev: i64) -> Option<ResponseHeader> {
    Some(ResponseHeader {
        revision: rev,
        ..Default::default()
    })
}

fn to_proto_kv(kv: &BackendKv) -> mvccpb::KeyValue {
    mvccpb::KeyValue {
        key: kv.key.as_bytes().to_vec(),
        value: kv.value.clone(),
        create_revision: kv.create_revision,
        mod_revision: kv.mod_revision,
        version: kv.version,
        lease: kv.lease,
    }
}

fn to_proto_kvs(kv: Option<&BackendKv>) -> Vec<mvccpb::KeyValue> {
    kv.map(|kv| vec![to_proto_kv(kv)]).unwrap_or_default()
}

fn backend_err_to_status(e: BackendError) -> Status {
    match e {
        BackendError::KeyExists => Status::failed_precondition(e.to_string()),
        BackendError::Compacted => Status::out_of_range(e.to_string()),
        BackendError::FutureRev => Status::out_of_range(e.to_string()),
        BackendError::Internal(msg) => Status::internal(msg),
    }
}

/// Detect the Create transaction pattern:
/// Compare: single MOD == 0 or VERSION == 0
/// Success: Put, optionally followed by Range ops
/// Failure: empty
fn is_create(txn: &TxnRequest) -> Option<&PutRequest> {
    if txn.compare.len() == 1
        && txn.compare[0].result() == compare::CompareResult::Equal
        && ((txn.compare[0].target() == compare::CompareTarget::Mod
            && txn.compare[0].mod_revision() == 0)
            || (txn.compare[0].target() == compare::CompareTarget::Version
                && txn.compare[0].version() == 0))
        && txn.failure.is_empty()
        && txn.success.len() >= 1
    {
        // First op must be a Put
        let put = txn.success[0].request.as_ref().and_then(|r| match r {
            request_op::Request::RequestPut(put) => Some(put),
            _ => None,
        })?;
        // All remaining ops must be Range requests
        for op in &txn.success[1..] {
            match op.request.as_ref() {
                Some(request_op::Request::RequestRange(_)) => {}
                _ => return None,
            }
        }
        Some(put)
    } else {
        None
    }
}

/// Detect the Update transaction pattern:
/// Compare: single MOD == expected_rev
/// Success: Put, optionally followed by Range ops
/// Failure: single Range
fn is_update(txn: &TxnRequest) -> Option<(i64, &[u8], &[u8], i64)> {
    if txn.compare.len() == 1
        && txn.compare[0].target() == compare::CompareTarget::Mod
        && txn.compare[0].result() == compare::CompareResult::Equal
        && txn.success.len() >= 1
        && txn.failure.len() == 1
    {
        let put = txn.success[0].request.as_ref().and_then(|r| match r {
            request_op::Request::RequestPut(put) => Some(put),
            _ => None,
        })?;
        // All remaining success ops must be Range requests
        for op in &txn.success[1..] {
            match op.request.as_ref() {
                Some(request_op::Request::RequestRange(_)) => {}
                _ => return None,
            }
        }
        // Verify failure is a Range
        txn.failure[0].request.as_ref().and_then(|r| match r {
            request_op::Request::RequestRange(_) => Some(()),
            _ => None,
        })?;
        Some((
            txn.compare[0].mod_revision(),
            &txn.compare[0].key,
            &put.value,
            put.lease,
        ))
    } else {
        None
    }
}

/// Detect the Delete transaction pattern. Two forms:
/// 1) No compare, success = [Range, DeleteRange], failure = empty
/// 2) Compare: MOD == rev, success = [DeleteRange], failure = [Range]
fn is_delete(txn: &TxnRequest) -> Option<(i64, &[u8])> {
    // Form 1: unconditional delete
    if txn.compare.is_empty()
        && txn.failure.is_empty()
        && txn.success.len() == 2
    {
        let is_range = txn.success[0].request.as_ref().is_some_and(|r| {
            matches!(r, request_op::Request::RequestRange(_))
        });
        let del = txn.success[1].request.as_ref().and_then(|r| match r {
            request_op::Request::RequestDeleteRange(del) => Some(del),
            _ => None,
        });
        if is_range
            && let Some(del) = del
        {
            return Some((0, &del.key));
        }
    }

    // Form 2: conditional delete
    if txn.compare.len() == 1
        && txn.compare[0].target() == compare::CompareTarget::Mod
        && txn.compare[0].result() == compare::CompareResult::Equal
        && txn.success.len() == 1
        && txn.failure.len() == 1
    {
        let del = txn.success[0].request.as_ref().and_then(|r| match r {
            request_op::Request::RequestDeleteRange(del) => Some(del),
            _ => None,
        })?;
        txn.failure[0].request.as_ref().and_then(|r| match r {
            request_op::Request::RequestRange(_) => Some(()),
            _ => None,
        })?;
        return Some((txn.compare[0].mod_revision(), &del.key));
    }

    None
}

/// Helper to access fields from a Compare's target_union.
trait CompareExt {
    fn mod_revision(&self) -> i64;
    fn version(&self) -> i64;
}

impl CompareExt for Compare {
    fn mod_revision(&self) -> i64 {
        match &self.target_union {
            Some(compare::TargetUnion::ModRevision(r)) => *r,
            _ => 0,
        }
    }

    fn version(&self) -> i64 {
        match &self.target_union {
            Some(compare::TargetUnion::Version(v)) => *v,
            _ => 0,
        }
    }
}

/// Extract Range requests from success ops after the first (Put) op.
fn extract_trailing_ranges(success: &[RequestOp]) -> Vec<RangeRequest> {
    success[1..]
        .iter()
        .filter_map(|op| match op.request.as_ref() {
            Some(request_op::Request::RequestRange(r)) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

impl<B: Backend> KvBridge<B> {
    async fn handle_range(
        &self,
        r: &RangeRequest,
    ) -> Result<RangeResponse, Status> {
        if r.range_end.is_empty() {
            self.handle_get(r).await
        } else {
            self.handle_list(r).await
        }
    }

    async fn handle_get(
        &self,
        r: &RangeRequest,
    ) -> Result<RangeResponse, Status> {
        let key = String::from_utf8_lossy(&r.key);
        let range_end = String::from_utf8_lossy(&r.range_end);

        let (rev, kv) = self
            .backend
            .get(&key, &range_end, r.limit, r.revision, r.keys_only)
            .await
            .map_err(backend_err_to_status)?;

        let kvs = to_proto_kvs(kv.as_ref());
        let count = kvs.len() as i64;
        Ok(RangeResponse {
            header: response_header(rev),
            kvs,
            count,
            more: false,
        })
    }

    async fn handle_list(
        &self,
        r: &RangeRequest,
    ) -> Result<RangeResponse, Status> {
        let range_end = &r.range_end;
        let prefix = {
            let mut p = range_end[..range_end.len() - 1].to_vec();
            if let Some(last) = p.last_mut() {
                *last = last.wrapping_sub(1);
            }
            let mut s = String::from_utf8_lossy(&p).to_string();
            if !s.ends_with('/') {
                s.push('/');
            }
            s
        };
        let start = String::from_utf8_lossy(&r.key).to_string();
        let revision = if r.revision > 0 { r.revision } else { 0 };

        if r.count_only {
            let (rev, count) = self
                .backend
                .count(&prefix, &start, revision)
                .await
                .map_err(backend_err_to_status)?;

            return Ok(RangeResponse {
                header: response_header(rev),
                kvs: vec![],
                count,
                more: false,
            });
        }

        let limit = if r.limit > 0 { r.limit + 1 } else { 0 };
        let (rev, kvs) = self
            .backend
            .list(&prefix, &start, limit, revision, r.keys_only)
            .await
            .map_err(backend_err_to_status)?;

        let proto_kvs: Vec<mvccpb::KeyValue> = kvs.iter().map(to_proto_kv).collect();
        let count = proto_kvs.len() as i64;

        let (kvs, more, count) = if r.limit > 0 && count > r.limit {
            let trimmed = proto_kvs[..r.limit as usize].to_vec();
            let rev_for_count = if revision == 0 { rev } else { revision };
            let (_, total_count) = self
                .backend
                .count(&prefix, &start, rev_for_count)
                .await
                .map_err(backend_err_to_status)?;
            (trimmed, true, total_count)
        } else {
            (proto_kvs, false, count)
        };

        Ok(RangeResponse {
            header: response_header(rev),
            kvs,
            count,
            more,
        })
    }

    async fn handle_create(
        &self,
        put: &PutRequest,
        trailing_ranges: &[RangeRequest],
    ) -> Result<TxnResponse, Status> {
        if put.ignore_lease {
            return Err(Status::unimplemented("ignoreLease is not implemented"));
        }
        if put.ignore_value {
            return Err(Status::unimplemented("ignoreValue is not implemented"));
        }
        if put.prev_kv {
            return Err(Status::unimplemented("prevKv is not implemented"));
        }

        let key = String::from_utf8_lossy(&put.key);

        match self.backend.create(&key, &put.value, put.lease).await {
            Ok(rev) => {
                let mut responses = vec![ResponseOp {
                    response: Some(response_op::Response::ResponsePut(PutResponse {
                        header: response_header(rev),
                        prev_kv: None,
                    })),
                }];
                for r in trailing_ranges {
                    let range_resp = self.handle_range(r).await?;
                    responses.push(ResponseOp {
                        response: Some(response_op::Response::ResponseRange(range_resp)),
                    });
                }
                Ok(TxnResponse {
                    header: response_header(rev),
                    succeeded: true,
                    responses,
                })
            }
            Err(BackendError::KeyExists) => {
                let rev = self
                    .backend
                    .current_revision()
                    .await
                    .map_err(backend_err_to_status)?;
                Ok(TxnResponse {
                    header: response_header(rev),
                    succeeded: false,
                    responses: vec![],
                })
            }
            Err(e) => Err(backend_err_to_status(e)),
        }
    }

    async fn handle_update(
        &self,
        rev: i64,
        key: &[u8],
        value: &[u8],
        lease: i64,
        trailing_ranges: &[RangeRequest],
    ) -> Result<TxnResponse, Status> {
        let key_str = String::from_utf8_lossy(key);

        if rev == 0 {
            // rev==0 means "create if not exists, get current if exists"
            match self.backend.create(&key_str, value, lease).await {
                Ok(new_rev) => {
                    let mut responses = vec![ResponseOp {
                        response: Some(response_op::Response::ResponsePut(
                            PutResponse {
                                header: response_header(new_rev),
                                prev_kv: None,
                            },
                        )),
                    }];
                    for r in trailing_ranges {
                        let range_resp = self.handle_range(r).await?;
                        responses.push(ResponseOp {
                            response: Some(response_op::Response::ResponseRange(range_resp)),
                        });
                    }
                    return Ok(TxnResponse {
                        header: response_header(new_rev),
                        succeeded: true,
                        responses,
                    });
                }
                Err(BackendError::KeyExists) => {
                    // Key already exists; fall through to get current value
                    let (current_rev, kv) = self
                        .backend
                        .get(&key_str, "", 1, 0, false)
                        .await
                        .map_err(backend_err_to_status)?;
                    let kvs = to_proto_kvs(kv.as_ref());
                    return Ok(TxnResponse {
                        header: response_header(current_rev),
                        succeeded: false,
                        responses: vec![ResponseOp {
                            response: Some(response_op::Response::ResponseRange(
                                RangeResponse {
                                    header: response_header(current_rev),
                                    kvs,
                                    count: 1,
                                    more: false,
                                },
                            )),
                        }],
                    });
                }
                Err(e) => return Err(backend_err_to_status(e)),
            }
        }

        let (new_rev, kv, ok) = self
            .backend
            .update(&key_str, value, rev, lease)
            .await
            .map_err(backend_err_to_status)?;

        if ok {
            let mut responses = vec![ResponseOp {
                response: Some(response_op::Response::ResponsePut(PutResponse {
                    header: response_header(new_rev),
                    prev_kv: None,
                })),
            }];
            for r in trailing_ranges {
                let range_resp = self.handle_range(r).await?;
                responses.push(ResponseOp {
                    response: Some(response_op::Response::ResponseRange(range_resp)),
                });
            }
            Ok(TxnResponse {
                header: response_header(new_rev),
                succeeded: true,
                responses,
            })
        } else {
            let kvs = to_proto_kvs(kv.as_ref());
            let count = kvs.len() as i64;
            Ok(TxnResponse {
                header: response_header(new_rev),
                succeeded: false,
                responses: vec![ResponseOp {
                    response: Some(response_op::Response::ResponseRange(
                        RangeResponse {
                            header: response_header(new_rev),
                            kvs,
                            count,
                            more: false,
                        },
                    )),
                }],
            })
        }
    }

    async fn handle_delete(
        &self,
        key: &[u8],
        revision: i64,
    ) -> Result<TxnResponse, Status> {
        let key_str = String::from_utf8_lossy(key);

        let (rev, kv, ok) = self
            .backend
            .delete(&key_str, revision)
            .await
            .map_err(backend_err_to_status)?;

        let kvs = to_proto_kvs(kv.as_ref());

        if ok {
            Ok(TxnResponse {
                header: response_header(rev),
                succeeded: true,
                responses: vec![ResponseOp {
                    response: Some(response_op::Response::ResponseDeleteRange(
                        DeleteRangeResponse {
                            header: response_header(rev),
                            prev_kvs: kvs.clone(),
                            deleted: kvs.len() as i64,
                        },
                    )),
                }],
            })
        } else {
            let count = kvs.len() as i64;
            Ok(TxnResponse {
                header: response_header(rev),
                succeeded: false,
                responses: vec![ResponseOp {
                    response: Some(response_op::Response::ResponseRange(
                        RangeResponse {
                            header: response_header(rev),
                            kvs,
                            count,
                            more: false,
                        },
                    )),
                }],
            })
        }
    }
}

#[tonic::async_trait]
impl<B: Backend> Kv for KvBridge<B> {
    async fn range(
        &self,
        request: Request<RangeRequest>,
    ) -> Result<Response<RangeResponse>, Status> {
        let r = request.into_inner();
        let resp = self.handle_range(&r).await?;
        Ok(Response::new(resp))
    }

    async fn put(
        &self,
        request: Request<PutRequest>,
    ) -> Result<Response<PutResponse>, Status> {
        let put = request.into_inner();
        let key = String::from_utf8_lossy(&put.key);

        // Try create first; if key exists, update unconditionally.
        match self.backend.create(&key, &put.value, put.lease).await {
            Ok(rev) => Ok(Response::new(PutResponse {
                header: response_header(rev),
                prev_kv: None,
            })),
            Err(BackendError::KeyExists) => {
                // Get current revision so we can do an unconditional update.
                let (_, existing) = self
                    .backend
                    .get(&key, "", 1, 0, false)
                    .await
                    .map_err(backend_err_to_status)?;
                let mod_rev = existing.as_ref().map(|kv| kv.mod_revision).unwrap_or(0);
                let prev_kv = if put.prev_kv {
                    existing.as_ref().map(to_proto_kv)
                } else {
                    None
                };
                let (new_rev, _, _) = self
                    .backend
                    .update(&key, &put.value, mod_rev, put.lease)
                    .await
                    .map_err(backend_err_to_status)?;
                Ok(Response::new(PutResponse {
                    header: response_header(new_rev),
                    prev_kv,
                }))
            }
            Err(e) => Err(backend_err_to_status(e)),
        }
    }

    async fn delete_range(
        &self,
        request: Request<DeleteRangeRequest>,
    ) -> Result<Response<DeleteRangeResponse>, Status> {
        let req = request.into_inner();
        let key = String::from_utf8_lossy(&req.key);

        let (rev, kv, ok) = self
            .backend
            .delete(&key, 0)
            .await
            .map_err(backend_err_to_status)?;

        let deleted = if ok { 1 } else { 0 };
        let prev_kvs = if req.prev_kv {
            to_proto_kvs(kv.as_ref())
        } else {
            vec![]
        };
        Ok(Response::new(DeleteRangeResponse {
            header: response_header(rev),
            deleted,
            prev_kvs,
        }))
    }

    async fn txn(
        &self,
        request: Request<TxnRequest>,
    ) -> Result<Response<TxnResponse>, Status> {
        let txn = request.into_inner();

        if let Some(put) = is_create(&txn) {
            let put = put.clone();
            let trailing = extract_trailing_ranges(&txn.success);
            return Ok(Response::new(
                self.handle_create(&put, &trailing).await?,
            ));
        }
        if let Some((rev, key, value, lease)) = is_update(&txn) {
            let key = key.to_vec();
            let value = value.to_vec();
            let trailing = extract_trailing_ranges(&txn.success);
            return Ok(Response::new(
                self.handle_update(rev, &key, &value, lease, &trailing)
                    .await?,
            ));
        }
        if let Some((rev, key)) = is_delete(&txn) {
            let key = key.to_vec();
            return Ok(Response::new(self.handle_delete(&key, rev).await?));
        }

        Err(Status::invalid_argument(
            "unsupported transaction pattern",
        ))
    }

    async fn compact(
        &self,
        request: Request<CompactionRequest>,
    ) -> Result<Response<CompactionResponse>, Status> {
        let r = request.into_inner();
        let rev = self
            .backend
            .compact(r.revision)
            .await
            .map_err(backend_err_to_status)?;
        Ok(Response::new(CompactionResponse {
            header: response_header(rev),
        }))
    }
}
