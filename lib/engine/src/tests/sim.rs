//! An external database over `SimStore` (plan 0030): the owner's contract
//! holds under power-loss crashes at every store call.

use std::collections::BTreeMap;

use super::external::external;
use crate::*;

type Entry = (u64, Vec<(Vec<u8>, Vec<u8>)>);

/// Entries 1..=n, each writing two of 997 keys with 16-byte values.
fn entries(n: u64) -> Vec<Entry> {
    (1..=n)
        .map(|seq| {
            let ops = (0..2u64)
                .map(|i| {
                    (
                        vec![b'k', ((seq * 3 + i) % 8) as u8],
                        seq.to_le_bytes().to_vec(),
                    )
                })
                .collect();
            (seq, ops)
        })
        .collect()
}

fn apply(db: &Db, (seq, ops): &Entry) -> Result<Applied, Error> {
    let ops: Vec<Op<'_>> = ops
        .iter()
        .map(|(key, value)| Op::Put { key, value })
        .collect();
    db.apply(*seq, &ops)
}

fn state_at(entries: &[Entry], seq: u64) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut map = BTreeMap::new();
    for (_, ops) in entries.iter().take_while(|(s, _)| *s <= seq) {
        for (key, value) in ops {
            map.insert(key.clone(), value.clone());
        }
    }
    map
}

fn contents(db: &Db) -> BTreeMap<Vec<u8>, Vec<u8>> {
    db.range(None, None).unwrap().map(Result::unwrap).collect()
}

fn fresh() -> SimStore {
    Db::new_sim_store(&external()).unwrap()
}

/// Apply the entries with a checkpoint every 25, the hook recording how far
/// the "log" was made durable. Stops at the first error.
fn workload(db: &Db, log: &[Entry], durable_log: &mut u64) -> Result<(), Error> {
    for entry in log {
        apply(db, entry)?;
        if entry.0.is_multiple_of(25) {
            db.checkpoint_with(|seq| {
                *durable_log = (*durable_log).max(seq);
                Ok(())
            })?;
        }
    }
    Ok(())
}

#[test]
fn a_crash_at_every_store_call_recovers_a_committed_prefix() {
    let log = entries(1500);
    let total = {
        let store = fresh();
        let db = Db::open_external_on(store.clone(), external()).unwrap();
        workload(&db, &log, &mut 0).unwrap();
        store.calls()
    };
    assert!(total > 50, "{total} store calls");
    let mut crashes = 0;
    for cut in 0..total {
        let store = fresh();
        let mut durable_log = 0;
        if let Ok(db) = Db::open_external_on(store.clone(), external()) {
            store.die_after(cut);
            let _ = workload(&db, &log, &mut durable_log);
        }
        for seed in 0..3 {
            let image = store.crash(cut * 31 + seed);
            let db = Db::open_external_on(SimStore::from_image(&image), external())
                .unwrap_or_else(|error| panic!("cut {cut}: reopen failed: {error}"));
            let applied = db.applied_seq();
            assert!(
                applied <= durable_log,
                "cut {cut}: the hook covered the commit"
            );
            assert_eq!(contents(&db), state_at(&log, applied), "cut {cut}");
            // Redelivery from applied + 1 completes the log.
            for entry in &log[applied as usize..] {
                assert_eq!(apply(&db, entry).unwrap(), Applied::Applied);
            }
            assert_eq!(apply(&db, &log[0]).unwrap(), Applied::AlreadyApplied);
            assert_eq!(contents(&db), state_at(&log, 1500));
            crashes += 1;
        }
    }
    assert_eq!(crashes, 3 * total);
}

#[test]
fn a_snapshot_installs_over_a_simulated_store() {
    let log = entries(25);
    let leader = Db::open_external_on(fresh(), external()).unwrap();
    for entry in &log {
        apply(&leader, entry).unwrap();
    }
    leader.checkpoint_with(|_| Ok(())).unwrap();
    let mut snapshot = Vec::new();
    leader.write_snapshot(&mut snapshot).unwrap();

    let follower = Db::open_external_on(fresh(), external()).unwrap();
    let (follower, store) = follower
        .install_snapshot_on(&mut snapshot.as_slice())
        .unwrap();
    assert_eq!(follower.applied_seq(), 25);
    assert_eq!(contents(&follower), state_at(&log, 25));
    // Installed means durable: a crash right after keeps it.
    drop(follower);
    let reopened = Db::open_external_on(SimStore::from_image(&store.crash(1)), external()).unwrap();
    assert_eq!(reopened.applied_seq(), 25);
    assert_eq!(contents(&reopened), state_at(&log, 25));

    let standalone = Db::open(tempfile::tempdir().unwrap().path(), super::small()).unwrap();
    assert!(matches!(
        standalone.install_snapshot_on(&mut snapshot.as_slice()),
        Err(Error::WrongMode)
    ));
}
