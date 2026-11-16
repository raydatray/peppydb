use std::path::Path;

use crate::{Fs, FsFile, RealFs, SimFs, SimImage};

fn dir() -> &'static Path {
    Path::new("/log")
}

fn file(name: &str) -> std::path::PathBuf {
    dir().join(name)
}

/// The behaviour both file systems share, with no crash.
fn exercise<F: Fs>(fs: &F, dir: &Path) {
    fs.create_dir_all(dir).unwrap();
    assert!(fs.list(dir).unwrap().is_empty());
    let path = dir.join("a");
    let mut a = fs.create_new(&path).unwrap();
    assert!(fs.create_new(&path).is_err(), "create_new refuses a name");
    a.append(b"hello ").unwrap();
    a.append(b"world").unwrap();
    assert_eq!(a.len(), 11);
    let mut buf = [0; 5];
    a.read_at(6, &mut buf).unwrap();
    assert_eq!(&buf, b"world");
    assert!(a.read_at(8, &mut buf).is_err(), "a read past the end fails");
    a.set_len(5).unwrap();
    a.append(b"!").unwrap();
    a.sync_data().unwrap();
    assert_eq!(fs.read(&path).unwrap(), b"hello!");

    let reopened = fs.open(&path).unwrap();
    assert_eq!(reopened.len(), 6);
    fs.rename(&path, &dir.join("b")).unwrap();
    assert_eq!(fs.list(dir).unwrap(), ["b"]);
    fs.remove(&dir.join("b")).unwrap();
    fs.sync_dir(dir).unwrap();
    assert!(fs.list(dir).unwrap().is_empty());
    assert!(fs.open(&path).is_err());
}

#[test]
fn real_and_simulated_file_systems_agree() {
    let tmp = tempfile::tempdir().unwrap();
    exercise(&RealFs, &tmp.path().join("log"));
    exercise(&SimFs::new(), dir());
}

#[test]
fn synced_state_survives_and_unsynced_bytes_are_lost() {
    let fs = SimFs::new();
    let mut f = fs.create_new(&file("f")).unwrap();
    f.append(b"durable").unwrap();
    f.sync_data().unwrap();
    fs.sync_dir(dir()).unwrap();
    f.append(b" lost").unwrap();

    let image = fs.crash_clean();
    assert_eq!(image.files[&file("f")], b"durable");
    let restarted = SimFs::from_image(&image);
    assert_eq!(restarted.read(&file("f")).unwrap(), b"durable");
}

#[test]
fn a_torn_tail_is_a_prefix_of_the_unsynced_bytes() {
    let fs = SimFs::new();
    let mut f = fs.create_new(&file("f")).unwrap();
    fs.sync_dir(dir()).unwrap();
    f.append(b"abc").unwrap();
    f.sync_data().unwrap();
    f.append(b"defgh").unwrap();
    for keep in 0..=5 {
        let image = fs.crash_with(|len| {
            assert_eq!(len, 5);
            keep
        });
        assert_eq!(image.files[&file("f")], &b"abcdefgh"[..3 + keep]);
    }
    // Seeded tears are prefixes too, and the same seed tears the same way.
    for seed in 0..50 {
        let bytes = &fs.crash(seed).files[&file("f")];
        assert!(b"abcdefgh".starts_with(bytes) && bytes.len() >= 3);
        assert_eq!(fs.crash(seed), fs.crash(seed));
    }
}

#[test]
fn an_unsynced_shortening_is_lost_whole() {
    let fs = SimFs::new();
    let mut f = fs.create_new(&file("f")).unwrap();
    fs.sync_dir(dir()).unwrap();
    f.append(b"abcdef").unwrap();
    f.sync_data().unwrap();
    f.set_len(2).unwrap();
    f.append(b"XY").unwrap();
    assert_eq!(fs.crash(7).files[&file("f")], b"abcdef");
    f.sync_data().unwrap();
    assert_eq!(fs.crash(7).files[&file("f")], b"abXY");
}

#[test]
fn unsynced_names_are_undone() {
    let fs = SimFs::new();
    let mut old = fs.create_new(&file("old")).unwrap();
    old.append(b"1").unwrap();
    old.sync_data().unwrap();
    fs.sync_dir(dir()).unwrap();

    // A create, a rename, and a remove, none of them synced.
    let mut new = fs.create_new(&file("new")).unwrap();
    new.append(b"2").unwrap();
    new.sync_data().unwrap();
    fs.rename(&file("new"), &file("renamed")).unwrap();
    fs.remove(&file("old")).unwrap();
    let image = fs.crash_clean();
    assert_eq!(image.files.keys().collect::<Vec<_>>(), [&file("old")]);

    // Synced, they survive.
    fs.sync_dir(dir()).unwrap();
    let image = fs.crash_clean();
    assert_eq!(image.files.keys().collect::<Vec<_>>(), [&file("renamed")]);
    assert_eq!(image.files[&file("renamed")], b"2");

    // A sync of another directory does not help.
    let mut other = fs.create_new(&file("other")).unwrap();
    other.sync_data().unwrap();
    fs.sync_dir(Path::new("/elsewhere")).unwrap();
    assert!(!fs.crash_clean().files.contains_key(&file("other")));
}

#[test]
fn injected_failures_surface_and_change_nothing() {
    let fs = SimFs::new();
    let mut f = fs.create_new(&file("f")).unwrap();
    assert_eq!(fs.ops(), 1);
    fs.fail_op(Some(2));
    f.append(b"a").unwrap();
    assert!(f.append(b"b").is_err());
    f.append(b"c").unwrap();
    assert_eq!(fs.read(&file("f")).unwrap(), b"ac");

    fs.cut_power_at(Some(fs.ops()));
    assert!(f.sync_data().is_err());
    assert!(fs.sync_dir(dir()).is_err());
    assert!(f.append(b"d").is_err());
    assert!(fs.create_new(&file("g")).is_err());
    assert_eq!(fs.crash_clean(), SimImage::default(), "nothing was synced");
    fs.cut_power_at(None);
    f.sync_data().unwrap();
}
