# BF-Tree inner nodes

`InnerNode` is the packed routing node of peppydb's [BF-Tree](architecture.md).
It stores sorted, variable-length separator keys and fixed-size child
references inside a caller-provided byte buffer. Inner nodes use their own
format: they do not wrap the [shared leaf `Page`](bf-tree-pages.md). They share
only the technique of viewing caller bytes through `zerocopy` and the borrowed
`KeyRef` logical-key view.

[Plan 0006](../plans/0006-bf-tree-inner-node.md) delivers this primitive. The
[upstream inner-node explanation](../explanations/bf-tree-upstream-inner-nodes.md)
describes the Microsoft implementation that informed it.

## Child identity

Children are opaque, typed IDs from `node_id`:

- `InnerId(u64)` and `LeafId(u64)` are distinct `#[repr(transparent)]` types.
  `new(u64) -> Option<Self>` accepts every value except the reserved `u64::MAX`,
  which matches the leaf page's no-backing-leaf sentinel. `raw()` returns the
  value. Zero is valid.
- `ChildRef` is `Inner(InnerId)` or `Leaf(LeafId)`. Equality includes the kind.
- IDs are neither addresses nor disk offsets. Allocation, resolution,
  uniqueness, and reuse belong to later owning components.

All children of one node have the same kind. The node stores the kind once in
its header and each child as an eight-byte raw ID. The node cannot verify that
a referenced node exists or has the right height; the owning tree must.

## Layout

```text
| header incl. C0 (16 B) | sorted slots --> | free gap | <-- [key tail][right child ID] |
```

Integers are little-endian, offsets are measured from the node start, and any
buffer alignment works. Capacities are multiples of 64 bytes from 64 through
4096, as for `Page`. The in-memory tree will normally use 4096-byte nodes;
smaller capacities make boundary tests inexpensive.

### Header

| Bytes | Field | Meaning |
| --- | --- | --- |
| `0..2` | `count` | Number of separators, excluding the first child |
| `2..4` | `capacity` | Node length in bytes |
| `4..6` | `remaining_size` | Free bytes between the slots and the payloads |
| `6..8` | `children_kind` | `0` = leaf IDs, `1` = inner IDs |
| `8..16` | `first_child` | Raw ID of the child left of every separator |

The header is structural only. Root identity, the optimistic-lock version, disk
location, and recovery metadata belong to a future owning frame, side table, or
tree, never to the node image. This is an in-memory structural format, not a
durable-node contract. Plan 0006's agent notes record the comparison with
upstream's 32-byte header and dummy leftmost entry.

`InnerNode::init(bytes, first)` validates the capacity, zeroes the buffer, and
writes a node with one child and no separators. The first child fixes the node's
child kind. An invalid capacity leaves the buffer unchanged.

## Total reads

Methods that take `&self` must not panic or read outside the buffer for any
byte content. They clamp decoded counts, offsets, and lengths to the buffer and
may return meaningless results for bytes that no valid node contains. This
differs from `Page`, whose reads may panic on invalid bytes. The reason is
future optimistic latch coupling: a reader may observe a torn node and rely on
a version re-check to discard the result.

Mutating methods assume the bytes came from `init` and this type's operations.
Validating arbitrary stored bytes is a separate, future boundary.
