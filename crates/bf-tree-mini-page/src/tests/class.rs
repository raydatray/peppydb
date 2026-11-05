use super::*;

/// Leaf capacity, key limit, and value limit.
type Limits = (usize, usize, usize);

#[test]
fn the_ladder_follows_the_derived_largest_class() {
    let rows: [(Limits, &[usize]); 3] = [
        ((4096, 64, 1024), &[64, 128, 256, 512, 1024, 2048, 2688]),
        ((512, 16, 32), &[64, 128, 256, 320]),
        ((128, 8, 0), &[64]),
    ];
    for ((l, k, v), ladder) in rows {
        let classes = SizeClasses::derive(l, k, v).unwrap();
        assert_eq!(classes.as_slice(), ladder, "({l}, {k}, {v})");
        assert_eq!(classes.largest(), *ladder.last().unwrap());
        for &class in classes.as_slice() {
            assert!(
                bf_tree_page::Page::check_capacity(class).is_ok(),
                "class {class}"
            );
        }
    }

    // M = 64, but a maximum record needs 32 + 8 + 1 + 37 = 78 bytes.
    assert!(SizeClasses::derive(192, 1, 37).is_err());
    // Keys and values larger than the leaf.
    assert!(SizeClasses::derive(4096, 1024, 0).is_err());
    assert!(SizeClasses::derive(100, 8, 0).is_err());
}

#[test]
fn a_largest_class_that_is_a_power_of_two_is_not_repeated() {
    // 1024 - 5 * 16 - 0 - 24 = 920, floor64 = 896: not a power of two.
    // 640 - 5 * 8 - 32 - 24 = 544, floor64 = 512: a power of two.
    let classes = SizeClasses::derive(640, 8, 32).unwrap();
    assert_eq!(classes.as_slice(), [64, 128, 256, 512]);
}

#[test]
fn next_and_smallest_for_at_their_boundaries() {
    let classes = SizeClasses::derive(4096, 64, 1024).unwrap();
    assert_eq!(classes.next(64), Some(128));
    assert_eq!(classes.next(2048), Some(2688));
    assert_eq!(classes.next(2688), None);
    assert_eq!(classes.next(100), None);

    assert_eq!(classes.smallest_for(1), Some(64));
    assert_eq!(classes.smallest_for(32), Some(64));
    assert_eq!(classes.smallest_for(33), Some(128));
    // A maximum default record (8 + 64 + 1024 bytes) lands in 2048.
    assert_eq!(classes.smallest_for(1096), Some(2048));
    assert_eq!(classes.smallest_for(2656), Some(2688));
    assert_eq!(classes.smallest_for(2657), None);
}
