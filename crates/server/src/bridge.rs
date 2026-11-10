//! The blocking bridge (plan 0026, "The blocking bridge").
//!
//! `Db` methods block: a durable write waits for an fsync. tonic runs
//! handlers on tokio's async worker threads, so each engine call moves to
//! tokio's blocking pool, holding one permit of a semaphore. The semaphore
//! bounds the work in flight, and shutdown closes it and takes every permit
//! to prove that no engine call is running.

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use engine::Db;
use tokio::sync::Semaphore;
use tonic::Status;

use crate::Config;

/// State shared by the services.
pub(crate) struct Inner {
    /// `None` once shutdown has taken the database to close it.
    db: ArcSwapOption<Db>,
    inflight: Arc<Semaphore>,
    pub(crate) config: Config,
}

impl Inner {
    pub(crate) fn new(db: Db, config: Config) -> Self {
        Self {
            db: ArcSwapOption::from_pointee(db),
            inflight: Arc::new(Semaphore::new(config.max_inflight)),
            config,
        }
    }

    /// Run `f` on the blocking pool, holding one in-flight permit until the
    /// engine call returns, even if the client goes away first.
    pub(crate) async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Db) -> Result<T, engine::Error> + Send + 'static,
    ) -> Result<T, Status> {
        self.blocking_status(move |db| f(db).map_err(|error| crate::status::status(&error)))
            .await
    }

    /// As [`Inner::blocking`], for calls that build their own status.
    pub(crate) async fn blocking_status<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Db) -> Result<T, Status> + Send + 'static,
    ) -> Result<T, Status> {
        let permit = Arc::clone(&self.inflight)
            .acquire_owned()
            .await
            .map_err(|_| shutting_down())?;
        let db = self.db.load_full().ok_or_else(shutting_down)?;
        tokio::task::spawn_blocking(move || {
            // Declared first, so dropped last: the database reference is
            // released before the permit, and shutdown, which waits for
            // every permit, then holds the only reference.
            let _permit = permit;
            let db = db;
            f(&db)
        })
        .await
        .map_err(|error| Status::internal(format!("engine task failed: {error}")))?
    }

    /// Stop new engine calls, wait for running ones, and take the database.
    /// Returns `None` if it was already taken.
    pub(crate) async fn take_db(&self) -> Option<Db> {
        // The semaphore is fair: requests that arrive after this call queue
        // behind it, and closing the semaphore fails them.
        let all = u32::try_from(self.config.max_inflight).unwrap_or(u32::MAX);
        let permits = self.inflight.acquire_many(all).await.ok();
        self.inflight.close();
        drop(permits);
        let db = self.db.swap(None)?;
        let db = Arc::try_unwrap(db).unwrap_or_else(|_| {
            panic!("every engine call has returned, so no other reference exists")
        });
        Some(db)
    }
}

fn shutting_down() -> Status {
    Status::unavailable("the server is shutting down")
}
