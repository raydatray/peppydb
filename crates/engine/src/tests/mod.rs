mod batch;
mod checkpoint;
mod crash;
mod external;
mod open;
mod replica;
mod write;

use crate::{Options, TreeConfig};

/// Tiny nodes, so trees split and evict early.
pub(super) fn small() -> Options {
    Options {
        tree: TreeConfig {
            circular_buffer_bytes: 64 << 10,
            ..TreeConfig {
                leaf_capacity: 256,
                inner_capacity: 128,
                max_key_len: 8,
                max_value_len: 16,
                ..TreeConfig::default()
            }
        },
        ..Options::default()
    }
}
