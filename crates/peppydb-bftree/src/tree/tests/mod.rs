use super::*;

mod config;

fn config(
    leaf_capacity: usize,
    inner_capacity: usize,
    max_key_len: usize,
    max_value_len: usize,
) -> TreeConfig {
    TreeConfig {
        leaf_capacity,
        inner_capacity,
        max_key_len,
        max_value_len,
    }
}
