use bf_tree_circular_buffer::{CircularBuffer, CircularBufferFull, Kind};
use bf_tree_page::{PageError, RecordState};

use super::*;

/// Classes 64, 128, 256, 320 for L = 512, K = 16, V = 32.
fn classes() -> SizeClasses {
    SizeClasses::derive(512, 16, 32).unwrap()
}

fn capacity(slot: &Option<Mini>) -> usize {
    slot.as_ref().unwrap().page().capacity()
}

/// The leaf holds exactly `n` live blocks of the circular buffer.
fn live_blocks(buffer: &CircularBuffer) -> u64 {
    let metrics = buffer.metrics();
    metrics.allocs + metrics.reuses - metrics.frees
}

#[test]
fn writes_climb_the_ladder_and_end_in_needs_merge() {
    let buffer = CircularBuffer::new(4096, classes().as_slice(), 0).unwrap();
    let classes = classes();
    let mut slot = None;
    let mut seen = Vec::new();
    let mut outcome = WriteOutcome::Buffered;
    for i in 0..100u8 {
        outcome = buffer_record(&mut slot, &buffer, &classes, &[b'k', i], &[i; 20]);
        if outcome == WriteOutcome::NeedsMerge {
            break;
        }
        assert_eq!(outcome, WriteOutcome::Buffered);
        if seen.last() != Some(&capacity(&slot)) {
            seen.push(capacity(&slot));
        }
        assert_eq!(live_blocks(&buffer), 1);
    }
    assert_eq!(outcome, WriteOutcome::NeedsMerge);
    assert_eq!(seen, [64, 128, 256, 320]);
    assert_eq!(capacity(&slot), classes.largest());

    // NeedsMerge changed nothing; every record buffered before is still there.
    let mini = slot.take().unwrap();
    let count = mini.page().len();
    assert!(count > 0);
    for i in 0..count as u8 {
        assert!(matches!(mini.lookup_marking(&[b'k', i]), Lookup::Found(_)));
    }
    buffer.free(mini.block);
    assert_eq!(buffer.used(), 0);
}

fn buffer_record(
    slot: &mut Option<Mini>,
    buffer: &CircularBuffer,
    classes: &SizeClasses,
    key: &[u8],
    value: &[u8],
) -> WriteOutcome {
    super::buffer(
        slot,
        buffer,
        leaf(1),
        classes,
        key,
        RecordState::Insert(value),
    )
    .unwrap()
}

#[test]
fn compaction_is_chosen_when_holes_suffice() {
    let buffer = CircularBuffer::new(4096, classes().as_slice(), 0).unwrap();
    let classes = classes();
    let mut slot = None;
    // Fill the 64-byte class's 32 record bytes: 8 + 1 + 14 and 8 + 1 + 0.
    buffer_record(&mut slot, &buffer, &classes, b"a", &[1; 14]);
    buffer_record(&mut slot, &buffer, &classes, b"b", &[]);
    assert_eq!(capacity(&slot), 64);
    assert_eq!(slot.as_ref().unwrap().page().free_bytes(), 0);
    // Shrinking "a" leaves a 14-byte hole. Growing it again needs a fresh
    // 7-byte payload, which only compaction can provide.
    buffer_record(&mut slot, &buffer, &classes, b"a", &[]);
    buffer_record(&mut slot, &buffer, &classes, b"a", &[2; 6]);
    assert_eq!(capacity(&slot), 64, "compacted, not grown");
    assert_eq!(live_blocks(&buffer), 1);
    buffer.free(slot.take().unwrap().block);
}

#[test]
fn a_record_too_large_for_the_class_grows_without_compacting() {
    let buffer = CircularBuffer::new(4096, classes().as_slice(), 0).unwrap();
    let classes = classes();
    let mut slot = None;
    buffer_record(&mut slot, &buffer, &classes, b"a", &[1]);
    assert_eq!(capacity(&slot), 64);
    // 8 + 16 + 32 = 56 bytes: too large for an empty 64-byte page (32).
    buffer_record(&mut slot, &buffer, &classes, &[b'z'; 16], &[2; 32]);
    assert_eq!(capacity(&slot), 128);
    assert_eq!(live_blocks(&buffer), 1);
    buffer.free(slot.take().unwrap().block);
}

