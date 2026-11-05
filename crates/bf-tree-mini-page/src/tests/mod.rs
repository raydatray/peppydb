use super::*;

mod class;
mod leaf;
mod mini;
mod model;
mod refs;
mod write;

use bf_tree_node_id::LeafId;

fn leaf(raw: u64) -> LeafId {
    LeafId::new(raw).unwrap()
}
