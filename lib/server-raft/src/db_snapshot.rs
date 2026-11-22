//! Snapshots of `DbStateMachine` (plan 0030, diff 7).

use std::io;

use openraft::AsyncRuntime;

use crate::db_sm::Shared;
use crate::types::OrSnapshot;
use crate::types::OrSnapshotMeta;

pub(crate) async fn build<RT: AsyncRuntime>(_shared: &Shared<RT>) -> Result<OrSnapshot, io::Error> {
    Err(io::Error::other("snapshots are not built yet"))
}

pub(crate) async fn install<RT: AsyncRuntime>(
    _shared: &Shared<RT>,
    _meta: &OrSnapshotMeta,
    _snapshot: Vec<u8>,
) -> Result<(), io::Error> {
    Err(io::Error::other("snapshots are not installed yet"))
}

pub(crate) async fn current<RT: AsyncRuntime>(
    _shared: &Shared<RT>,
) -> Result<Option<OrSnapshot>, io::Error> {
    Ok(None)
}
