//! Multi-process 3-node cluster tests (plan 0032): election, leader hints,
//! `SIGKILL` leader failover, and snapshot catch-up on restart.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use kv_proto::kv::v1::admin_service_client::AdminServiceClient;
use kv_proto::kv::v1::kv_service_client::KvServiceClient;
use kv_proto::kv::v1::{GetRequest, LeaderHint, PutRequest, StatusRequest, StatusResponse};
use prost::Message;
use tonic::Code;

use super::process::ServerProcess;

fn free_addrs(n: usize) -> Vec<SocketAddr> {
    let listeners: Vec<_> = (0..n)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    listeners
        .into_iter()
        .map(|l| l.local_addr().unwrap())
        .collect()
}

fn node_args(
    id: u64,
    dir: &std::path::Path,
    addr: SocketAddr,
    peers_spec: &str,
    bootstrap: bool,
) -> Vec<String> {
    let mut args = vec![
        "serve".to_owned(),
        "--data-dir".to_owned(),
        dir.display().to_string(),
        "--listen".to_owned(),
        addr.to_string(),
        "--node-id".to_owned(),
        id.to_string(),
        "--peers".to_owned(),
        peers_spec.to_owned(),
        "--snapshot-after".to_owned(),
        "25".to_owned(),
        "--keep-below-snapshot".to_owned(),
        "5".to_owned(),
    ];
    if bootstrap {
        args.push("--bootstrap".to_owned());
    }
    args
}

async fn status_of(addr: SocketAddr) -> Option<StatusResponse> {
    let mut admin = AdminServiceClient::connect(format!("http://{addr}"))
        .await
        .ok()?;
    admin
        .status(StatusRequest {})
        .await
        .ok()
        .map(|r| r.into_inner())
}

async fn wait_for_leader(
    addrs: &BTreeMap<u64, SocketAddr>,
    exclude: Option<u64>,
) -> (u64, StatusResponse) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            for (&id, &addr) in addrs {
                if exclude == Some(id) {
                    continue;
                }
                if let Some(st) = status_of(addr).await
                    && st.role == "leader"
                {
                    return (id, st);
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("cluster elected a leader")
}

#[tokio::test(flavor = "multi_thread")]
async fn three_process_cluster_elects_serves_hints_and_survives_leader_sigkill() {
    let ports = free_addrs(3);
    let addrs: BTreeMap<u64, SocketAddr> = (1..=3u64).zip(ports.iter().copied()).collect();
    let peers_spec = addrs
        .iter()
        .map(|(id, addr)| format!("{id}={addr}"))
        .collect::<Vec<_>>()
        .join(",");

    let dirs: BTreeMap<u64, tempfile::TempDir> = (1..=3u64)
        .map(|id| (id, tempfile::tempdir().unwrap()))
        .collect();

    let mut procs: BTreeMap<u64, ServerProcess> = BTreeMap::new();
    for (&id, &addr) in &addrs {
        let args = node_args(id, dirs[&id].path(), addr, &peers_spec, id == 1);
        let (proc, bound) = ServerProcess::start_args(&args);
        assert_eq!(bound, addr);
        procs.insert(id, proc);
    }

    // 1. Wait for a leader and verify followers know the leader.
    let (leader_id, leader_status) = wait_for_leader(&addrs, None).await;
    let follower_id = *addrs.keys().find(|&&id| id != leader_id).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(st) = status_of(addrs[&follower_id]).await
                && st.role == "follower"
                && st.leader_id == Some(leader_id)
            {
                assert_eq!(st.leader_addr, Some(addrs[&leader_id].to_string()));
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("follower learned leader");

    // 2. Follower rejects Put and Get with UNAVAILABLE + LeaderHint details.
    let mut follower_kv = KvServiceClient::connect(format!("http://{}", addrs[&follower_id]))
        .await
        .unwrap();
    let put_err = follower_kv
        .put(PutRequest {
            key: b"reject".to_vec(),
            value: b"v".to_vec(),
        })
        .await
        .unwrap_err();
    assert_eq!(put_err.code(), Code::Unavailable);
    let hint = LeaderHint::decode(put_err.details()).expect("decode LeaderHint");
    assert_eq!(hint.leader_id, leader_id);
    assert_eq!(hint.leader_addr, addrs[&leader_id].to_string());

    let get_err = follower_kv
        .get(GetRequest {
            key: b"reject".to_vec(),
        })
        .await
        .unwrap_err();
    assert_eq!(get_err.code(), Code::Unavailable);
    let hint = LeaderHint::decode(get_err.details()).expect("decode LeaderHint");
    assert_eq!(hint.leader_id, leader_id);

    // 3. Write 20 keys to the leader.
    let mut leader_kv = KvServiceClient::connect(format!("http://{}", addrs[&leader_id]))
        .await
        .unwrap();
    for i in 0..20u32 {
        leader_kv
            .put(PutRequest {
                key: format!("k-{i:03}").into_bytes(),
                value: format!("v-{i:03}").into_bytes(),
            })
            .await
            .unwrap();
    }

    // 4. SIGKILL the leader and wait for failover to a surviving node.
    procs.get_mut(&leader_id).unwrap().sigkill();
    procs.remove(&leader_id);

    let (new_leader_id, new_status) = wait_for_leader(&addrs, Some(leader_id)).await;
    assert!(new_status.term > leader_status.term);

    // 5. Verify all 20 acknowledged keys survive on the new leader.
    let mut new_kv = KvServiceClient::connect(format!("http://{}", addrs[&new_leader_id]))
        .await
        .unwrap();
    for i in 0..20u32 {
        let value = new_kv
            .get(GetRequest {
                key: format!("k-{i:03}").into_bytes(),
            })
            .await
            .unwrap()
            .into_inner()
            .value;
        assert_eq!(
            value,
            Some(format!("v-{i:03}").into_bytes()),
            "lost k-{i:03}"
        );
    }

    // 6. Write 40 more keys while the killed node is down so the leader
    //    compacts and purges its log past the old node's position.
    for i in 20..60u32 {
        new_kv
            .put(PutRequest {
                key: format!("k-{i:03}").into_bytes(),
                value: format!("v-{i:03}").into_bytes(),
            })
            .await
            .unwrap();
    }
    let target_seq = status_of(addrs[&new_leader_id]).await.unwrap().applied_seq;

    // 7. Restart the killed node; it must catch up via InstallSnapshot.
    let restart_args = node_args(
        leader_id,
        dirs[&leader_id].path(),
        addrs[&leader_id],
        &peers_spec,
        leader_id == 1,
    );
    let (restarted, _) = ServerProcess::start_args(&restart_args);
    procs.insert(leader_id, restarted);

    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(st) = status_of(addrs[&leader_id]).await
                && st.applied_seq >= target_seq
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("restarted node caught up via snapshot");

    // 8. Stop all 3 nodes cleanly with SIGTERM.
    for proc in procs.values() {
        proc.sigterm();
    }
    for mut proc in procs.into_values() {
        let status = proc.wait();
        assert_eq!(status.code(), Some(0), "stderr:\n{}", proc.stderr());
    }
}
