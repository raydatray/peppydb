//! The `AdminService` service: status, checkpoints, and snapshots.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::Stream;
use kv_proto::kv::v1::admin_service_server::AdminService;
use kv_proto::kv::v1::{
    CheckpointRequest, CheckpointResponse, SnapshotRequest, SnapshotResponse, StatusRequest,
    StatusResponse,
};
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

use crate::VERSION;
use crate::bridge::Inner;

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
        _request: Request<SnapshotRequest>,
    ) -> Result<Response<Self::SnapshotStream>, Status> {
        let (_tx, _rx) = mpsc::channel::<Result<SnapshotResponse, Status>>(1);
        Err(Status::unimplemented("snapshot is not implemented yet"))
    }
}
