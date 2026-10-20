# Inner nodes in Microsoft's BF-Tree

This walkthrough describes Microsoft revision
[`f442e6172bf7e5483ff320d8e3ac16a70e31bbd6`][upstream]. It complements the
[leaf-page walkthrough](bf-tree-upstream-pages.md) and the
[BF-Tree overview](bf-tree.md). peppydb's proposed inner-node contract belongs
in [plan 0006](../plans/0006-bf-tree-inner-node.md).

## A separate representation for routing

The paper's §3.4 describes inner nodes that normally stay in memory and use
optimistic latch coupling. The inspected implementation gives them a separate
`InnerNode` format; they do not store leaf record kinds or variable-length values.

On its normal non-`shuttle` layout, the header is 32 bytes. It contains node
metadata, a version lock, a disk offset, and snapshot state. A root flag shares
the snapshot-version word. `NodeMeta` records whether the children are leaves.
The implementation allocates 4096-byte inner nodes.

Each `InnerKVMeta` uses eight bytes:

```text
| payload offset: 2 B | full key length: 2 B | first four key bytes: 4 B |
```

Those four bytes are part of the stored key, rather than a duplicate preview.
The payload holds the remaining key bytes followed by an eight-byte child
reference. A key of at most four bytes has no key bytes in the payload.

This differs from the leaf representation's two-byte preview and shared,
fence-derived prefix. The current inner-node header has no page-prefix field
or fence slots, despite the paper's broader discussion of layout optimizations
in §3.5.

## Leftmost child and routing

A node with `n` separators needs `n + 1` child references. Upstream represents
the extra leftmost child as slot zero with an empty dummy key. Searches skip
that key, but the metadata count includes its slot. The dummy entry consumes
eight metadata bytes and eight payload bytes.

For separators `m` and `t`, routing is:

```text
key < m       -> child 0
m <= key < t  -> child 1
t <= key      -> child 2
```

`lower_bound` returns the child-bearing slot for the greatest separator not
greater than the query, or slot zero when none exists. Equality routes right
of the matching separator. It compares meaningful inline key bytes first,
then compares key tails when needed.

`get_full_key` allocates a `Vec<u8>` and appends the inline bytes and stored tail.
The search itself compares these parts directly. Child values can be unaligned,
so `get_value` uses `read_unaligned` and returns a copied `PageID`.

`PageID` is a packed `u64`: inner references can be raw memory addresses, while
leaf/mini-page IDs have a distinguishing bit set. The node metadata also records
the child kind. Allocation, pointer validity, and locking are managed outside
the packed accessors.

## Insertion and splitting

`insert` reserves space for the slot, key tail, and child reference, shifts the
directory, and appends the payload. A duplicate separator updates its child
reference, although the method performs its space check before reaching that
duplicate-key path. `update_at_pos` provides direct child replacement.

`split` chooses its boundary by metadata count. The selected separator is
returned for promotion, and its child becomes the new right node's leftmost
child. Following entries go to the right node; the left node is consolidated.
The incoming separator is not part of this method's split calculation.

`consolidate` collects owned key/child pairs, reinitializes the node, and
reinserts the pairs. This repacks payloads and updates snapshot metadata. A
builder owns newly allocated memory until it transfers that allocation to the
tree. Parent insertion and root growth are separate tree operations.

Count-based selection is simple, but variable-length separator keys can produce
unequal byte occupancy. Any alternative selection policy must still preserve
the promotion rule and all child references; balancing records alone is not
enough to establish those structural guarantees.

## Sources

- [BF-Tree paper](../references.md#bf-tree-paper), §§3.4–3.5.
- [`src/nodes/inner_node.rs`][inner]: `InnerKVMeta`, `InnerNodeBuilder`,
  `reinitialize`, `lower_bound`, `insert`, `update_at_pos`, `consolidate`, and
  `split`.
- [`src/nodes/page_id.rs`][page-id]: pointer/ID encoding.
- [`src/nodes/node_meta.rs`][node-meta]: child-kind flag and counts.
- [`src/nodes/mod.rs`][nodes]: default node sizes and key-length limits.

[upstream]: https://github.com/microsoft/bf-tree/tree/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6
[inner]: https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/nodes/inner_node.rs
[page-id]: https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/nodes/page_id.rs
[node-meta]: https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/nodes/node_meta.rs
[nodes]: https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/nodes/mod.rs
