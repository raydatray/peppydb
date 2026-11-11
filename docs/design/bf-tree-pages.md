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
- A `Page` must come from `Page::init` or `Page::init_with_fences`, or from
  `Page::validate` for bytes of unknown provenance; see
  [Stored-page validation](#stored-page-validation).

## Header

Every leaf-level page uses the same 32-byte header, as upstream does:

| Bytes | Field | Meaning |
| --- | --- | --- |
| `0..2` | `count_and_flags` | Total slot count (low 12 bits), including fences; flags (high 4 bits), including `HAS_FENCES = 0x8000` |
| `2..4` | `capacity` | Page length in bytes |
| `4..6` | `remaining_size` | Free bytes between the slots and the payloads |
| `6..8` | `prefix_len` | Length of the key prefix stored once in the low fence and omitted from other keys; see [Prefix compression](#prefix-compression) |
| `8..16` | `backing_leaf` | Base leaf that a mini-page overlays; `u64::MAX` for base pages and full pages; see [Backing leaf](#backing-leaf) |
| `16..24` | `write_seq` | `U64`: the write sequence of the tree write-through that stored the page; zero in pages never stored, including mini-pages (plan 0018) |
| `24..28` | `leaf` | `U32`: the leaf that a stored base page belongs to; `u32::MAX` (`NO_LEAF`) for none; see [Leaf stamp and checksum](#leaf-stamp-and-checksum) |
| `28..32` | `crc` | `U32`: CRC32C of the whole page with this field taken as zero |

Bytes `0..16` describe the page structure. Bytes `16..32` hold storage and
recovery state: `write_seq` orders the stored copies of a leaf, and `leaf`
and `crc` identify and protect a page in the store. The
[persistence design](bf-tree-persistence.md#base-page-identity-and-the-page-checksum)
records how the tree uses them. A smaller mini-page header without this
state is deferred: it would not shrink upstream's allocation sizes, and the
state would need another home.

Bytes `24..32` held `snapshot_version` until it was replaced in
[plan 0017](../plans/0017-bf-tree-store-format.md). The field followed
upstream, which uses it for CPR (Concurrent Prefix Recovery) checkpoints:
per-page copy-on-write by checkpoint version. peppydb does not use CPR, so
the field was free; shadow paging (plan 0018) takes its role, and the 32-bit
leaf stamp and the 32-bit page CRC32C took its bytes.

### Backing leaf

`backing_leaf()` returns `Option<LeafId>`, and `set_backing_leaf(Option<LeafId>)`
writes the field. `LeafId` comes from the `bf-tree-node-id` crate. The stored
sentinel for `None` is `u64::MAX`, which is also `LeafId`'s reserved raw value,
so no real leaf ID can collide with it. `init` and `init_with_fences` store
`None`. Splits and batch merges copy the field from the source page to every
output. The page itself gives the field no meaning; role types decide when it
must be set (see [`Page` as a base type](#page-as-a-base-type)).

### Leaf stamp and checksum

The page crate provides the fields; the tree gives them their roles.

- `leaf()` returns `Option<LeafId>`. `set_leaf(Option<LeafId>)` writes the
  field and returns `PageError::LeafIdTooLarge` for a raw ID of `u32::MAX`
  or more. The stored sentinel for `None` is `u32::MAX`, so an all-zero
  header reads as leaf 0, not `None`.
- `checksum()` computes the CRC32C of every page byte with the `crc` field
  (`CRC_RANGE`, bytes `28..32`) taken as zero. `stored_checksum()` reads the
  field, `seal()` stores `checksum()` in it, and `verify_checksum()`
  compares the two. `seal` must be the last change to a page.
- The free function `verify_stored_checksum(&[u8])` checks raw bytes that
  may not be a valid page. It reads only the `crc` field and computes the
  CRC over the slice length, never over a `capacity` read from the header.
- `init` and `init_with_fences` set `write_seq` to zero, `leaf` to `None`,
  and `crc` to zero.
  Split and batch-merge outputs copy `backing_leaf`, `write_seq`, `leaf`,
  and `crc` from the source page, and in-place operations leave them unchanged,
  so after any change the stored checksum is stale. No page operation
  seals.
- `write_seq()` and `set_write_seq(seq)` read and write the write
  sequence. Plan 0018 renamed the field from the inactive `lsn`.
- Only the tree's `write_through` stamps (`write_seq`, then `leaf`) and
  seals, immediately before a base page is written to the store. A stored base page therefore names its
  leaf and carries a valid checksum. Mini-pages and in-memory full pages do
  not maintain the checksum. `Page::validate` stays structural: it checks
  neither field.

## Slots and records

Each slot uses 8 bytes:

| Bytes | Meaning |
| --- | --- |
| `0..2` | Payload offset from the page start |
| `2..4` | Record kind (high 2 bits) and key length (low 14 bits) |
| `4..6` | Reserved bit (high bit, always zero) and value length (low 15 bits) |
| `6..8` | First two key bytes (preview), zero-padded |

- Bit 15 of bytes `4..6` (the `ref_and_value_len` word) is reserved. Every page
  operation writes it as zero, and `Page::validate` rejects a finite slot that
  sets it. Upstream stores a reference bit there. peppydb keeps reference bits
  outside the page format: readers will hold only a shared leaf lock, and safe
  Rust cannot set a bit inside bytes that other readers are reading. Mini-page
  reference bits live beside each mini-page in `RefBits` (crate
  `bf-tree-mini-page`; see [BF-Tree mini-pages](bf-tree-mini-page.md)).
  Upstream also clears the bit on every copy, so the format loses no durable
  state.

- Ordinary slots are sorted by full key bytes. Each payload holds the key
  followed by its encoded value allocation, even when a tombstone hides it.
- Record kinds follow upstream: `Insert` (live value), `Delete` (tombstone),
  `Cache` (clean cached value), and `Phantom` (clean cached absence).
  `RecordKind::is_dirty()` is true for `Insert` and `Delete`, which can be
  newer than the base page. `Cache` and `Phantom` are clean: they only repeat
  what the base page holds.
- Keys must be nonempty, as upstream requires. Values may be empty: an empty
  value is a live record, and value length never encodes absence. Upstream
  asserts that values are nonempty; peppydb allows empty values for
  presence-only records.

## Borrowed reads

`get` returns `Option<RecordRef>`: a miss is `None`, while known absence is a
stored `Delete` or `Phantom` state. `Insert` and `Cache` borrow their logical value,
including an empty value. `RecordState::kind()` and `value()` expose these two
views. `RecordRef` also borrows the key. `RecordRef::shorten` returns the same
record with a shorter lifetime; it lets records from two sources with different
lifetimes form one iterator item type, for example in a [`Merge`](#merging-sorted-records).

`find_index(key)` returns the record index of a stored key, or `None` on a miss.
It rejects empty and out-of-range keys as `get` does. Indices exclude fence
slots. `record_at(index)` returns the record at an index in key order and
panics if `index >= len()`.

### Routed lookups

`get` calls `check_key`, which decodes both fences and compares the key with
them, on every call. `get_routed(key)` returns the same result as `get` for a
key that a validated route brought to this page, without that check: such a
key is nonempty and inside the fences by construction. A `debug_assert`
keeps the check in debug builds, so debug test runs still catch routing bugs.
The checked methods stay for callers whose keys did not come from a route.
[Plan 0023](../plans/0023-bf-tree-io-perf.md) added it.

`Tree::get_with` uses `get_routed` on the base page and on a full page. Before
plan 0023 it called `check_key` and then `get`, so it checked the fences
twice. Measured A/B (Docker on macOS (aarch64), indicative: Docker Desktop
29.2.1 on macOS 27.0.1, Apple M4 Pro host, Linux VM with 12 CPUs, kernel
6.12.72-linuxkit; release build; `baseline_components`; 1,000,000 keys of
8 bytes, 16-byte values, 32 MiB circular buffer; four alternating runs):
`get_with` on a hot leaf took 189.7 to 192.3 ns before and 184.2 to
185.1 ns after, about 3 percent less. `find_index_routed`, also planned, had
no caller on a routed path and was not added.

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
change page bytes. Borrowed results keep the page borrowed;
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
compares the slot's two-byte zero-padded preview, read as a big-endian `u16`,
with a query word that the search computes once from the query suffix
(`preview_word`). It loads the stored suffix and compares slices only when the
words are equal (`compare_preview` in `lib/bf-tree-page/src/read.rs`).
Unequal previews avoid payload access and out-of-line `memcmp` calls.

**Why integer order is safe.** For two byte strings, a strict inequality of
their zero-padded big-endian words is their lexicographic order. At the first
differing byte, either both bytes are real, or the zero-padded byte ends a
shorter string that is a prefix of the other, which therefore sorts first. Equal
words decide nothing: an embedded zero byte and padding look the same, so the
search then compares the stored bytes. This preserves full lexicographic order,
including embedded zero bytes, short keys, and preview collisions.

`KeyRef::cmp_bytes` uses the same rule through `cmp_short`
(`lib/bf-tree-page/src/key.rs`). It compares the prefix and the suffix parts
in turn. When the common length of two parts is at most 8, it compares them as
zero-padded big-endian `u64` words (`padded_word`) and then by length; longer
parts use slice order. Inner-node routing uses the same rule for its 4-byte
inline bytes ([inner nodes](bf-tree-inner-nodes.md#search)).

Plan 0023 measured the change against the previous slice comparisons (Docker on
macOS (aarch64), indicative, as above; release builds; `baseline_components`;
1,000,000 keys of 8 bytes, 16-byte values, 32 MiB circular buffer; four
alternating runs): descent 134.6 to 140.3 ns before and 83.2 to 85.2 ns after;
`get_with` on a hot leaf 186.2 to 188.9 ns before and 78.7 to 87.7 ns after;
random `put` 606 to 663 ns before and 538 to 574 ns after. With 64-byte keys:
descent about 198 ns to about 133 ns, `get_with` about 209 ns to about 86 ns,
and `put` unchanged at about 1.6 µs. The benchmark keys are hashed, so previews
decide most probes. Keys with long shared prefixes should gain less, because
more probes can tie on the preview and compare the stored bytes; this case was
not measured.

On a page with a nonzero prefix, the search first classifies the full query
against the prefix. A query that diverges inside the prefix, or ends within it,
sorts before or after every record without a slot comparison. Otherwise the
binary search compares the query's remaining suffix with stored suffixes and
their previews. A query equal to the prefix has an empty suffix and is searched
normally. With a zero prefix, the search reduces to full-key comparison.

## Typed writes

`try_insert_record` inserts any `RecordState` at a distinct nonempty key;
`try_insert` is the `Insert` convenience. Newly inserted absence states have
zero encoded value length. Any stored state makes a key a duplicate. Validation
checks key emptiness, range membership, standalone record size including fences,
duplicates, then contiguous space. Every rejected operation preserves the entire
page image.

The page stores no access-tracking state. Cache policy records accesses outside
the page format; see [Slots and records](#slots-and-records). Reads remain pure.

### Replacement

`try_upsert` returns `Inserted` or `Replaced`. `upsert_indexed` has the same
contract and also returns the record index that the key now occupies. On
`Inserted`, every record at or above that index has moved up by one. `Delete`
changes only the kind, retaining the payload and its
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
kind, retained tombstone value, flag, and recovery/backing-leaf field. It rebuilds physical slots because logical absence views hide retained
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
use normal `Insert` slots with zero value length and a clear reserved bit. Their keys
may be empty, with a normal offset at or below page end. Infinite fences use
explicit offsets: `u16::MAX - 1` for lower and `u16::MAX` for upper, with all
remaining fields zero. Decoding never infers infinity from key length. Compaction
packs finite fence payloads and preserves infinity tags.

Finite bounds must satisfy `lower < upper`. The domain is `[lower, upper)`.
Every checked point operation checks membership, returning `KeyOutOfRange` for
incorrect routing and the normal miss result for an in-range miss.
`get_routed` asserts membership only in debug builds
([Routed lookups](#routed-lookups)). Empty ordinary keys
still return `EmptyKey` first. Fence initialization checks capacity, ordering,
then storage size; all failures preserve the supplied buffer. Writes budget
the fences when deciding `RecordTooLarge`, before checking duplicates or space.

Fences are immutable structural metadata. A split initializes destination
pages with new bounds and budgets their actual fence bytes; see
[Splitting](#splitting). Fences alone do not guarantee that a page is a
complete cache of its domain.

## Splitting

`split_with_insert(right_storage, key, state)` splits a fenced page together
with one pending record and returns `LeafSplit { separator, right }`. `self`
becomes the left output with fences `[lower, separator)`; `right_storage`
becomes the right output with fences `[separator, upper)`. The contract
mirrors [`InnerNode::split_with_insert`](bf-tree-inner-nodes.md#splitting-with-a-pending-separator),
so the tree treats both levels the same way.

The separator is the full key of the right output's first record. It is
therefore the right output's lower fence, the left output's upper fence, and
the parent's new separator. Routing agrees by construction: inner nodes route
`key >= separator` right, and fences are lower-inclusive.

Selection works over a *virtual sequence*: the stored records merged in key
order with a sorted batch of pending records by [`Merge`](#merging-sorted-records).
A pending record replaces a stored record with the same key.
`split_with_insert` passes a batch of one; `merge_batch_split` passes a larger
batch to the same selector (see [Batch merges](#batch-merges)). For `m` virtual records and candidates `j` in `1..m`:

```text
prefix_left  = implied prefix of [lower, key_j)
prefix_right = implied prefix of [key_j, upper)
left_used    = 32 + fence_bytes(lower, key_j)  + sum(8 + key_len - prefix_left  + value_len), i < j
right_used   = 32 + fence_bytes(key_j, upper)  + sum(8 + key_len - prefix_right + value_len), i >= j
score        = (max(left_used, right_used), |left_used - right_used|, j)
```

`value_len` is the logical value length, so absence states count zero bytes.
Candidates whose outputs exceed the capacity are discarded; the rest minimize
the score. One pass computes the totals and a second evaluates each candidate.
Every key already shares the source page's stored prefix with both fences, so
each prefix comparison starts after it. The computed sizes are exact: each
output's `capacity - free_bytes()` equals its score component.

Both outputs are built in 4096-byte stack scratch images with
`init_with_fences` and `try_insert_record`, then copied. Each output therefore
gets the prefix its own fences imply, even if the source stored a shorter
one. Record kinds are preserved. Tombstones lose retained
value bytes, as in any rebuild. Both outputs inherit the source's
`backing_leaf`, `write_seq`, `leaf`, and `crc` fields; other header flags
are not copied.

Validation checks the right buffer's capacity (`InvalidCapacity`), that it
equals the source capacity (`CapacityMismatch`), that the source is fenced
(`InvalidFences`), the pending record's key and size as in `try_insert_record`,
duplicates (`DuplicateKey`), and finally `CannotSplit` when fewer than two
virtual records exist or no candidate fits. Every error preserves both
buffers. The page does not allocate a leaf ID or update a parent. The
admission rule that makes `CannotSplit` unreachable in the tree is described in
[BF-Tree structure](bf-tree-structure.md#admission-rule).

## Merging sorted records

`Merge<A, B>` is a public iterator over two sorted `RecordRef` sequences,
built with `Merge::new(older, newer)`. It yields records in key order. On an
equal key it yields the record from `newer` and skips the one from `older`.
Both sides must yield the same `RecordRef` lifetime; a side that borrows longer
converts with `.map(RecordRef::shorten)`. `Merge` is fused when both sides are
fused, and it does not allocate. Splits and batch merges build their virtual
sequence with it, with the page's records as `older`.

## Batch merges

Three methods merge a sorted batch of dirty records into a fenced base page:

| Method | Result |
| --- | --- |
| `merge_batch_fits(batch)` | `Ok(true)` if the merged page fits in place |
| `merge_batch_in_place(batch)` | Rebuilds `self` with the batch applied, or returns `PageFull` |
| `merge_batch_split(right, batch)` | Splits the merged records into `self` (left) and `right`, as in [Splitting](#splitting) |

The merged sequence is the page's records with the batch applied. An `Insert`
adds or replaces a key. A `Delete` removes the key, and is a no-op if the page
does not hold the key. The output holds only `Insert` records, so a merge never
writes a tombstone to a base page.

```rust
Merge::new(self.iter(), batch.map(RecordRef::shorten))
    .filter(|record| record.state.kind() == RecordKind::Insert)
```

Every method checks the batch in one pass before it changes anything:

1. The page must be fenced, else `InvalidFences`. `merge_batch_split` first
   checks the right buffer as `split_with_insert` does (`InvalidCapacity`,
   `CapacityMismatch`).
2. For each record in order: the key must be nonempty and inside the fences,
   and the record must fit an empty page, with the same errors as
   `try_insert_record` (`EmptyKey`, `KeyOutOfRange`, `RecordTooLarge`).
3. Each record must be dirty, and each key must be strictly greater than the
   previous key. Otherwise the error is `PageError::InvalidBatch(reason)`.

Every error leaves every buffer unchanged. An empty batch leaves the page
byte-for-byte unchanged in place.

`merge_batch_fits` and `merge_batch_in_place` size the packed result: holes do
not count, because the rebuild packs the page. Sizes use the page's stored
prefix length. `merge_batch_in_place` builds the result in a 4096-byte stack
scratch image with `init_with_fences` and `try_insert_record`, then copies it
back. The page keeps its fences and gets the prefix they imply. It keeps its
`backing_leaf`, `write_seq`, `leaf`, and `crc` fields. `merge_batch_split` uses the
split selector, so it returns `CannotSplit` under the same conditions as
`split_with_insert`.

These methods are the single merge path into a base page: the tree merges a
mini-page's dirty records through them
([plan 0013](../plans/0013-bf-tree-mini-tree.md)).

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

## Stored-page validation

Every `Page` method except `validate` trusts its bytes: the methods assume an
image that this crate's operations produced, and they may panic on anything
else. `Page::validate(bytes) -> Result<&Page, PageError>` is the single entry
point for bytes that cross a trust boundary, such as pages read from a
[page store](bf-tree-leaf-store.md). It never panics or reads outside the
buffer, and it accepts exactly the images the trusted operations produce,
fenced or not. It rejects any image where:

- the buffer length is not a legal capacity or differs from the header's
  `capacity` field;
- an unknown header flag is set, or the slot directory or the free gap does
  not fit;
- a payload starts before the end of the free gap or ends past the buffer, or
  two nonempty payloads overlap. An empty payload (a key equal to the page
  prefix with an empty value) is excluded from the overlap check, because it
  may start where a neighbour starts;
- a finite slot sets the reserved bit ("reserved slot bit is set");
- a fence slot carries a value, an unbounded fence has a key length, or the
  prefix length is nonzero without a finite low fence long enough to hold it;
- a record key is empty or shorter than the prefix, or a slot preview differs
  from the stored key bytes;
- finite fences are not ordered, or a prefix is stored with an unbounded fence;
- record keys are not strictly increasing or lie outside the fences.

The error is `PageError::Corrupt(reason)`, naming the first failed check.
Validation costs one pass over the slot directory plus a sort of the payload
ranges. After it succeeds, the page keeps the trusted contract: the tests
confirm that a validated page survives `get`, `iter`, `range`, `compact`,
`try_upsert`, and `remove` without panicking and stays valid afterwards, over
model pages mutated with random bit flips, byte overwrites, and truncation.

Validation is structural. It does not know the tree configuration, so the
tree checks key and value lengths against its limits separately when it
opens a store. It also ignores the `leaf` stamp and the `crc` field: the
tree verifies the checksum on the raw bytes before it validates, and checks
the stamp afterwards.

## `Page` as a base type

The `bf-tree-page` crate uses private `layout`, `key`, `record`, `read`,
`write`, `compact`, `fence`, `split`, `merge`, and `validate` modules. They implement the same `Page` type and keep
encoding details private; `KeyRef` is the public logical-key view. Unit tests use
independent byte decoding alongside ordered-map checks, and an independent
fixture encoder builds compressed pages with any stored prefix length. The
mixed-operation model owns full logical keys and retained physical values,
derives the fence prefix and suffix lengths itself, and tracks the contiguous
gap from operation history. It checks errors, logical states, ranges, the
clear reserved bit, the stored prefix length, suffix previews, payload overlap,
and full byte accounting after each operation, across representative capacities
and fence profiles, including compressed ones. Batch-merge tests compare
merges against an ordered map, and check that deletes of missing keys are
no-ops, that an empty batch changes nothing, and that every rejected batch
leaves both buffers unchanged.

`Page` holds what every leaf-level page shares: the header, the slot directory,
and the record operations. Each role adds rules on top of `Page`. With `L` the
tree's leaf page capacity and `M` the largest mini-page size class:

| Role | Status | Fences | `backing_leaf` | Record kinds | Capacity |
| --- | --- | --- | --- | --- | --- |
| Base page | Implemented as a plain `Page` | Required | `None` | `Insert` only | `L` |
| Mini-page | Implemented as `MiniPage` in `bf-tree-mini-page` | None | Its base leaf | Any | One size class, `64..=M` |
| Full page | Implemented as a plain `Page` in a `Kind::Full` block ([plan 0015](../plans/0015-bf-tree-caching.md)) | Required | `None` | `Insert` only | `L` |

A stored base page also carries its own leaf in `leaf` and a sealed `crc`
([Leaf stamp and checksum](#leaf-stamp-and-checksum)); mini-pages and full
pages in memory do not maintain them. A base page never stores a tombstone: a [batch merge](#batch-merges) drops a
`Delete` after it removes the base key. [BF-Tree mini-pages](bf-tree-mini-page.md)
defines the mini-page rules, size classes, and reference bits.

All roles share one format, so records and whole pages move between roles
without format conversion. Inner nodes use a separate format; see
[BF-Tree inner nodes](bf-tree-inner-nodes.md).

## Boundaries

The page primitive makes no base/mini/cache role decisions. It can split itself
and merge a batch once asked, but deciding when to split or merge, mini-page
growth, and selection of records during rebuild belong to other components.
All mutations require exclusive access (`&mut Page`), and reads require only
shared access (`&Page`). No read writes page bytes, so concurrent readers under
a shared leaf lock need no write protocol inside the page; access tracking
lives outside the format (see [Slots and records](#slots-and-records)).
Page operations do not log, stamp the write sequence or the leaf, or seal
the checksum.
Callers must protect borrowed results against relocation.

## References

- [Page operations in Microsoft's BF-Tree](../explanations/bf-tree-upstream-pages.md)
  explains the pinned implementation's layout, mutation, consolidation, and
  capacity-handling mechanisms.
- [BF-Tree paper](../references.md#bf-tree-paper), §3.2/Figure 3: leaf pages
  and mini-pages share one packed record format.
- [Microsoft implementation](../references.md#microsoft-bf-tree-implementation),
  revision `f442e6172bf7e5483ff320d8e3ac16a70e31bbd6`: `LeafNode` in
  `src/nodes/leaf_node.rs` has the same header size, the same fields and
  field order in bytes `0..24`, and the same slot packing. peppydb differs
  by encoding integers as explicit little-endian values, measuring payload
  offsets from the page start, capping pages at 4096 bytes (upstream allows
  up to 32 KiB), reserving the slot bit where upstream stores its reference
  bit, and storing `leaf` and `crc` in bytes `24..32`, where upstream stores
  `snapshot_version`.
- [BF-Tree paper](../references.md#bf-tree-paper), §3.5: fence-derived prefix
  compression and look-ahead (preview) bytes. The
  [upstream page explanation](../explanations/bf-tree-upstream-pages.md#fences-prefixes-and-previews)
  describes upstream's stored logical lengths, full low fence, suffix-only high
  fence and records, and suffix previews, which peppydb follows. peppydb differs
  by keeping finite empty fences distinct from infinity, checking fence
  membership on every checked point operation (`get_routed` checks it only in
  debug builds), and returning segmented `KeyRef` views.
- [Plan 0023](../plans/0023-bf-tree-io-perf.md): routed lookups and integer
  comparisons, with their measurements.
- Figure 3 of the paper shows a 14-bit value length and a separate fence bit.
  peppydb follows upstream's current slot packing, except for the reserved bit.
- [Plan 0011](../plans/0011-bf-tree-mini-page.md) retired the in-slot
  reference bit and added backing-leaf accessors, `Merge`, and batch merges.
- [Plan 0017](../plans/0017-bf-tree-store-format.md) replaced
  `snapshot_version` with the `leaf` stamp and the `crc` checksum; see the
  [persistence design](bf-tree-persistence.md).
