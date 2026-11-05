use super::*;

fn set_bits(refs: &RefBits) -> Vec<usize> {
    (0..512).filter(|&index| refs.is_set(index)).collect()
}

#[test]
fn mark_sets_exactly_one_bit() {
    let refs = RefBits::default();
    refs.mark(70);
    refs.mark(70);
    refs.mark(0);
    assert_eq!(set_bits(&refs), [0, 70]);
}

#[test]
fn insert_shifts_bits_at_and_above_the_index_across_words() {
    for index in [0, 5, 63, 64, 65, 127, 128, 507] {
        let mut refs = RefBits::default();
        let marked = [0, 4, 5, 62, 63, 64, 100, 127, 128, 200, 506, 507];
        marked.iter().for_each(|&bit| refs.mark(bit));
        refs.insert(index);

        let expected: Vec<_> = marked
            .iter()
            .map(|&bit| if bit >= index { bit + 1 } else { bit })
            .collect();
        assert_eq!(set_bits(&refs), expected, "insert at {index}");
        assert!(!refs.is_set(index));
    }
}

#[test]
fn clear_resets_every_bit() {
    let mut refs = RefBits::default();
    (0..512).step_by(7).for_each(|bit| refs.mark(bit));
    refs.clear();
    assert!(set_bits(&refs).is_empty());
}

#[test]
fn concurrent_marks_are_all_kept() {
    let refs = RefBits::default();
    std::thread::scope(|scope| {
        for thread in 0..4 {
            let refs = &refs;
            scope.spawn(move || {
                for bit in (thread..508).step_by(4) {
                    refs.mark(bit);
                }
            });
        }
    });
    assert_eq!(set_bits(&refs), (0..508).collect::<Vec<_>>());
}
