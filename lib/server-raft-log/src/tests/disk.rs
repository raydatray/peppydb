use std::path::{Path, PathBuf};

use wal_segments::HEADER_BYTES;

use crate::disk::Disk;
use crate::format::{self, FORMAT, Record, State};
use crate::{
    EntryKind, Fs, FsFile, LogEntry, LogError, LogOptions, LogPos, RealFs, Recovered, SimFs,
    StoredVote,
};

pub(super) fn dir() -> &'static Path {
    Path::new("/raft-log")
}

pub(super) fn entry(term: u64, index: u64) -> LogEntry {
    LogEntry {
        index,
        term,
        kind: EntryKind::Normal,
        payload: format!("entry {index}").into_bytes(),
    }
}

pub(super) fn pos(term: u64, index: u64) -> LogPos {
    LogPos { term, index }
}

/// Small segments, so tests cross segment boundaries.
pub(super) fn small() -> LogOptions {
    LogOptions {
        segment_bytes: 200,
        ..LogOptions::default()
    }
}

fn open<F: Fs>(fs: &F, dir: &Path) -> (Disk<F>, Recovered) {
    Disk::open(fs.clone(), dir, small()).unwrap()
}

/// Append `entries` as one group.
fn append<F: Fs>(disk: &mut Disk<F>, entries: impl IntoIterator<Item = LogEntry>) {
    for entry in entries {
        disk.push_entry(&entry).unwrap();
    }
    disk.sync_group().unwrap();
}

fn read_all<F: Fs>(disk: &Disk<F>, indexes: std::ops::RangeInclusive<u64>) -> Vec<u64> {
    indexes
        .filter_map(|index| disk.read(index).unwrap())
        .map(|entry| entry.index)
        .collect()
}

fn segments(fs: &SimFs) -> Vec<u64> {
    let mut names: Vec<_> = fs
        .list(dir())
        .unwrap()
        .iter()
        .filter_map(|name| FORMAT.parse_name(name))
        .collect();
    names.sort_unstable();
    names
}

fn segment_path(name: u64) -> PathBuf {
    FORMAT.path(dir(), name)
}

#[test]
fn records_and_state_round_trip() {
    for kind in [EntryKind::Blank, EntryKind::Normal, EntryKind::Membership] {
        let entry = LogEntry {
            kind,
            ..entry(3, 9)
        };
        let mut buf = Vec::new();
        let body_len = format::encode_entry(&mut buf, &entry);
        let (body, used) = FORMAT.decode_frame(&buf).unwrap();
        assert_eq!((body.len(), used), (body_len, buf.len()));
        assert_eq!(format::decode_body(body), Some(Record::Entry(entry)));
    }
    let mut buf = Vec::new();
    format::encode_commit(&mut buf, pos(2, 7));
    let (body, _) = FORMAT.decode_frame(&buf).unwrap();
    assert_eq!(format::decode_body(body), Some(Record::Commit(pos(2, 7))));

    for state in [
        State::default(),
        State {
            vote: Some(StoredVote {
                term: 4,
                node: Some(2),
                committed: true,
            }),
            purged: Some(pos(3, 10)),
        },
        State {
            vote: Some(StoredVote {
                term: 4,
                node: None,
                committed: false,
            }),
            purged: None,
        },
    ] {
        let bytes = format::encode_state(&state);
        assert_eq!(format::decode_state(&bytes), Ok(state));
        let mut bad = bytes;
        bad[20] ^= 1;
        assert_eq!(format::decode_state(&bad), Err("bad state checksum"));
        assert!(format::decode_state(&bytes[..63]).is_err());
    }
}

#[test]
fn entries_survive_a_reopen_on_both_file_systems() {
    fn check<F: Fs>(fs: F, dir: &Path) {
        let (mut disk, recovered) = open(&fs, dir);
        assert_eq!(recovered, Recovered::default());
        append(&mut disk, (1..=3).map(|i| entry(1, i)));
        disk.push_commit(pos(1, 2)).unwrap();
        append(&mut disk, (4..=20).map(|i| entry(2, i)));
        disk.save_vote(StoredVote {
            term: 2,
            node: Some(1),
            committed: true,
        })
        .unwrap();
        drop(disk);

        let (disk, recovered) = open(&fs, dir);
        assert_eq!(recovered.last, Some(pos(2, 20)));
        assert_eq!(recovered.committed, Some(pos(1, 2)));
        assert_eq!(recovered.vote.map(|vote| vote.node), Some(Some(1)));
        assert_eq!(read_all(&disk, 0..=30), (1..=20).collect::<Vec<_>>());
        assert_eq!(disk.read(7).unwrap(), Some(entry(2, 7)));
    }
    let tmp = tempfile::tempdir().unwrap();
    check(RealFs, &tmp.path().join("log"));
    check(SimFs::new(), dir());
}

