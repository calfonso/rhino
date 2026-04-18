use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};
use tracing::{error, trace};

use crate::backend::{Backend, BackendError};
use crate::proto::etcdserverpb::*;
use crate::proto::etcdserverpb::watch_server::Watch;
use crate::proto::mvccpb;

use super::KvBridge;

/// Global watch ID counter — matches kine's globally unique watch IDs.
static WATCH_ID_COUNTER: AtomicI64 = AtomicI64::new(1);

/// How often to check if a broadcast progress response should be sent.
/// Matches kine's `progressResponsePeriod` (watch.go:19).
const PROGRESS_ALL_INTERVAL: Duration = Duration::from_millis(100);

type WatchResponseStream = Pin<Box<dyn Stream<Item = Result<WatchResponse, Status>> + Send>>;

fn backend_err_to_status(e: BackendError) -> Status {
    match e {
        BackendError::Compacted => Status::out_of_range(e.to_string()),
        _ => Status::internal(e.to_string()),
    }
}

/// Shared mutable state for a Watch stream, holding per-watch cancel and progress handles.
struct WatcherState {
    cancels: HashMap<i64, tokio::sync::oneshot::Sender<()>>,
    /// Progress channels for watches created with `progress_notify=true`.
    /// Capacity-1 mpsc channels: `try_send` failing with Full means the watcher is not synced.
    progress: HashMap<i64, mpsc::Sender<i64>>,
}

/// Background task: broadcast progress if all watchers are synced (every 100ms).
/// Matches kine's `ProgressAll` (watch.go:344-378).
async fn progress_all<B: Backend>(
    backend: Arc<B>,
    resp_tx: mpsc::Sender<Result<WatchResponse, Status>>,
    notify: Arc<AtomicBool>,
    state: Arc<tokio::sync::Mutex<WatcherState>>,
) {
    let mut interval = tokio::time::interval(PROGRESS_ALL_INTERVAL);

    loop {
        interval.tick().await;

        if resp_tx.is_closed() {
            break;
        }

        if !notify.load(Ordering::Acquire) {
            continue;
        }

        let rev = match backend.current_revision().await {
            Ok(r) => r,
            Err(e) => {
                error!("progress_all: failed to get revision: {e}");
                continue;
            }
        };

        // Wait for poll loop to catch up
        backend.wait_for_sync_to(rev).await;

        let state = state.lock().await;

        // Try to send rev 0 (sync check) to ALL progress channels.
        // If any channel is full, that watcher isn't synced — abort.
        let mut all_synced = true;
        for (id, tx) in &state.progress {
            match tx.try_send(0) {
                Ok(()) => {}
                Err(_) => {
                    trace!("progress_all: watcher {id} not synced");
                    all_synced = false;
                    break;
                }
            }
        }

        if !all_synced {
            continue;
        }

        // All synced — clear flag and send broadcast progress response.
        // watch_id = -1 matches kine's clientv3.InvalidWatchID.
        notify.store(false, Ordering::Release);

        let _ = resp_tx
            .send(Ok(WatchResponse {
                header: Some(ResponseHeader {
                    revision: rev,
                    ..Default::default()
                }),
                watch_id: -1,
                ..Default::default()
            }))
            .await;
    }
}