#[test]
fn a_full_circular_buffer_leaves_records_and_capacity_unchanged() {
    let classes = classes();
    // Room for the 64-byte block and little else.
    let buffer = CircularBuffer::new(1024, classes.as_slice(), 0).unwrap();
    let mut slot = None;
    buffer_record(&mut slot, &buffer, &classes, b"a", &[1; 7]);
    buffer_record(&mut slot, &buffer, &classes, b"b", &[1; 7]);
    let filler: Vec<_> =
        std::iter::from_fn(|| buffer.alloc(64, Kind::Mini, leaf(9)).ok()).collect();

    let result = super::buffer(
        &mut slot,
        &buffer,
        leaf(1),
        &classes,
        b"c",
        RecordState::Insert(&[1; 7]),
    );
    assert_eq!(result, Err(BufferError::Full(CircularBufferFull)));
    assert_eq!(capacity(&slot), 64);
    let mini = slot.as_ref().unwrap();
    assert_eq!(mini.page().len(), 2);
    assert_eq!(mini.lookup_marking(b"c"), Lookup::Miss);

    // Without a mini-page, a failed first allocation leaves the slot empty.
    let mut empty = None;
    let result = super::buffer(
        &mut empty,
        &buffer,
        leaf(2),
        &classes,
        b"x",
        RecordState::Delete,
    );
    assert_eq!(result, Err(BufferError::Full(CircularBufferFull)));
    assert!(empty.is_none());

    filler.into_iter().for_each(|block| buffer.free(block));
    buffer.free(slot.take().unwrap().block);
}

#[test]
fn invalid_records_fail_before_any_allocation() {
    let classes = classes();
    let buffer = CircularBuffer::new(4096, classes.as_slice(), 0).unwrap();
    let mut slot = None;
    let empty = super::buffer(
        &mut slot,
        &buffer,
        leaf(1),
        &classes,
        b"",
        RecordState::Delete,
    );
    assert_eq!(empty, Err(BufferError::Page(PageError::EmptyKey)));
    let huge = super::buffer(
        &mut slot,
        &buffer,
        leaf(1),
        &classes,
        b"k",
        RecordState::Insert(&[0; 400]),
    );
    assert_eq!(huge, Err(BufferError::Page(PageError::RecordTooLarge)));
    assert!(slot.is_none());
    assert_eq!(buffer.metrics().allocs, 0);
}

#[test]
fn growth_drops_cold_clean_records_and_keeps_dirty_ones() {
    let classes = classes();
    let buffer = CircularBuffer::new(4096, classes.as_slice(), 0).unwrap();
    let mut slot = None;
    let cache = |slot: &mut Option<Mini>, key: &[u8]| {
        super::buffer(
            slot,
            &buffer,
            leaf(1),
            &classes,
            key,
            RecordState::Cache(&[3; 7]),
        )
        .unwrap()
    };
    cache(&mut slot, b"cold");
    cache(&mut slot, b"hot");
    slot.as_ref().unwrap().lookup_marking(b"hot");
    // A dirty record that needs more room forces growth.
    buffer_record(&mut slot, &buffer, &classes, b"dirty", &[4; 20]);
    assert_eq!(capacity(&slot), 128);
    let mini = slot.as_ref().unwrap();
    assert_eq!(mini.lookup_marking(b"cold"), Lookup::Miss);
    assert!(matches!(mini.lookup_marking(b"hot"), Lookup::Found(_)));
    assert!(matches!(mini.lookup_marking(b"dirty"), Lookup::Found(_)));
    buffer.free(slot.take().unwrap().block);
}
