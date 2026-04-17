use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};

use tokio::sync::mpsc;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

use crate::backend::{Backend, BackendError};
use crate::proto::etcdserverpb::*;
use crate::proto::etcdserverpb::watch_server::Watch;
use crate::proto::mvccpb;

use super::KvBridge;

/// Global watch ID counter — matches kine's globally unique watch IDs.
static WATCH_ID_COUNTER: AtomicI64 = AtomicI64::new(1);

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

        // Channel for sending responses back to the client.
        // All spawned watch tasks send into this channel.
        let (resp_tx, resp_rx) = mpsc::channel::<Result<WatchResponse, Status>>(256);

        // Spawn a task that reads incoming requests and dispatches watches.
        tokio::spawn(async move {
            // Track active watch cancellation handles
            let mut cancels: HashMap<i64, tokio::sync::oneshot::Sender<()>> = HashMap::new();

            while let Some(req) = in_stream.next().await {
                let req = match req {
                    Ok(r) => r,
                    Err(_) => break,
                };

                match req.request_union {
                    Some(watch_request::RequestUnion::CreateRequest(create)) => {
                        let watch_id = WATCH_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
                        let key = String::from_utf8_lossy(&create.key).to_string();
                        let start_revision = create.start_revision;

                        // Send created confirmation
                        let _ = resp_tx.send(Ok(WatchResponse {
                            header: Some(ResponseHeader::default()),
                            watch_id,
                            created: true,
                            ..Default::default()
                        })).await;

                        // Create cancellation channel
                        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
                        cancels.insert(watch_id, cancel_tx);

                        // Spawn an independent task for this watch
                        let backend = backend.clone();
                        let resp_tx = resp_tx.clone();
                        tokio::spawn(async move {
                            let watch_result = match backend.watch(&key, start_revision).await {
                                Ok(wr) => wr,
                                Err(BackendError::Compacted) => {
                                    let _ = resp_tx.send(Ok(WatchResponse {
                                        header: Some(ResponseHeader::default()),
                                        watch_id,
                                        canceled: true,
                                        compact_revision: start_revision,
                                        cancel_reason: "compacted".to_string(),
                                        ..Default::default()
                                    })).await;
                                    return;
                                }
                                Err(e) => {
                                    let _ = resp_tx.send(Err(backend_err_to_status(e))).await;
                                    return;
                                }
                            };

                            let mut events_rx = watch_result.events;
                            let mut cancel_rx = cancel_rx;

                            loop {
                                tokio::select! {
                                    event_batch = events_rx.recv() => {
                                        let Some(events) = event_batch else {
                                            break; // Channel closed
                                        };

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

                                        if resp_tx.send(Ok(WatchResponse {
                                            header: Some(ResponseHeader {
                                                revision: last_rev,
                                                ..Default::default()
                                            }),
                                            watch_id,
                                            events: proto_events,
                                            ..Default::default()
                                        })).await.is_err() {
                                            break; // Client disconnected
                                        }
                                    }
                                    _ = &mut cancel_rx => {
                                        break; // Watch cancelled
                                    }
                                }
                            }
                        });
                    }
                    Some(watch_request::RequestUnion::CancelRequest(cancel)) => {
                        // Signal the watch task to stop
                        if let Some(cancel_tx) = cancels.remove(&cancel.watch_id) {
                            let _ = cancel_tx.send(());
                        }
                        let _ = resp_tx.send(Ok(WatchResponse {
                            header: Some(ResponseHeader::default()),
                            watch_id: cancel.watch_id,
                            canceled: true,
                            cancel_reason: "watch cancelled by client".to_string(),
                            ..Default::default()
                        })).await;
                    }
                    Some(watch_request::RequestUnion::ProgressRequest(_)) => {
                        let rev = backend
                            .current_revision()
                            .await
                            .map_err(backend_err_to_status);

                        let resp = match rev {
                            Ok(rev) => Ok(WatchResponse {
                                header: Some(ResponseHeader {
                                    revision: rev,
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }),
                            Err(e) => Err(e),
                        };
                        let _ = resp_tx.send(resp).await;
                    }
                    None => {}
                }
            }
        });

        // Convert the mpsc receiver into a stream for tonic
        let output = tokio_stream::wrappers::ReceiverStream::new(resp_rx);
        Ok(Response::new(Box::pin(output) as Self::WatchStream))
    }
}
