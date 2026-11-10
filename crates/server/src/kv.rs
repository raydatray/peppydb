//! The `KvService` service: point reads and writes.

use std::sync::Arc;

use server_proto::kv::v1::kv_service_server::KvService;
use server_proto::kv::v1::{
    DeleteRequest, DeleteResponse, GetRequest, GetResponse, PutRequest, PutResponse, ScanRequest,
    ScanResponse,
};
use tonic::{Request, Response, Status};

use crate::bridge::Inner;
use crate::keys::user_key;

pub(crate) struct Kv {
    pub(crate) inner: Arc<Inner>,
}

#[tonic::async_trait]
impl KvService for Kv {
    async fn get(&self, request: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        let key = user_key(&request.into_inner().key)?;
        let value = self.inner.blocking(move |db| db.get(&key)).await?;
        Ok(Response::new(GetResponse { value }))
    }

    async fn put(&self, request: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        let PutRequest { key, value } = request.into_inner();
        let key = user_key(&key)?;
        self.inner.blocking(move |db| db.put(&key, &value)).await?;
        Ok(Response::new(PutResponse {}))
    }

    async fn delete(
        &self,
        request: Request<DeleteRequest>,
    ) -> Result<Response<DeleteResponse>, Status> {
        let key = user_key(&request.into_inner().key)?;
        self.inner.blocking(move |db| db.delete(&key)).await?;
        Ok(Response::new(DeleteResponse {}))
    }

    async fn scan(&self, _request: Request<ScanRequest>) -> Result<Response<ScanResponse>, Status> {
        Err(Status::unimplemented("scans arrive in the next diff"))
    }
}