#[test]
fn segments_roll_at_group_boundaries_and_frames_are_aligned() {
    let fs = SimFs::new();
    let (mut disk, _) = open(&fs, dir());
    for i in 1..=30 {
        append(&mut disk, [entry(1, i)]);
    }
    let names = segments(&fs);
    assert!(names.len() > 3, "{names:?}");
    assert_eq!(names[0], 1);
    for name in &names {
        let bytes = fs.read(&segment_path(*name)).unwrap();
        assert_eq!(FORMAT.decode_header(&bytes), Ok(*name));
        let mut offset = HEADER_BYTES;
        let mut first = None;
        while offset < bytes.len() {
            assert_eq!(offset % 8, 0, "segment {name} offset {offset}");
            let (body, used) = FORMAT.decode_frame(&bytes[offset..]).unwrap();
            if let Some(Record::Entry(entry)) = format::decode_body(body) {
                first.get_or_insert(entry.index);
            }
            offset += used;
        }
        assert_eq!(first, Some(*name), "a segment starts at its name");
    }
}

#[test]
fn a_torn_tail_at_every_byte_is_repaired() {
    let fs = SimFs::new();
    let (mut disk, _) = open(&fs, dir());
    append(&mut disk, [entry(1, 1), entry(1, 2)]);
    drop(disk);
    let durable = fs.read(&segment_path(1)).unwrap().len();
    let record = FORMAT.frame_bytes(format::MIN_BODY + 7);
    // A group of three records, written but not synced.
    let mut bytes = Vec::new();
    for i in 3..=5 {
        format::encode_entry(&mut bytes, &entry(1, i));
    }
    assert_eq!(bytes.len(), 3 * record);
    let mut file = fs.open(&segment_path(1)).unwrap();
    file.append(&bytes).unwrap();
    for keep in 0..=bytes.len() {
        let image = fs.crash_with(|_| keep);
        let restarted = SimFs::from_image(&image);
        let (disk, recovered) = open(&restarted, dir());
        let whole = (keep / record) as u64;
        assert_eq!(recovered.last, Some(pos(1, 2 + whole)), "keep {keep}");
        assert_eq!(
            recovered.repaired_bytes,
            (keep % record) as u64,
            "keep {keep}"
        );
        assert_eq!(read_all(&disk, 1..=9).len() as u64, 2 + whole);
        assert_eq!(
            restarted.read(&segment_path(1)).unwrap().len(),
            durable + whole as usize * record
        );
    }
}

#[test]
fn damage_before_intact_records_and_in_earlier_segments_is_corruption() {
    let fs = SimFs::new();
    let (mut disk, _) = open(&fs, dir());
    for i in 1..=30 {
        append(&mut disk, [entry(1, i)]);
    }
    drop(disk);
    let names = segments(&fs);
    let image = fs.crash_clean();

    // Damage inside the last segment, followed by intact records.
    let last = segment_path(*names.last().unwrap());
    let mut damaged = image.clone();
    damaged.files.get_mut(&last).unwrap()[HEADER_BYTES + 9] ^= 1;
    let error = Disk::open(SimFs::from_image(&damaged), dir(), small()).err();
    assert!(
        matches!(
            error,
            Some(LogError::Corrupt {
                reason: "damage before intact records",
                ..
            })
        ),
        "{error:?}"
    );

    // Damage at the end of an earlier segment.
    let earlier = segment_path(names[0]);
    let mut damaged = image.clone();
    let bytes = damaged.files.get_mut(&earlier).unwrap();
    let at = bytes.len() - 3;
    bytes[at] ^= 1;
    let error = Disk::open(SimFs::from_image(&damaged), dir(), small()).err();
    assert!(
        matches!(
            error,
            Some(LogError::Corrupt {
                reason: "undecodable record",
                ..
            })
        ),
        "{error:?}"
    );

    // A missing middle segment.
    let mut damaged = image;
    damaged.files.remove(&segment_path(names[1]));
    let error = Disk::open(SimFs::from_image(&damaged), dir(), small()).err();
    assert!(
        matches!(
            error,
            Some(LogError::Corrupt {
                reason: "entries are missing",
                ..
            })
        ),
        "{error:?}"
    );
}

#[test]
fn a_torn_last_header_is_removed() {
    let fs = SimFs::new();
    let (mut disk, _) = open(&fs, dir());
    append(&mut disk, [entry(1, 1)]);
    drop(disk);
    let mut torn = fs.create_new(&segment_path(2)).unwrap();
    torn.append(&FORMAT.encode_header(2)[..10]).unwrap();
    torn.sync_data().unwrap();
    fs.sync_dir(dir()).unwrap();
    let (_, recovered) = open(&fs, dir());
    assert_eq!(recovered.last, Some(pos(1, 1)));
    assert_eq!(segments(&fs), [1]);
}

