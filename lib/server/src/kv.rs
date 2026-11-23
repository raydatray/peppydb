//! The `KvService` service: point reads and writes.

use std::sync::Arc;
use std::time::Instant;

use kv_proto::kv::v1::kv_service_server::KvService;
use kv_proto::kv::v1::{
    DeleteRequest, DeleteResponse, GetRequest, GetResponse, KeyValue, PutRequest, PutResponse,
    ScanRequest, ScanResponse,
};
use server_raft::Op;
use tonic::{Request, Response, Status};

use crate::bridge::Inner;
use crate::keys::{check_limits, strip, user_key, user_range};
use crate::metrics::{Method, result_code};

pub(crate) struct Kv {
    pub(crate) inner: Arc<Inner>,
}

#[tonic::async_trait]
impl KvService for Kv {
    async fn get(&self, request: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        let started = Instant::now();
        let result = async {
            let key = user_key(&request.into_inner().key)?;
            check_limits(&key, None, self.inner.max_key_len, self.inner.max_value_len)?;
            self.inner.read_barrier().await?;
            let value = self.inner.blocking(move |db| db.get(&key)).await?;
            Ok(Response::new(GetResponse { value }))
        }
        .await;
        self.inner
            .metrics
            .record_rpc(Method::Get, result_code(&result), started.elapsed());
        result
    }

    async fn put(&self, request: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        let started = Instant::now();
        let result = async {
            let PutRequest { key, value } = request.into_inner();
            let key = user_key(&key)?;
            check_limits(
                &key,
                Some(&value),
                self.inner.max_key_len,
                self.inner.max_value_len,
            )?;
            self.inner.propose(vec![Op::Put { key, value }]).await?;
            Ok(Response::new(PutResponse {}))
        }
        .await;
        self.inner
            .metrics
            .record_rpc(Method::Put, result_code(&result), started.elapsed());
        result
    }

    async fn delete(
        &self,
        request: Request<DeleteRequest>,
    ) -> Result<Response<DeleteResponse>, Status> {
        let started = Instant::now();
        let result = async {
            let key = user_key(&request.into_inner().key)?;
            check_limits(&key, None, self.inner.max_key_len, self.inner.max_value_len)?;
            self.inner.propose(vec![Op::Delete { key }]).await?;
            Ok(Response::new(DeleteResponse {}))
        }
        .await;
        self.inner
            .metrics
            .record_rpc(Method::Delete, result_code(&result), started.elapsed());
        result
    }

    /// One page of `[start, end)`: at most `page_size` records (the server
    /// maximum if 0 or larger), and no more once the page reaches
    /// `scan_max_bytes`. `next_start` is the first key not returned.
    async fn scan(&self, request: Request<ScanRequest>) -> Result<Response<ScanResponse>, Status> {
        let started = Instant::now();
        let result = async {
            let ScanRequest {
                start,
                end,
                page_size,
            } = request.into_inner();
            if let (Some(s), Some(e)) = (start.as_deref(), end.as_deref())
                && s > e
            {
                return Err(Status::invalid_argument("scan start is after end"));
            }
            self.inner.read_barrier().await?;
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
        .await;
        self.inner
            .metrics
            .record_rpc(Method::Scan, result_code(&result), started.elapsed());
        result
    }
}
