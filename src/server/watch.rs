use std::pin::Pin;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

use crate::backend::{Backend, BackendError};
use crate::proto::etcdserverpb::*;
use crate::proto::etcdserverpb::watch_server::Watch;
use crate::proto::mvccpb;

use super::KvBridge;

type WatchResponseStream = Pin<Box<dyn Stream<Item = Result<WatchResponse, Status>> + Send>>;

fn backend_err_to_status(e: BackendError) -> Status {
    match e {
        BackendError::Compacted => Status::out_of_range(e.to_string()),
        _ => Status::internal(e.to_string()),
    }
}

#[tonic::async_trait]
impl<B: Backend> Watch for KvBridge<B> {
    type WatchStream = WatchResponseStream;

    async fn watch(
        &self,
        request: Request<Streaming<WatchRequest>>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let mut in_stream = request.into_inner();
        let backend = self.backend.clone();

        let output: WatchResponseStream = Box::pin(async_stream::try_stream! {
            while let Some(req) = in_stream.next().await {
                let req = req?;

                match req.request_union {
                    Some(watch_request::RequestUnion::CreateRequest(create)) => {
                        let key = String::from_utf8_lossy(&create.key).to_string();
                        let watch_id = create.watch_id;

                        // Send the created response
                        yield WatchResponse {
                            header: Some(ResponseHeader::default()),
                            watch_id,
                            created: true,
                            ..Default::default()
                        };

                        // Start watching
                        let mut watch_result = backend
                            .watch(&key, create.start_revision)
                            .await
                            .map_err(backend_err_to_status)?;

                        while let Some(events) = watch_result.events.recv().await {
                            let proto_events: Vec<mvccpb::Event> = events
                                .iter()
                                .map(|e| {
                                    let event_type = if e.delete {
                                        mvccpb::event::EventType::Delete
                                    } else {
                                        mvccpb::event::EventType::Put
                                    };
                                    mvccpb::Event {
                                        r#type: event_type.into(),
                                        kv: Some(mvccpb::KeyValue {
                                            key: e.kv.key.as_bytes().to_vec(),
                                            value: e.kv.value.clone(),
                                            create_revision: e.kv.create_revision,
                                            mod_revision: e.kv.mod_revision,
                                            version: e.kv.version,
                                            lease: e.kv.lease,
                                        }),
                                        prev_kv: e.prev_kv.as_ref().map(|pk| mvccpb::KeyValue {
                                            key: pk.key.as_bytes().to_vec(),
                                            value: pk.value.clone(),
                                            create_revision: pk.create_revision,
                                            mod_revision: pk.mod_revision,
                                            version: pk.version,
                                            lease: pk.lease,
                                        }),
                                    }
                                })
                                .collect();

                            let last_rev = events
                                .last()
                                .map(|e| e.kv.mod_revision)
                                .unwrap_or(0);

                            yield WatchResponse {
                                header: Some(ResponseHeader {
                                    revision: last_rev,
                                    ..Default::default()
                                }),
                                watch_id,
                                events: proto_events,
                                ..Default::default()
                            };
                        }
                    }
                    Some(watch_request::RequestUnion::CancelRequest(cancel)) => {
                        yield WatchResponse {
                            header: Some(ResponseHeader::default()),
                            watch_id: cancel.watch_id,
                            canceled: true,
                            cancel_reason: "watch cancelled by client".to_string(),
                            ..Default::default()
                        };
                    }
                    Some(watch_request::RequestUnion::ProgressRequest(_)) => {
                        let rev = backend
                            .current_revision()
                            .await
                            .map_err(backend_err_to_status)?;

                        yield WatchResponse {
                            header: Some(ResponseHeader {
                                revision: rev,
                                ..Default::default()
                            }),
                            ..Default::default()
                        };
                    }
                    None => {}
                }
            }
        });

        Ok(Response::new(output))
    }
}