#[test]
fn truncation_and_purge_shape_the_log_across_segments() {
    let fs = SimFs::new();
    let (mut disk, _) = open(&fs, dir());
    for i in 1..=30 {
        append(&mut disk, [entry(1, i)]);
    }
    disk.truncate_after(Some(12)).unwrap();
    assert_eq!(disk.last(), Some(pos(1, 12)));
    append(&mut disk, [entry(2, 13), entry(2, 14)]);
    assert!(segments(&fs).iter().all(|name| *name <= 14));
    drop(disk);

    let (mut disk, recovered) = open(&fs, dir());
    assert_eq!(recovered.last, Some(pos(2, 14)));
    assert_eq!(disk.read(13).unwrap(), Some(entry(2, 13)));
    assert_eq!(disk.read(15).unwrap(), None);

    disk.purge(pos(1, 9)).unwrap();
    assert!(segments(&fs)[0] <= 10, "segment holding 10 is kept");
    assert!(segments(&fs)[0] > 1, "wholly purged segments are gone");
    assert_eq!(disk.read(9).unwrap(), None);
    drop(disk);

    let (disk, recovered) = open(&fs, dir());
    assert_eq!(recovered.purged, Some(pos(1, 9)));
    assert_eq!(read_all(&disk, 0..=20), (10..=14).collect::<Vec<_>>());

    // Truncating everything removes every segment.
    let (mut disk, _) = (disk, ());
    disk.truncate_after(Some(9)).unwrap();
    assert_eq!(disk.last(), None);
    drop(disk);
    let (_, recovered) = open(&fs, dir());
    assert_eq!(
        recovered.last,
        Some(pos(1, 9)),
        "the purge point when empty"
    );
}

#[test]
fn a_purge_past_the_end_lets_the_log_continue_after_it() {
    let fs = SimFs::new();
    let (mut disk, _) = open(&fs, dir());
    append(&mut disk, (1..=5).map(|i| entry(1, i)));
    disk.purge(pos(3, 40)).unwrap();
    assert!(segments(&fs).is_empty());
    append(&mut disk, [entry(3, 41)]);
    drop(disk);
    let (disk, recovered) = open(&fs, dir());
    assert_eq!(recovered.last, Some(pos(3, 41)));
    assert_eq!(segments(&fs), [41]);
    assert_eq!(disk.read(41).unwrap(), Some(entry(3, 41)));
}

#[test]
fn an_interrupted_purge_is_finished_on_open() {
    let fs = SimFs::new();
    let (mut disk, _) = open(&fs, dir());
    for i in 1..=30 {
        append(&mut disk, [entry(1, i)]);
    }
    let before = segments(&fs);
    // Power is cut after the state file is replaced, before any segment
    // is deleted: create, append, sync, rename, sync directory.
    fs.cut_power_at(Some(fs.ops() + 5));
    assert!(disk.purge(pos(1, 30)).is_err());
    drop(disk);
    let restarted = SimFs::from_image(&fs.crash_clean());
    assert_eq!(segments(&restarted), before);
    let (mut disk, recovered) = open(&restarted, dir());
    assert_eq!(recovered.purged, Some(pos(1, 30)));
    assert_eq!(recovered.last, Some(pos(1, 30)));
    assert!(segments(&restarted).is_empty());
    append(&mut disk, [entry(1, 31)]);
}

#[test]
fn the_committed_log_id_is_never_below_the_purge_point() {
    let fs = SimFs::new();
    let (mut disk, _) = open(&fs, dir());
    append(&mut disk, (1..=8).map(|i| entry(1, i)));
    disk.push_commit(pos(1, 3)).unwrap();
    disk.sync_group().unwrap();
    disk.purge(pos(1, 5)).unwrap();
    drop(disk);
    let (_, recovered) = open(&fs, dir());
    assert_eq!(recovered.committed, Some(pos(1, 5)));

    // A later commit record above the purge point wins.
    let (mut disk, _) = open(&fs, dir());
    disk.push_commit(pos(1, 7)).unwrap();
    disk.sync_group().unwrap();
    drop(disk);
    assert_eq!(open(&fs, dir()).1.committed, Some(pos(1, 7)));
}

#[test]
fn a_state_write_cut_at_any_operation_leaves_the_old_or_the_new_state() {
    let old = StoredVote {
        term: 1,
        node: Some(1),
        committed: false,
    };
    let new = StoredVote {
        term: 2,
        node: Some(3),
        committed: true,
    };
    for cut in 0.. {
        let fs = SimFs::new();
        let (mut disk, _) = open(&fs, dir());
        disk.save_vote(old).unwrap();
        let start = fs.ops();
        fs.cut_power_at(Some(start + cut));
        let done = disk.save_vote(new).is_ok();
        for seed in 0..4 {
            let restarted = SimFs::from_image(&fs.crash(seed));
            let (_, recovered) = open(&restarted, dir());
            let vote = recovered.vote.unwrap();
            assert!(vote == old || vote == new, "cut {cut}");
            if done {
                assert_eq!(vote, new);
            }
            assert!(
                !restarted
                    .list(dir())
                    .unwrap()
                    .contains(&"state.tmp".to_owned())
            );
        }
        if done {
            break;
        }
    }
}
