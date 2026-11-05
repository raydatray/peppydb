use bf_tree_page::{PageError, RecordState};

use super::*;

#[test]
fn a_new_mini_page_is_empty_unfenced_and_names_its_leaf() {
    let mut bytes = vec![0xff; 128];
    let mini = MiniPage::init(&mut bytes, leaf(9));
    assert_eq!(mini.backing_leaf(), leaf(9));
    assert!(mini.page().is_empty());
    assert!(mini.page().fences().is_none());
    assert_eq!(MiniPage::from_bytes(&bytes).backing_leaf(), leaf(9));
}

#[test]
fn lookup_marking_answers_per_kind_and_marks_exactly_the_hit_slot() {
    let mut bytes = vec![0; 256];
    let mini = MiniPage::init(&mut bytes, leaf(1));
    let page = mini.page_mut();
    page.try_insert_record(b"a", RecordState::Insert(b"1"))
        .unwrap();
    page.try_insert_record(b"b", RecordState::Delete).unwrap();
    page.try_insert_record(b"c", RecordState::Cache(b"3"))
        .unwrap();
    page.try_insert_record(b"d", RecordState::Phantom).unwrap();
    let mini = MiniPage::from_bytes(&bytes);

    let cases = [
        (&b"a"[..], Lookup::Found(b"1"), Some(0)),
        (b"b", Lookup::Absent, Some(1)),
        (b"c", Lookup::Found(b"3"), Some(2)),
        (b"d", Lookup::Absent, Some(3)),
        (b"e", Lookup::Miss, None),
        (b"", Lookup::Miss, None),
    ];
    for (key, answer, slot) in cases {
        let refs = RefBits::default();
        assert_eq!(mini.lookup_marking(&refs, key), answer);
        let set: Vec<_> = (0..512).filter(|&i| refs.is_set(i)).collect();
        assert_eq!(set, slot.into_iter().collect::<Vec<_>>(), "key {key:?}");
    }
}

fn kind_of(mini: &MiniPage, key: &[u8]) -> Option<(bf_tree_page::RecordKind, Option<Vec<u8>>)> {
    let index = mini.page().find_index(key).unwrap()?;
    let state = mini.page().record_at(index).state;
    Some((state.kind(), state.value().map(<[u8]>::to_vec)))
}

#[test]
fn apply_follows_the_record_table_cell_by_cell() {
    let incoming = [
        RecordState::Insert(b"new"),
        RecordState::Delete,
        RecordState::Cache(b"new"),
        RecordState::Phantom,
    ];
    let buffered = [
        None,
        Some(RecordState::Insert(b"old")),
        Some(RecordState::Delete),
        Some(RecordState::Cache(b"old")),
        Some(RecordState::Phantom),
    ];
    for state in incoming {
        for before in buffered {
            let mut bytes = vec![0; 256];
            let mini = MiniPage::init(&mut bytes, leaf(1));
            let mut refs = RefBits::default();
            if let Some(before) = before {
                mini.apply(&mut refs, b"k", before).unwrap();
            }
            let dirty_before = before.is_some_and(|b| b.kind().is_dirty());
            let skip = dirty_before && !state.kind().is_dirty();

            let result = mini.apply(&mut refs, b"k", state).unwrap();
            let expected = if skip {
                (Apply::SkippedDirty, before.unwrap())
            } else {
                (Apply::Applied, state)
            };
            assert_eq!(result, expected.0, "{state:?} over {before:?}");
            let value = expected.1.value().map(<[u8]>::to_vec);
            assert_eq!(kind_of(mini, b"k"), Some((expected.1.kind(), value)));
            assert_eq!(mini.page().len(), 1);
        }
    }
}

#[test]
fn apply_errors_leave_the_page_unchanged() {
    let mut bytes = vec![0; 64];
    let mini = MiniPage::init(&mut bytes, leaf(1));
    let mut refs = RefBits::default();
    mini.apply(&mut refs, b"a", RecordState::Insert(&[1; 16]))
        .unwrap();
    let before = bytes.clone();
    let mini = MiniPage::from_bytes_mut(&mut bytes);
    assert_eq!(
        mini.apply(&mut refs, b"b", RecordState::Insert(&[2; 16])),
        Err(PageError::PageFull)
    );
    assert_eq!(
        mini.apply(&mut refs, b"b", RecordState::Insert(&[2; 40])),
        Err(PageError::RecordTooLarge)
    );
    assert_eq!(
        mini.apply(&mut refs, b"", RecordState::Delete),
        Err(PageError::EmptyKey)
    );
    assert_eq!(bytes, before);
}

