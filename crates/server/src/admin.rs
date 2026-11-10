//! The `AdminService` service: status and checkpoints.

use std::sync::Arc;

use server_proto::kv::v1::admin_service_server::AdminService;
use server_proto::kv::v1::{CheckpointRequest, CheckpointResponse, StatusRequest, StatusResponse};
use tonic::{Request, Response, Status};

use crate::bridge::Inner;

/// The server's version, as Cargo or the build reports it.
const VERSION: &str = match option_env!("CARGO_PKG_VERSION") {
    Some(version) => version,
    None => "unknown",
};

pub(crate) struct Admin {
    pub(crate) inner: Arc<Inner>,
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
}
