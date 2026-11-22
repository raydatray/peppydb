//! `SimStore`'s crash model (plan 0030).

use super::*;

const PAGE: usize = 128;

fn store() -> SimStore {
    SimStore::new(superblock(PAGE)).unwrap()
}

fn state(id: u64) -> State {
    State {
        checkpoint_id: id,
        applied_seq: id * 10,
        write_seq: id * 10,
    }
}

#[test]
fn synced_pages_survive_and_unsynced_ones_are_lost_landed_or_torn() {
    let store = store();
    let a = store.alloc().unwrap();
    store.write(a, &page_image(PAGE, 1)).unwrap();
    store.sync().unwrap();
    store.write(a, &page_image(PAGE, 2)).unwrap();

    let lost = store.crash_with(|_| 0);
    assert_eq!(lost.pages[0], page_image(PAGE, 1));
    let landed = store.crash_with(|bound| if bound == 3 { 1 } else { 0 });
    assert_eq!(landed.pages[0], page_image(PAGE, 2));
    // Torn: every other sector lands.
    let mut flip = false;
    let torn = store.crash_with(|bound| {
        if bound == 3 {
            return 2;
        }
        flip = !flip;
        u64::from(!flip)
    });
    for (sector, chunk) in torn.pages[0].chunks(SECTOR).enumerate() {
        let from = if sector % 2 == 0 { 2 } else { 1 };
        assert_eq!(
            chunk,
            &page_image(PAGE, from)[sector * SECTOR..(sector + 1) * SECTOR]
        );
    }
    assert_eq!(store.crash(7), store.crash(7), "a seed fixes the crash");
}

#[test]
fn a_commit_makes_writes_and_the_state_durable() {
    let store = store();
    let a = store.alloc().unwrap();
    store.write(a, &page_image(PAGE, 3)).unwrap();
    store.commit(&state(1)).unwrap();
    let image = store.crash(1);
    assert_eq!(image.state, state(1));
    assert_eq!(image.pages[0], page_image(PAGE, 3));
    assert!(matches!(
        store.commit(&state(3)),
        Err(StoreError::CommitOrder { .. })
    ));

    store.fail_next_commit(CommitFault::BeforeCopy);
    assert!(store.commit(&state(2)).is_err());
    assert_eq!(store.durable_state(), state(1));
    store.fail_next_commit(CommitFault::AfterCopy);
    assert!(store.commit(&state(2)).is_err());
    assert_eq!(store.durable_state(), state(2), "the copy landed");
}

#[test]
fn an_image_reopens_and_a_dead_store_refuses_calls() {
    let store = store();
    assert!(store.is_fresh());
    let a = store.alloc().unwrap();
    store.write(a, &page_image(PAGE, 4)).unwrap();
    store.commit(&state(1)).unwrap();
    let reopened = SimStore::from_image(&store.crash(3));
    assert!(!reopened.is_fresh());
    assert_eq!(reopened.state(), state(1));
    let mut buf = vec![0; PAGE];
    reopened.read(a, &mut buf).unwrap();
    assert_eq!(buf, page_image(PAGE, 4));

    let calls = store.calls();
    store.die_after(1);
    store.read(a, &mut buf).unwrap();
    assert!(store.read(a, &mut buf).is_err());
    assert!(store.sync().is_err());
    assert!(store.calls() > calls);
    reopened.kill();
    assert!(reopened.alloc().is_err());
}

#[test]
fn a_store_file_image_parses() {
    let image = {
        let store = store();
        let a = store.alloc().unwrap();
        store.write(a, &page_image(PAGE, 5)).unwrap();
        store.commit(&state(1)).unwrap();
        store.crash(1)
    };
    let mut file = vec![0; 2 * COPY_BYTES];
    image
        .superblock
        .encode(&image.state, &mut file[..COPY_BYTES])
        .unwrap();
    file.extend(&image.pages[0]);
    assert_eq!(SimImage::from_file(&file), Ok(image));
    assert_eq!(
        SimImage::from_file(&file[..file.len() - 1]),
        Err(StoreError::TruncatedFile)
    );
    assert_eq!(
        SimImage::from_file(&vec![0; 2 * COPY_BYTES]),
        Err(StoreError::NoSuperblock)
    );
}
