# Shared BF-Tree pages

`Page` is the storage format shared by every leaf-level page in peppydb's
[BF-Tree](architecture.md): base leaf pages on disk, full-page caches in memory,
and mini-pages. It stores sorted key/value records inside a caller-provided byte
buffer.

## Page layout

```text
| header (32 B) | slot directory --> | free space | <-- key/value payloads |
```

- The header starts the page. The slot directory follows it, with one slot per
  record, sorted by key. Payloads grow backward from the end of the page. Free
  space is the gap between the slots and the payloads.
- A `Page` is the buffer itself, viewed in place; it owns no memory. Readers
  share `&Page`, and a writer holds `&mut Page`.
- Capacities are multiples of 64 bytes, from 64 through 4096. The 64-byte step
  matches upstream's cache-line-sized page allocations, including mini-page
  sizes such as 448 bytes. 4096 bytes is upstream's default leaf page size.
- Integers are little-endian, and offsets are measured from the page start. Any
  buffer alignment works.
- A `Page` must come from `Page::init`. Stored bytes must be validated before
  they are used as a `Page`; reopening pages is future work.

## Header

Every leaf-level page uses the same 32-byte header, as upstream does:

| Bytes | Field | Meaning |
| --- | --- | --- |
| `0..2` | `count_and_flags` | Record count (low 12 bits) and flags (high 4 bits) |
| `2..4` | `capacity` | Page length in bytes |
| `4..6` | `remaining_size` | Free bytes between the slots and the payloads |
| `6..8` | `prefix_len` | Length of the key prefix shared by the page; zero until prefix compression |
| `8..16` | `backing_leaf` | Base leaf that a mini-page or full-page cache belongs to; `u64::MAX` if none |
| `16..24` | `lsn` | Log sequence number (LSN) for recovery; inactive |
| `24..32` | `snapshot_version` | Checkpoint snapshot version; inactive |

Bytes `0..16` describe the page structure. Bytes `16..32` hold recovery state,
which the logging and checkpoint design will define. A smaller mini-page header
without recovery state is deferred to that design: it would not shrink
upstream's allocation sizes, and the recovery state would need another home.

## Slots and records

Each slot uses 8 bytes:

| Bytes | Meaning |
| --- | --- |
| `0..2` | Payload offset from the page start |
| `2..4` | Record kind (high 2 bits) and key length (low 14 bits) |
| `4..6` | Reference bit (high bit) and value length (low 15 bits) |
| `6..8` | First two key bytes (preview), zero-padded |

- Slots are sorted by full key bytes. Each payload holds the key followed by the
  value.
- Record kinds follow upstream: `Insert` (live value), `Delete` (tombstone),
  `Cache` (clean cached value), and `Phantom` (clean cached absence).
- Keys must be nonempty, as upstream requires. Values may be empty: an empty
  value is a live record, and value length never encodes absence. Upstream
  asserts that values are nonempty; peppydb allows empty values for
  presence-only records.

## `Page` as a base type

`Page` holds what every leaf-level page shares: the header, the slot directory,
and the record operations. Later stacks add role types that wrap `Page`, expose
the header fields their role uses, and enforce role-specific rules:

| Role (planned) | Adds to `Page` |
| --- | --- |
| `BasePage` | Fences (the page's key-range bounds); only `Insert` and `Delete` records; no backing leaf |
| `MiniPage` | A size class; any record kind; `backing_leaf` set to its base leaf; no fences |
| Full-page cache | A whole in-memory copy of a base leaf, with `backing_leaf` set |

All roles share one format, so records and whole pages move between roles
without format conversion. Inner nodes use a separate format, which the tree
structure design will define.

## References

- [Page operations in Microsoft's BF-Tree](../explanations/bf-tree-upstream-pages.md)
  explains the pinned implementation's layout, mutation, consolidation, and
  capacity-handling mechanisms.
- [BF-Tree paper](../references.md#bf-tree-paper), §3.2/Figure 3: leaf pages
  and mini-pages share one packed record format.
- [Microsoft implementation](../references.md#microsoft-bf-tree-implementation),
  revision `f442e6172bf7e5483ff320d8e3ac16a70e31bbd6`: `LeafNode` in
  `src/nodes/leaf_node.rs` has the same header fields, field order, and slot
  packing. peppydb differs by encoding integers as explicit little-endian
  values, measuring payload offsets from the page start, and capping pages at
  4096 bytes (upstream allows up to 32 KiB).
- Figure 3 of the paper shows a 14-bit value length and a separate fence bit.
  peppydb follows upstream's current slot packing.
