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
  ordinary record, sorted by key. Fenced pages place two structural slots before
  those records. Payloads grow backward from the end; obsolete payloads can leave
  holes. Free space is the contiguous gap between slots and the payload region.
- A `Page` is the buffer itself, viewed in place; it owns no memory. Readers
  share `&Page`, and a writer holds `&mut Page`.
- Capacities are multiples of 64 bytes, from 64 through 4096. The 64-byte step
  matches upstream's cache-line-sized page allocations, including mini-page
  sizes such as 448 bytes. 4096 bytes is upstream's default leaf page size.
- Integers are little-endian, and offsets are measured from the page start. Any
  buffer alignment works.
- A `Page` must come from `Page::init` or `Page::init_with_fences`. Stored bytes
  must be validated before they are used as a `Page`; reopening pages is future work.

## Header

Every leaf-level page uses the same 32-byte header, as upstream does:

| Bytes | Field | Meaning |
| --- | --- | --- |
| `0..2` | `count_and_flags` | Total slot count (low 12 bits), including fences; flags (high 4 bits), including `HAS_FENCES = 0x8000` |
| `2..4` | `capacity` | Page length in bytes |
| `4..6` | `remaining_size` | Free bytes between the slots and the payloads |
| `6..8` | `prefix_len` | Length of the key prefix stored once in the low fence and omitted from other keys; see [Prefix compression](#prefix-compression) |
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

- Ordinary slots are sorted by full key bytes. Each payload holds the key
  followed by its encoded value allocation, even when a tombstone hides it.
- Record kinds follow upstream: `Insert` (live value), `Delete` (tombstone),
  `Cache` (clean cached value), and `Phantom` (clean cached absence).
- Keys must be nonempty, as upstream requires. Values may be empty: an empty
  value is a live record, and value length never encodes absence. Upstream
  asserts that values are nonempty; peppydb allows empty values for
  presence-only records.

## Borrowed reads

`get` returns `Option<RecordRef>`: a miss is `None`, while known absence is a
stored `Delete` or `Phantom` state. `Insert` and `Cache` borrow their logical value,
including an empty value. `RecordState::kind()` and `value()` expose these two
views. `RecordRef` also borrows the key and reports the reference bit.

### Logical keys

`RecordRef::key` is a `KeyRef`: a borrowed logical key made of a prefix slice
and a suffix slice. The parts may borrow disjoint page regions, and either may be
empty. `prefix()` and `suffix()` return slices with the page lifetime, not the
lifetime of the view value. `len()`, `bytes()`, `cmp_bytes(&[u8])`, and the `Eq`
and `Ord` implementations use the concatenated bytes, so `ab` + `c` equals
`a` + `bc`. This lets a compressed base-page key compare directly with an
uncompressed mini-page key. `From<&[u8]>` builds a view with an empty prefix.
`KeyRef` has no implicit contiguous-slice conversion, because flattening can
require allocation; callers use `to_vec()` explicitly. Point operations and
range bounds still take full `&[u8]` keys.

`iter()` and iteration over `&Page` yield every stored record in key order,
including absence records. The iterator is exact-sized and fused. Reads never
change page bytes or reference bits. Borrowed results keep the page borrowed;
callers must retain its allocation and any later synchronization protection.

### Bounded iteration

`range(start, end)` returns ordinary records in `[start, end)`, including absence
states. `None` is unbounded, while an empty slice is finite. Equal bounds yield
an empty iterator and reversed bounds return `InvalidRange`. Query bounds may
extend outside the page's fences: only stored records in the intersection are
returned. Two lower-bound searches choose an index interval; the iterator borrows
only the page and retains no query slices. It is exact-sized, fused, and uses
`O(log n + output)` comparisons without allocation.

### Preview-first search

All point paths and range endpoints use one binary search. It returns
`Ok(index)` for a stored key and `Err(index)` with the insertion position
otherwise. Point operations decide hit or miss from that result without a second
key comparison; range endpoints use the index from either variant. Each probe
compares the meaningful first one or two preview bytes as slices, then loads the complete
key only on a tie. Zero padding is excluded for short keys. This preserves full
lexicographic byte ordering, including embedded zero bytes and preview collisions,
without relying on native integer ordering. Unequal previews avoid payload access.
This is a search fast path; no throughput claim is made without a workload.

On a page with a nonzero prefix, the search first classifies the full query
against the prefix. A query that diverges inside the prefix, or ends within it,
sorts before or after every record without a slot comparison. Otherwise the
binary search compares the query's remaining suffix with stored suffixes and
their previews. A query equal to the prefix has an empty suffix and is searched
normally. With a zero prefix, the search reduces to full-key comparison.

## Typed writes and reference hints

`try_insert_record` inserts any `RecordState` at a distinct nonempty key;
`try_insert` is the `Insert` convenience. Newly inserted absence states have
zero encoded value length. Any stored state makes a key a duplicate. Validation
checks key emptiness, range membership, standalone record size including fences,
duplicates, then contiguous space. Every rejected operation preserves the entire
page image.

New entries have a clear reference bit. `set_referenced` changes only that bit
under exclusive access and returns whether an entry exists. Cache policy will
decide when to mark accesses; raw reads remain pure.

### Replacement

`try_upsert` returns `Inserted` or `Replaced`. It preserves the reference bit of
an existing entry. `Delete` changes only the kind, retaining the payload and its
encoded value length. Later live writes may reuse that retained value region.
`Phantom` stores a zero-length value. Other smaller/equal writes overwrite the
existing value; shortening leaves a hole and reduces the encoded length. There
is no separate allocation-capacity field.

Writes size records physically: a new entry needs an eight-byte slot plus its
stored suffix (`key.len() - prefix_len`) and value. Logical key and value lengths
are checked against their 14-bit and 15-bit slot fields before narrowing.

Growing replacements allocate the complete stored-suffix/value payload from the gap,
redirect the existing slot, and leave the old payload as a hole. They require
no new slot but do not credit the old payload toward available space. `PageFull`
describes this physical path, even when only the final logical records would
fit. No write implicitly compacts. All fallible checks precede mutation.

All kind transitions are mechanically supported. A later overlay must prevent
cache fills from replacing newer dirty records. Generic pages do not infer
that a deletion has reached its base leaf.

## Removal and compaction

`remove` physically forgets any record kind. It immediately returns eight slot
bytes to the contiguous gap and leaves the old payload unreachable. Callers must
not use it to discard an unapplied deletion.

`free_bytes()` counts only the contiguous gap. `reclaimable_bytes()` counts holes
outside that gap. The payload boundary is `32 + 8 * slot_count + free_bytes()`;
reachable payloads are disjoint and lie above it, but need not start at it.

`compact()` packs all reachable payloads using one zero-initialized 4096-byte
scratch image and returns the recovered byte count. It preserves every record,
kind, reference bit, retained tombstone value, flag, and recovery/backing-leaf
field. It rebuilds physical slots because logical absence views hide retained
values and logical keys include the shared prefix. Each slot's physical size
depends on its role: the low fence stores its full key, the high fence and
ordinary records store suffixes, and infinite fences have no payload. The
stored prefix length stays unchanged. The completed image replaces the source;
compaction does not apply a
pending write or choose records to discard. Capacity handling remains with the
caller. An insertion rejected on a fragmented page leaves every byte unchanged.

## Page fences

`init_with_fences` reserves slots 0 and 1 for lower and upper bounds and copies
their keys. Ordinary records follow them. The header count includes these slots;
`len`, lookup, and iteration exclude them. Both unbounded fences still consume
16 directory bytes. `Page::init` creates an unfenced page with no such slots.

`Fence<K>` and `Fences<K>` share one bound definition between representations.
`init_with_fences` takes `Fences<&[u8]>` with full input keys, and `fences()`
returns `Option<Fences<KeyRef>>` with borrowed logical bounds. `K` has no trait
bound; derived traits apply when `K` supports them. `None` means the page has no
fence slots. Membership checks compare logical fence views with the full query.

`Fence::Key(b"")` is finite and distinct from `Fence::Unbounded`. Finite fences
use normal `Insert` slots with zero value length and reference bit. Their keys
may be empty, with a normal offset at or below page end. Infinite fences use
explicit offsets: `u16::MAX - 1` for lower and `u16::MAX` for upper, with all
remaining fields zero. Decoding never infers infinity from key length. Compaction
packs finite fence payloads and preserves infinity tags.

Finite bounds must satisfy `lower < upper`. The domain is `[lower, upper)`.
Every point operation checks membership, returning `KeyOutOfRange` for incorrect
routing and the normal miss result for an in-range miss. Empty ordinary keys
still return `EmptyKey` first. Fence initialization checks capacity, ordering,
then storage size; all failures preserve the supplied buffer. Writes budget
the fences when deciding `RecordTooLarge`, before checking duplicates or space.

Fences are immutable structural metadata. A split will initialize destination
pages with new bounds and must budget their actual fence bytes. Fences alone do
not guarantee that a page is a complete cache of its domain.

## Prefix compression

A fenced page stores a key prefix shared by its whole domain once. For two
finite fences, `init_with_fences` selects their longest common prefix. Every key
in `[lower, upper)` starts with that prefix. Unfenced pages, unbounded or
one-sided fences, and fences with no common bytes use an empty prefix.

| Entry | Stored key bytes | Slot key length | Preview |
| --- | --- | --- | --- |
| Finite low fence | Full key | Full length | First two full-key bytes |
| Finite high fence | Suffix after the prefix | Full length | First two suffix bytes |
| Ordinary record, any kind | Suffix after the prefix | Full length | First two suffix bytes |
| Unbounded fence | None; offset tag | Zero | Zero |

The prefix bytes are the first `prefix_len` bytes of the low fence's payload, so
they need no separate allocation. Values follow the stored suffix. For example,
bounds `[acct/100, acct/200)` select `acct/`. Records `acct/123` and `acct/150`
store `123` and `150`, and the high fence stores `200`. With one-byte values on
a 128-byte page, compression saves 15 bytes: 45 free bytes instead of 30.

Invariants:

- The prefix is fixed while the fences are fixed. Insertion, removal, and
  compaction never recompute it from current records. A split must rebuild
  destination pages with their own fences and prefixes.
- A nonzero prefix requires two finite fences, must not exceed their common
  prefix, and matches every ordinary logical key. A zero prefix is always a valid
  representation, even when the fences would permit compression; reads and
  writes honor the stored length.
- Decoding recognizes infinity tags before subtracting the prefix or slicing a
  payload. A zero prefix never follows an infinite fence's offset.
- An ordinary key may equal the prefix. It then has an empty stored suffix and,
  with an empty value, a zero-byte payload whose offset may equal the payload
  boundary. Its logical key is still nonempty.

Physical size uses stored bytes, not logical views. With `p = prefix_len`:

```text
fence_bytes = 16 + lower.len() + (upper.len() - p)   // finite fences
packed_size = 32 + fence_bytes + sum(8 + (key_len - p) + stored_value_len)
```

An unbounded fence keeps its eight-byte slot and has no payload.
`stored_value_len` includes value bytes retained by tombstones.

Search classifies a query against the prefix once and then compares suffixes;
see [Preview-first search](#preview-first-search). Reads return borrowed
`KeyRef` views whose prefix borrows the low-fence payload; no read allocates.

## `Page` as a base type

The public `page` module uses private `layout`, `key`, `record`, `read`, `write`,
`compact`, and `fence` submodules. They implement the same `Page` type and keep
encoding details private; `KeyRef` is the public logical-key view. Unit tests use
independent byte decoding alongside ordered-map checks, and an independent
fixture encoder builds compressed pages with any stored prefix length. The
mixed-operation model owns full logical keys and retained physical values,
derives the fence prefix and suffix lengths itself, and tracks the contiguous
gap from operation history. It checks errors, logical states, ranges, references,
the stored prefix length, suffix previews, payload overlap, and full byte
accounting after each operation, across representative capacities and fence
profiles, including compressed ones.

`Page` holds what every leaf-level page shares: the header, the slot directory,
and the record operations. Later stacks add role types that wrap `Page`, expose
the header fields their role uses, and enforce role-specific rules:

| Role (planned) | Adds to `Page` |
| --- | --- |
| `BasePage` | Requires fences; only `Insert` and `Delete` records; no backing leaf |
| `MiniPage` | A size class; any record kind; `backing_leaf` set to its base leaf; no fences |
| Full-page cache | A whole in-memory copy of a base leaf, with `backing_leaf` set |

All roles share one format, so records and whole pages move between roles
without format conversion. Inner nodes use a separate format, which the tree
structure design will define.

## Staged integration

[Plan 0004](../plans/0004-bf-tree-page-mutation.md) delivers the mutable, fenced,
range-aware page. [Plan 0005](../plans/0005-bf-tree-page-prefix.md) delivers
fence-derived prefix compression and borrowed logical keys.

The page primitive makes no base/mini/cache role decisions. Growth, merging,
splitting, and selection of records during rebuild belong to later components.
All mutations require exclusive access. Page operations do not log, advance
the LSN, or change the snapshot version. Future concurrency must protect borrowed
results and relocation; explicit reference updates are not synchronization points.
The current unaligned byte representation does not establish a shared-reader
reference-bit update protocol.

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
- [BF-Tree paper](../references.md#bf-tree-paper), §3.5: fence-derived prefix
  compression and look-ahead (preview) bytes. The
  [upstream page explanation](../explanations/bf-tree-upstream-pages.md#fences-prefixes-and-previews)
  describes upstream's stored logical lengths, full low fence, suffix-only high
  fence and records, and suffix previews, which peppydb follows. peppydb differs
  by keeping finite empty fences distinct from infinity, checking fence
  membership on every point operation, and returning segmented `KeyRef` views.
- Figure 3 of the paper shows a 14-bit value length and a separate fence bit.
  peppydb follows upstream's current slot packing.