#[test]
fn reference_bits_follow_their_slots_through_insertions() {
    let mut bytes = vec![0; 512];
    let mini = MiniPage::init(&mut bytes, leaf(1));
    let mut refs = RefBits::default();
    for key in [b"m", b"c", b"x"] {
        mini.apply(&mut refs, key, RecordState::Cache(b"v"))
            .unwrap();
    }
    // Mark "m" (slot 1) and "x" (slot 2).
    assert_eq!(mini.lookup_marking(&refs, b"m"), Lookup::Found(b"v"));
    assert_eq!(mini.lookup_marking(&refs, b"x"), Lookup::Found(b"v"));

    // Inserting before them moves their slots and their bits up.
    for key in [b"a", b"d", b"n"] {
        mini.apply(&mut refs, key, RecordState::Insert(b"w"))
            .unwrap();
    }
    let marked: Vec<_> = (0..mini.page().len())
        .filter(|&i| refs.is_set(i))
        .map(|i| mini.page().record_at(i).key.to_vec())
        .collect();
    assert_eq!(marked, [b"m".to_vec(), b"x".to_vec()]);

    // Replacements and compaction keep positions.
    mini.apply(&mut refs, b"m", RecordState::Insert(b""))
        .unwrap();
    mini.compact();
    assert!(refs.is_set(mini.page().find_index(b"m").unwrap().unwrap()));
}

#[test]
fn copy_into_keeps_dirty_records_and_drops_cold_clean_ones_only_when_asked() {
    let mut bytes = vec![0; 512];
    let mini = MiniPage::init(&mut bytes, leaf(4));
    let mut refs = RefBits::default();
    let records = [
        (&b"a"[..], RecordState::Insert(b"1")),
        (b"b", RecordState::Delete),
        (b"c", RecordState::Cache(b"hot")),
        (b"d", RecordState::Cache(b"cold")),
        (b"e", RecordState::Phantom),
    ];
    for (key, state) in records {
        mini.apply(&mut refs, key, state).unwrap();
    }
    mini.lookup_marking(&refs, b"c");

    let keys =
        |page: &MiniPage| -> Vec<Vec<u8>> { page.page().iter().map(|r| r.key.to_vec()).collect() };

    let mut all = vec![0; 1024];
    let copy = mini.copy_into(&refs, &mut all, false);
    assert_eq!(
        keys(copy),
        [b"a", b"b", b"c", b"d", b"e"].map(|k| k.to_vec())
    );
    assert_eq!(copy.backing_leaf(), leaf(4));

    let mut kept = vec![0; 256];
    let copy = mini.copy_into(&refs, &mut kept, true);
    assert_eq!(keys(copy), [b"a", b"b", b"c"].map(|k| k.to_vec()));
    assert_eq!(copy.backing_leaf(), leaf(4));

    let dirty: Vec<_> = mini.dirty().map(|r| r.key.to_vec()).collect();
    assert_eq!(dirty, [b"a".to_vec(), b"b".to_vec()]);
}

#[test]
fn a_mini_lives_in_a_circular_buffer_block() {
    use bf_tree_circular_buffer::{CircularBuffer, Kind};

    let buffer = CircularBuffer::new(1024, &[64, 128], 0).unwrap();
    let mut mini = Mini::new(buffer.alloc(64, Kind::Mini, leaf(3)).unwrap(), leaf(3));
    assert_eq!(mini.backing_leaf(), leaf(3));
    mini.apply(b"k", RecordState::Cache(b"v")).unwrap();
    assert_eq!(mini.lookup_marking(b"k"), Lookup::Found(b"v"));
    assert!(mini.refs.is_set(0));

    let mut grown = Mini::new(buffer.alloc(128, Kind::Mini, leaf(3)).unwrap(), leaf(3));
    mini.copy_into(&mut grown, true);
    assert_eq!(grown.lookup_marking(b"k"), Lookup::Found(b"v"));
    assert_eq!(grown.page().capacity(), 128);

    let Mini { block, .. } = mini;
    buffer.free(block);
    buffer.free(grown.block);
    assert_eq!(buffer.used(), 0);
}
