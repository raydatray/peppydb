//! The `KvService` service: point reads and writes.

use std::sync::Arc;

use kv_proto::kv::v1::kv_service_server::KvService;
use kv_proto::kv::v1::{
    DeleteRequest, DeleteResponse, GetRequest, GetResponse, KeyValue, PutRequest, PutResponse,
    ScanRequest, ScanResponse,
};
use tonic::{Request, Response, Status};

use crate::bridge::Inner;
use crate::keys::{strip, user_key, user_range};

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

    /// One page of `[start, end)`: at most `page_size` records (the server
    /// maximum if 0 or larger), and no more once the page reaches
    /// `scan_max_bytes`. `next_start` is the first key not returned.
    async fn scan(&self, request: Request<ScanRequest>) -> Result<Response<ScanResponse>, Status> {
        let ScanRequest {
            start,
            end,
            page_size,
        } = request.into_inner();
        let config = self.inner.config;
        let limit = match usize::try_from(page_size).unwrap_or(usize::MAX) {
            0 => config.scan_max_records,
            n => n.min(config.scan_max_records),
        };
        let page = self
            .inner
            .blocking(move |db| {
                let (start, end) = user_range(start.as_deref(), end.as_deref());
                let mut records = Vec::new();
                let mut bytes = 0;
                for record in db.range(Some(&start), Some(&end))? {
                    let (key, value) = record?;
                    if records.len() == limit || bytes >= config.scan_max_bytes {
                        return Ok(ScanResponse {
                            records,
                            next_start: Some(strip(&key)),
                        });
                    }
                    let key = strip(&key);
                    bytes += key.len() + value.len();
                    records.push(KeyValue { key, value });
                }
                Ok(ScanResponse {
                    records,
                    next_start: None,
                })
            })
            .await?;
        Ok(Response::new(page))
    }
}
