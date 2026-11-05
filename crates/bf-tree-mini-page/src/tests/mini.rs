use bf_tree_page::RecordState;

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
