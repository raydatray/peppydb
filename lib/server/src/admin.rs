//! The `AdminService` service: status, checkpoints, and snapshots.

use std::io::{self, Write};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::Stream;
use kv_proto::kv::v1::admin_service_server::AdminService;
use kv_proto::kv::v1::snapshot_response::Payload;
use kv_proto::kv::v1::{
    CheckpointRequest, CheckpointResponse, SnapshotRequest, SnapshotResponse, SnapshotTrailer,
    StatusRequest, StatusResponse,
};
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

use crate::VERSION;
use crate::bridge::Inner;

/// Bounded chunks in flight between the blocking `write_snapshot` call and the
/// gRPC stream. Provides backpressure against slow clients.
const SNAPSHOT_CHANNEL_CAPACITY: usize = 4;

pub(crate) struct Admin {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct SnapshotStream {
    rx: mpsc::Receiver<Result<SnapshotResponse, Status>>,
}

impl Stream for SnapshotStream {
    type Item = Result<SnapshotResponse, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

/// Bridges `Db::write_snapshot`'s synchronous `io::Write` to the bounded gRPC
/// response channel, tracking total bytes and a running Castagnoli CRC32C.
struct ChannelWriter {
    tx: mpsc::Sender<Result<SnapshotResponse, Status>>,
    chunk_size: usize,
    buf: Vec<u8>,
    crc: u32,
    total_bytes: u64,
    cancelled: bool,
}

impl ChannelWriter {
    fn new(tx: mpsc::Sender<Result<SnapshotResponse, Status>>, chunk_size: usize) -> Self {
        Self {
            tx,
            chunk_size,
            buf: Vec::with_capacity(chunk_size),
            crc: 0,
            total_bytes: 0,
            cancelled: false,
        }
    }

    fn send_chunk(&mut self, chunk: Vec<u8>) -> io::Result<()> {
        self.crc = crc32c::crc32c_append(self.crc, &chunk);
        self.total_bytes = self.total_bytes.saturating_add(chunk.len() as u64);
        self.tx
            .blocking_send(Ok(SnapshotResponse {
                payload: Some(Payload::Chunk(chunk)),
            }))
            .map_err(|_| {
                self.cancelled = true;
                io::Error::new(io::ErrorKind::BrokenPipe, "snapshot client disconnected")
            })
    }

    fn finish(mut self, checkpoint: engine::Checkpoint) {
        if !self.buf.is_empty() {
            let chunk = std::mem::take(&mut self.buf);
            if self.send_chunk(chunk).is_err() {
                return;
            }
        }
        let trailer = SnapshotTrailer {
            applied_seq: checkpoint.applied_seq,
            checkpoint_id: checkpoint.checkpoint_id,
            total_bytes: self.total_bytes,
            crc32c: self.crc,
        };
        let _ = self.tx.blocking_send(Ok(SnapshotResponse {
            payload: Some(Payload::Trailer(trailer)),
        }));
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, mut data: &[u8]) -> io::Result<usize> {
        let len = data.len();
        while !data.is_empty() {
            let space = self.chunk_size - self.buf.len();
            let take = space.min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() == self.chunk_size {
                let chunk = std::mem::replace(&mut self.buf, Vec::with_capacity(self.chunk_size));
                self.send_chunk(chunk)?;
            }
        }
        Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tonic::async_trait]
impl AdminService for Admin {
    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let applied_seq = self.inner.blocking(|db| Ok(db.applied_seq())).await?;
        Ok(Response::new(StatusResponse {
            applied_seq,
            version: VERSION.to_owned(),
        }))
    }

    async fn checkpoint(
        &self,
        _request: Request<CheckpointRequest>,
    ) -> Result<Response<CheckpointResponse>, Status> {
        let checkpoint = self.inner.blocking(|db| db.checkpoint()).await?;
        Ok(Response::new(CheckpointResponse {
            applied_seq: checkpoint.applied_seq,
            checkpoint_id: checkpoint.checkpoint_id,
        }))
    }

    type SnapshotStream = SnapshotStream;

    async fn snapshot(
        &self,
        request: Request<SnapshotRequest>,
    ) -> Result<Response<Self::SnapshotStream>, Status> {
        let checkpoint_first = request.into_inner().checkpoint;
        let chunk_size = self.inner.config.snapshot_chunk_bytes;
        let (tx, rx) = mpsc::channel(SNAPSHOT_CHANNEL_CAPACITY);
        let inner = Arc::clone(&self.inner);
        self.inner
            .spawn_snapshot(move |db| {
                if checkpoint_first && let Err(error) = db.checkpoint() {
                    let _ = tx.blocking_send(Err(inner.engine_status(error)));
                    return;
                }
                let mut writer = ChannelWriter::new(tx.clone(), chunk_size);
                match db.write_snapshot(&mut writer) {
                    Ok(checkpoint) => writer.finish(checkpoint),
                    Err(_) if writer.cancelled => {
                        // The client dropped the stream; stop cleanly.
                    }
                    Err(error) => {
                        let _ = tx.blocking_send(Err(inner.engine_status(error)));
                    }
                }
            })
            .await?;
        Ok(Response::new(SnapshotStream { rx }))
    }
}