/// Background task: per-watcher progress if synced (every ~5s + jitter).
/// Matches kine's `ProgressIfSynced` (watch.go:380-407).
async fn progress_if_synced<B: Backend>(
    backend: Arc<B>,
    resp_tx: mpsc::Sender<Result<WatchResponse, Status>>,
    notify: Arc<AtomicBool>,
    state: Arc<tokio::sync::Mutex<WatcherState>>,
    notify_interval: Duration,
) {
    // Add jitter: rand(1/10 * notify_interval), matching kine's getProgressReportInterval.
    let jitter_range_ms = notify_interval.as_millis() as u64 / 10;
    let jitter_ms = if jitter_range_ms > 0 {
        (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64)
            % jitter_range_ms
    } else {
        0
    };
    let interval_with_jitter = notify_interval + Duration::from_millis(jitter_ms);
    let mut interval = tokio::time::interval(interval_with_jitter);

    loop {
        interval.tick().await;

        if resp_tx.is_closed() {
            break;
        }

        // Don't send individual progress if broadcast progress has been requested.
        // Avoids double-progress (one broadcast from ProgressAll, another from this timer).
        if notify.load(Ordering::Acquire) {
            continue;
        }

        let rev = match backend.current_revision().await {
            Ok(r) => r,
            Err(e) => {
                error!("progress_if_synced: failed to get revision: {e}");
                continue;
            }
        };

        backend.wait_for_sync_to(rev).await;

        let state = state.lock().await;

        // Send actual revision to all synced channels (non-blocking).
        // If full, watcher has pending events — skip it.
        for (_id, tx) in &state.progress {
            let _ = tx.try_send(rev);
        }
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
        let notify_interval = self.notify_interval;

        // Channel for sending responses back to the client.
        let (resp_tx, resp_rx) = mpsc::channel::<Result<WatchResponse, Status>>(256);

        // Shared state for this Watch stream
        let notify = Arc::new(AtomicBool::new(false));
        let state = Arc::new(tokio::sync::Mutex::new(WatcherState {
            cancels: HashMap::new(),
            progress: HashMap::new(),
        }));

        // Spawn progress_all background task (100ms interval)
        tokio::spawn(progress_all(
            backend.clone(),
            resp_tx.clone(),
            notify.clone(),
            state.clone(),
        ));

        // Spawn progress_if_synced background task (notify_interval + jitter)
        tokio::spawn(progress_if_synced(
            backend.clone(),
            resp_tx.clone(),
            notify.clone(),
            state.clone(),
            notify_interval,
        ));

        // Spawn inbound request handler
        tokio::spawn(async move {
            while let Some(req) = in_stream.next().await {
                let req = match req {
                    Ok(r) => r,
                    Err(_) => break,
                };

                match req.request_union {
                    Some(watch_request::RequestUnion::CreateRequest(create)) => {
                        // Reject client-provided watch IDs (kine only accepts AutoWatchID = 0)
                        if create.watch_id != 0 {
                            let _ = resp_tx.send(Ok(WatchResponse {
                                header: Some(ResponseHeader::default()),
                                watch_id: -1, // InvalidWatchID
                                created: true,
                                canceled: true,
                                cancel_reason: "etcdserver: unsupported options in watch request".to_string(),
                                ..Default::default()
                            })).await;
                            continue;
                        }

                        let watch_id = WATCH_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
                        let raw_key = String::from_utf8_lossy(&create.key).to_string();
                        // Redirect compact_rev_key to internal key
                        let key = if raw_key == "compact_rev_key" {
                            "compact_rev_key_apiserver".to_string()
                        } else {
                            raw_key
                        };
                        let start_revision = create.start_revision;

                        // Reject negative start revision (kine sends ErrCompacted)
                        if start_revision < 0 {
                            let _ = resp_tx.send(Ok(WatchResponse {
                                header: Some(ResponseHeader::default()),
                                watch_id,
                                canceled: true,
                                compact_revision: start_revision,
                                cancel_reason: "compacted".to_string(),
                                ..Default::default()
                            })).await;
                            continue;
                        }

                        // Create cancellation channel
                        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();

                        // Create progress channel if requested
                        let progress_rx = if create.progress_notify {
                            let (ptx, prx) = mpsc::channel::<i64>(1);
                            state.lock().await.progress.insert(watch_id, ptx);
                            Some(prx)
                        } else {
                            None
                        };

                        state.lock().await.cancels.insert(watch_id, cancel_tx);

                        // Send created confirmation
                        let _ = resp_tx.send(Ok(WatchResponse {
                            header: Some(ResponseHeader::default()),
                            watch_id,
                            created: true,
                            ..Default::default()
                        })).await;

                        // Spawn an independent task for this watch
                        let backend = backend.clone();
                        let resp_tx = resp_tx.clone();
                        tokio::spawn(async move {
                            let watch_result = match backend.watch(&key, start_revision).await {
                                Ok(wr) => {
                                    // Check if watch result indicates compacted revision
                                    // (matching kine watch.go:160-166)
                                    if wr.compact_revision != 0 {
                                        let _ = resp_tx.send(Ok(WatchResponse {
                                            header: Some(ResponseHeader {
                                                revision: wr.current_revision,
                                                ..Default::default()
                                            }),
                                            watch_id,
                                            canceled: true,
                                            compact_revision: wr.compact_revision,
                                            cancel_reason: "compacted".to_string(),
                                            ..Default::default()
                                        })).await;
                                        return;
                                    }
                                    wr
                                }
                                Err(BackendError::Compacted) => {
                                    // Backend returned compacted error — get actual revisions
                                    let current_rev = backend.current_revision().await.unwrap_or(0);
                                    let _ = resp_tx.send(Ok(WatchResponse {
                                        header: Some(ResponseHeader {
                                            revision: current_rev,
                                            ..Default::default()
                                        }),
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
                            let has_progress = progress_rx.is_some();
                            let mut progress_rx = progress_rx;

                            loop {
                                tokio::select! {
                                    event_batch = events_rx.recv() => {
                                        let Some(mut events) = event_batch else {
                                            break; // Channel closed
                                        };

                                        // Coalesce queued batches (matching kine's event batching)
                                        while let Ok(more) = events_rx.try_recv() {
                                            events.extend(more);
                                        }

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

                                        if last_rev >= start_revision || start_revision == 0 {
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
                                    }
                                    Some(revision) = async {
                                        if has_progress {
                                            progress_rx.as_mut().unwrap().recv().await
                                        } else {
                                            // Never resolves — effectively disabled
                                            std::future::pending::<Option<i64>>().await
                                        }
                                    } => {
                                        // Progress notification request.
                                        // revision == 0: "sync check" — the fact that we received
                                        // means we're synced (not blocked on events). No response.
                                        // revision > 0: send a progress response with this revision.
                                        if revision > 0 {
                                            if resp_tx.send(Ok(WatchResponse {
                                                header: Some(ResponseHeader {
                                                    revision,
                                                    ..Default::default()
                                                }),
                                                watch_id,
                                                ..Default::default()
                                            })).await.is_err() {
                                                break;
                                            }
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
                        let mut s = state.lock().await;
                        // Signal the watch task to stop
                        if let Some(cancel_tx) = s.cancels.remove(&cancel.watch_id) {
                            let _ = cancel_tx.send(());
                        }
                        // Clean up progress channel
                        s.progress.remove(&cancel.watch_id);
                        drop(s);

                        let _ = resp_tx.send(Ok(WatchResponse {
                            header: Some(ResponseHeader::default()),
                            watch_id: cancel.watch_id,
                            canceled: true,
                            cancel_reason: "watch cancelled by client".to_string(),
                            ..Default::default()
                        })).await;
                    }
                    Some(watch_request::RequestUnion::ProgressRequest(_)) => {
                        // Set flag for ProgressAll background task to handle.
                        // Matches kine's Progress() (watch.go:335-340).
                        trace!("watch progress request received");
                        notify.store(true, Ordering::Release);
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
