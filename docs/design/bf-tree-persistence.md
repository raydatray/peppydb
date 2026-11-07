# BF-Tree persistence: store format, checkpoints, and recovery

This document records how a tree's leaves are kept durable in a page store.
It covers the file layout, the checksummed superblock and its committed
state, page numbers and how they differ from leaf IDs, the identity stamp,
write sequence, and checksum in every stored page, shadow paging, the
checkpoint protocol, and how `Tree::open` selects the committed image after
a crash. It also records what is guaranteed today, with the test that backs
each guarantee, and what is not guaranteed yet.

[Plan 0017](../plans/0017-bf-tree-store-format.md) built the store format.
[Plan 0018](../plans/0018-bf-tree-checkpoint.md) built shadow paging,
checkpoints, and recovery on that format without changing its layout.
Related documents:
[leaf storage](bf-tree-leaf-store.md) (the `LeafMap`, `load_base`,
`write_through`, and the tree lifecycle),
[shared pages](bf-tree-pages.md) (the page header and validation), and
[concurrency](bf-tree-concurrency.md) (the lock order, including the
write-through gate, and the shareable store contract).

The code is in `crates/bf-tree-store/src/` (`lib.rs`: `PageNumber`,
`PageStore`, `StoreError`; `superblock.rs`: the codec; `file.rs` and
`mem.rs`: the backends), `crates/bf-tree-page/src/` (`layout.rs`: the
header; `lib.rs`: stamp, sequence, and checksum accessors), and
`crates/bf-tree/src/` (`leaf_map.rs`: the page table, `load_base`,
`write_through`, the superseded list, and the gate; `lib.rs`:
`Tree::checkpoint` and the commit step; `write.rs`: the sequence closure;
`open.rs`: selection and erasure).

## Terms

- **Page number:** the position of one page-sized region in the store
  (`PageNumber`). It says where a page is.
- **Leaf ID:** the identity of a leaf in the tree (`LeafId`). It says which
  leaf a page belongs to.
- **Stored page:** a base page as it is in the store.
- **Leaf stamp:** the 32-bit `leaf` header field of a stored page. It names
  the leaf that the page belongs to.
- **Write sequence (`write_seq`):** the number that one `write_through`
  call draws and stamps into every page it writes. A later write-through
  draws a higher number.
- **Operation sequence:** the number that a `put` or `remove` draws. It
  orders writes to records. It is separate from the write sequence, which
  orders page versions.
- **Seal:** compute the page's CRC32C and store it in the `crc` header
  field.
- **CRC32C:** the 32-bit cyclic redundancy check with the Castagnoli
  polynomial (RFC 3720 §B.4).
- **Superblock copy:** one 4096-byte block that holds the configuration and
  a committed `State`. A store has two copies.
- **Commit:** a durable, atomic replacement of the committed `State`.
- **Applied watermark `N`:** `State::applied_seq`. Every operation with a
  sequence at or below `N` is in the committed image.
- **Write watermark `W`:** `State::write_seq`. Every write-through with a
  sequence at or below `W` finished, and was synced, before the commit.
- **Committed image:** for each leaf, its stored page with the highest
  write sequence at or below `W`.
- **Superseded page:** a stored page that a later write-through of the
  same leaf replaced.
- **Erase:** write a zero page over a page number, so that no later scan
  reads its old contents as a page.
- **Suspect page:** a page that fails its checksum and whose header claims
  a write sequence at or below `W`.
- **Hole:** a leaf ID that names no leaf: it was allocated, but no selected
  page carries its stamp.

## On-disk layout

```mermaid
flowchart LR
    subgraph area["superblock area: 8 KiB, independent of page size"]
        A["copy 0<br/>bytes 0..4096"]
        B["copy 1<br/>bytes 4096..8192"]
    end
    subgraph pages["pages: page_size bytes each"]
        P0["page 0<br/>8192 + 0 * P"]
        P1["page 1<br/>8192 + 1 * P"]
        Pn["page n<br/>8192 + n * P"]
    end
    area --> pages
```

- Superblock copy `c` is at byte `c * COPY_BYTES` (`COPY_BYTES = 4096`),
  whatever the page size. Each copy therefore has a known offset, and
  `FileStore::open` never infers the page size from a damaged copy.
- Page number `i` is at byte `PAGES_OFFSET + i * page_size`
  (`PAGES_OFFSET = 8192`). For 4096-byte pages, every page offset is
  4096-aligned, which direct I/O (plan 0023) requires. For smaller test page
  sizes the area costs up to 8 KiB of padding.
- A valid file length is `8192 + n * page_size` for some `n >= 0`.
  `FileStore::open` returns `TruncatedFile` for a shorter file or a ragged
  tail.
- There is no catalogue. The file length gives the page count, and the page
  number gives the offset.
- A new page is all zeros. A freed page keeps its old bytes (see
  [The `PageStore` contract](#the-pagestore-contract)).

`MemStore` has the same logical model (configuration, state, pages
addressed by page number) without byte offsets or superblock copies.

## Superblock version 2

### Fields

Each copy encodes 56 bytes, little-endian, then zero padding to 4096 bytes:

| Bytes | Field | Meaning |
| --- | --- | --- |
| `0..8` | `magic` | `PEPPYBF\0` |
| `8..12` | `version` | `2` |
| `12..16` | `page_size` | Page length and leaf page capacity |
| `16..20` | `inner_capacity` | Inner-node capacity |
| `20..24` | `max_key_len` | Admission limit for keys |
| `24..28` | `max_value_len` | Admission limit for values |
| `28..36` | `checkpoint_id` | Committed checkpoint number; `0` for a new store, `1` after `Tree::create` |
| `36..44` | `applied_seq` | The applied watermark `N` |
| `44..52` | `write_seq` | The write watermark `W` |
| `52..56` | `crc` | CRC32C of bytes `0..52` |

In memory the copy is two values. `Superblock` holds only the
configuration (`page_size`, `inner_capacity`, `max_key_len`,
`max_value_len`), as in plan 0008, and `State` holds `checkpoint_id`,
`applied_seq`, and `write_seq`. The store keeps them side by side. The
[leaf storage design](bf-tree-leaf-store.md#the-superblock) records how the
configuration fixes the admission rule and which tree settings it omits.

### Codec

- `Superblock::encode(&self, state, block)` validates the configuration,
  zero-fills `block`, and writes the 56 encoded bytes with their CRC. The
  block must hold at least 56 bytes. The stores always pass a full
  4096-byte copy, so direct I/O needs no special case.
- `Superblock::decode(block)` checks, in this order: the block holds 56
  bytes ("too short"), the magic ("bad magic"), the version ("unsupported
  version"), the CRC ("checksum mismatch"), and the configuration
  (`Superblock::validate`: the page size is a legal page capacity). Each
  failure is `StoreError::InvalidSuperblock(reason)`. A version-1 copy is
  rejected; no version-1 data was deployed.
- The CRC covers bytes `0..52`, every encoded field. The padding after
  byte 56 is not covered and is not checked on decode.

### Dual copies and selection

`Superblock::select(copy0, copy1)` decodes both copies and skips a copy
that fails, because a commit can tear it:

| Copy 0 | Copy 1 | Result |
| --- | --- | --- |
| intact | intact, same configuration | the copy with the higher `checkpoint_id`; copy 0 on a tie |
| intact | intact, different configuration | `InvalidSuperblock("superblock copies disagree on the configuration")` |
| intact | fails to decode | copy 0 |
| fails to decode | intact | copy 1 |
| fails to decode | fails to decode | `NoSuperblock` |

A configuration mismatch is never a torn write, because a commit never
changes the configuration, so it is rejected and not resolved by id. Only a
new store has two copies with equal ids.

### Commit

`commit(&State)` must receive `state.checkpoint_id ==
state().checkpoint_id + 1`. Otherwise it returns `StoreError::CommitOrder {
current, got }` and changes nothing. The check makes two concurrent
committers an error instead of two writes to the same copy.

`FileStore::commit` holds a commit mutex for the whole operation. It
encodes the state, writes the full copy `state.checkpoint_id % 2`, calls
`sync_data`, and only then replaces the in-memory state, which a separate,
short-held mutex protects. `state()` therefore never waits for a commit's
sync. Successive commits alternate copies, so the previous commit is always
in the other copy, and a torn commit leaves it intact. `MemStore::commit`
checks the order and replaces its state under one mutex.

The caller must sync its page writes before it commits. The tree commits in
two places only, both through `Tree::commit_written`: `Tree::checkpoint`
and the end of tree creation (see [Create](#create)).

### Store creation

`FileStore::create` validates the configuration, creates the file (it fails
if the path exists), writes both copies with `State::default()`, and calls
`sync_all`. It writes no page. `MemStore::new` starts with
`State::default()`.

`Tree::create` requires an empty store with the default state:
`page_count() == 0` and `state() == State::default()`. Otherwise it returns
`InvalidStore("store already holds pages")`. It then writes the first leaf
and commits checkpoint 1 ([Create](#create)).

## Page numbers and leaf IDs

A page number is a position, as SQLite's `Pgno` is. A leaf ID is an
identity. Plan 0008 used the leaf ID as the store position. Plan 0017
separated them, because shadow paging writes each new version of a leaf to
a different page number while the leaf keeps its ID.

```rust
/// The position of a page in the store.
pub struct PageNumber(u64);

impl PageNumber {
    /// `None` for `u64::MAX`, which callers use as "no page".
    pub const fn new(raw: u64) -> Option<Self>;
    pub const fn raw(self) -> u64;
}
```

- The name "page number" replaces "slot", because "slot" already names an
  entry of a page's slot directory. Using it for store positions would make
  it ambiguous in every layer.
- The store knows only page numbers. It does not depend on
  `bf-tree-node-id` and never sees a leaf ID.
- The tree maps each leaf ID to a page number in the
  [leaf-to-page table](#the-leaf-to-page-table). Each stored page names its
  leaf in its header, so `open` can rebuild the table from the pages alone.
- Every write-through moves the leaf to a fresh page number
  ([Shadow paging](#shadow-paging)). Between checkpoints, a leaf can
  therefore have several stored pages; the table names the newest one.
  Leaf IDs and page numbers are allocated independently, and leaf IDs can
  exceed the page count (see [Open](#open-select-the-committed-image)).

## The `PageStore` contract

```rust
pub trait PageStore: Send + Sync + std::fmt::Debug {
    fn page_size(&self) -> usize;
    /// Page numbers ever allocated, live or freed: `0..page_count()`.
    fn page_count(&self) -> u64;
    /// A freed page if any (its old bytes kept), else a new zero page.
    fn alloc(&self) -> Result<PageNumber, StoreError>;
    /// Return a page for reuse. Its bytes are not changed.
    fn free(&self, page_number: PageNumber) -> Result<(), StoreError>;
    fn read(&self, page_number: PageNumber, buf: &mut [u8]) -> Result<(), StoreError>;
    fn write(&self, page_number: PageNumber, buf: &[u8]) -> Result<(), StoreError>;
    fn sync(&self) -> Result<(), StoreError>;
    fn superblock(&self) -> &Superblock;
    /// The last committed state.
    fn state(&self) -> State;
    /// Durably and atomically replace the committed state.
    fn commit(&self, state: &State) -> Result<(), StoreError>;
}
```

- `alloc` reuses a freed page if one exists (last freed, first reused),
  else appends a new zero page. `FileStore::alloc` extends the file with a
  zero page before it returns, so the file length stays authoritative.
- `free` marks a page for reuse and does not change its bytes. A read of a
  freed page returns its old bytes. A write to a freed page and a second
  `free` are `FreedPage(page_number)`. Plan 0008's rule that a freed page
  is all zeros ended with plan 0017: open selects pages by their header,
  not by allocation state, so zeroing would cost one write per freed page.
  A consequence is that a freed page can hold an older copy of a leaf; the
  selection rule and the erase rule account for it.
- `page_count` counts every page number ever allocated, live or freed.
  Freeing does not shrink it, and the file never shrinks.
- A page number at or beyond `page_count` is `UnknownPage(page_number)`. A
  buffer whose length is not the page size is `BufferLength`.
- `FileStore` does not persist its free list. A reopened `FileStore` treats
  every page as live. `Tree::open`, which reads every page, frees the
  all-zero, superseded, and erased ones; a `FreedPage` reply from a backend
  that already knows is accepted.
- `state` and `commit` are described in [Commit](#commit).
- The [concurrency design](bf-tree-concurrency.md#the-shareable-store-contract)
  records the threading rules: every method takes `&self`, and the caller
  serializes accesses to one page.

The tree calls `alloc` only in `write_through`. It calls `free` in four
places: `write_through` (a fresh page whose write failed), the checkpoint's
free step (superseded pages that a commit covers), `LeafMap::discard` (a
failed split's right half), and `Tree::open` (zero, superseded, suspect,
and erased pages).

## Base-page identity and the page checksum

A page number does not identify a leaf, so a stored page must name its
leaf. A stored page also carries a checksum, so that `load_base` and `open`
detect damaged bytes instead of trusting them, and a write sequence, so
that `open` can tell which copy of a leaf is committed.

### Header fields

The header is 32 bytes. Bytes `8..32`:

| Bytes | Field | Meaning |
| --- | --- | --- |
| `8..16` | `backing_leaf: U64` | The leaf that a mini-page overlays. `u64::MAX` (none) for base pages and full pages |
| `16..24` | `write_seq: U64` | The write sequence of the write-through that stored the page. Both halves of a split share one value. Zero in pages never stored, including mini-pages |
| `24..28` | `leaf: U32` | The leaf that a stored base page belongs to. `NO_LEAF = u32::MAX` means none |
| `28..32` | `crc: U32` | CRC32C of the whole page with bytes `28..32` (`CRC_RANGE`) taken as zero |

- `backing_leaf` keeps its literal meaning: the leaf that an overlay
  covers. A separate `leaf` field names a stored base page's own leaf.
- `Page::leaf()` returns `Option<LeafId>`. `Page::set_leaf(Some(id))`
  returns `PageError::LeafIdTooLarge` if `id.raw() >= u32::MAX`.
- `Page::write_seq()` and `Page::set_write_seq(seq)` read and write the
  sequence. Plan 0018 renamed the field from the inactive `lsn`.
- An all-zero header reads as `leaf() == Some(LeafId(0))`, not `None`,
  because the sentinel is `u32::MAX`. `open` therefore tests for an all-zero
  page before it reads any header field, and it detects "header did not
  land" by `write_seq == 0`, not by the leaf stamp.
- The checksum covers every byte of the page: the header, including
  `write_seq` and `leaf`, the slot directory, the payloads, and the free
  gap. `Page::checksum`, `stored_checksum`, `seal`, and `verify_checksum`
  work on a valid `Page`. The free function
  `bf_tree_page::verify_stored_checksum(&[u8])` works on raw bytes that may
  not be a valid page: it reads only the `crc` field and computes the CRC
  over the slice length, never over a `capacity` read from a possibly
  damaged header.
- `bf_tree_page::raw_header_stamps(&[u8])` returns `(write_seq, leaf)` from
  the raw header of bytes that may be damaged, without any check. `open`
  uses it before the checksum check.

### Who stamps and seals

`LeafMap::write_through` is the only place that stamps and seals a page
for the store. For each page, it calls `set_write_seq(seq)`, then
`set_leaf(Some(id))`, then `seal()` as the last change, and then writes the
page. The checksum therefore covers both stamps.

The checksum is valid only for bytes as stored:

- `Page::init` and `Page::init_with_fences` set `write_seq` to zero, `leaf`
  to none, and `crc` to zero.
- Split and merge outputs copy `backing_leaf`, `write_seq`, `leaf`, and
  `crc` from the source page. In-place changes (inserts, removals,
  compaction) leave them as they are. Either way the `crc` is stale after a
  change. No page operation seals.
- Mini-pages are never written to the store. Nothing stamps or seals them.
- A full page loaded from the store keeps the stored stamps and `crc`.
  Direct writes make the `crc` stale. Its next write-through stamps and
  seals it again.

Stamping and sealing in `write_through` means that a page built in
scratch, such as a split's right half that inherited the left half's
header, never reaches the store with the wrong leaf, an old sequence, or a
stale checksum. The checksum costs one pass over each page written to the
store, and nothing on the in-memory write path.

### `load_base` checks

`load_base(leaf, buf)` checks a stored page in this order:

1. The leaf has a page number. Otherwise
   `CorruptLeaf { id, reason: "no stored page" }`.
2. The store reads the page.
3. `verify_stored_checksum` on the raw bytes, before any structural check.
   A mismatch is `CorruptLeaf { id, reason: "checksum mismatch" }`.
4. In debug builds, the full `Page::validate`. In every build, the O(1)
   `Page::check_header`. A failure is `CorruptLeaf { id, reason }`.
5. `leaf() == Some(id)`. A mismatch is
   `CorruptLeaf { id, reason: "names another leaf" }`. It catches a
   page-table bug or a misdirected write on the first load.

### Why `snapshot_version` was free

Bytes `24..32` held `snapshot_version` until plan 0017. Upstream uses that
field for Concurrent Prefix Recovery (CPR) checkpoints. Each page carries a
checkpoint version, and a write to a page with an older version copies the
page first: per-page copy-on-write (`src/nodes/leaf_node.rs`,
`src/mini_page_op.rs` at about lines 505 to 545, and `src/snapshot.rs`).
peppydb does not use CPR. Shadow paging and the `write_seq` stamp replace
it, and peppydb snapshots (plans 0019 and 0022) copy the committed pages
that `write_seq` selects. No per-page snapshot version is needed, so the
field became the 32-bit `leaf` and the 32-bit `crc`.

The split keeps the 32-byte header. No page capacity, mini-page size class,
or admission calculation changed, and the header size still matches
upstream. The cost is a limit of 2^32 - 1 leaf IDs per tree (see
[Leaf ID allocation](#leaf-id-allocation)). No inactive header field
remains: later per-page metadata needs a larger header or a page trailer.
For this reason recovery has no open generation in the page header; see
[The erase rule](#the-erase-rule).

## The leaf-to-page table

```rust
pub(super) struct LeafMap {
    store: Box<dyn PageStore>,
    entries: Table<RwLock<LeafEntry>>,
    /// Page number of each leaf's stored page, or `NO_PAGE` (`u64::MAX`).
    pages: Table<AtomicU64>,
    /// The next leaf ID.
    next: AtomicUsize,
    /// Last write sequence drawn.
    write_seq: AtomicU64,
    /// Shared by every write-through; exclusive for the watermark read.
    gate: RwLock<()>,
    /// Pages that a later write replaced, with the replacing sequence.
    superseded: Mutex<Vec<(PageNumber, u64)>>,
    // circular_buffer and classes
}
```

- `pages[id]` holds the raw page number of leaf `id`'s newest stored page,
  or `NO_PAGE`. `NO_PAGE` is `u64::MAX`, which `PageNumber::new` maps to
  `None`. `page_of(id)` returns `Option<PageNumber>`.
- The table lives beside `entries`, not inside `LeafEntry`. The entry
  lock must stay within 64 bytes (the plan 0012 size test,
  `entry_lock_is_at_most_64_bytes`), and only base-page I/O and open read
  the page number.
- A thread writes `pages[id]` only under leaf `id`'s exclusive (X) lock:
  `alloc` sets `NO_PAGE`, `write_through` swaps in the fresh page number on
  every write, and `discard` resets it to `NO_PAGE`. Readers load it under
  the shared (S) or X lock.
- `open` builds the table from the selected pages and passes it to
  `LeafMap::new` as one `Option<PageNumber>` per leaf ID.

## Leaf ID allocation

- `LeafMap::alloc` takes the next index from the atomic counter `next`,
  grows `entries` and `pages` to it, resets the entry, sets `NO_PAGE`, and
  returns the fresh leaf under X. The store is not involved: the leaf's
  first `write_through` allocates its page.
- Leaf IDs are append-only within a process. `alloc` never reuses a hole.
  The checkpoint pass depends on this rule
  ([Why `N` is complete](#why-n-is-complete)). After `open`, the counter
  starts at the largest selected leaf plus one, so IDs of trailing holes,
  which no selected page names, can be allocated again after a reopen.
- The counter's `fetch_update` refuses to pass `MAX_LEAVES = u32::MAX`. The
  largest leaf ID is therefore `u32::MAX - 1`, which fits the stamp next to
  the `NO_LEAF` sentinel, and a tree has at most 2^32 - 1 leaf IDs,
  including holes (16 TiB of 4 KiB leaves). At the limit `alloc` returns
  `TreeError::TooManyLeaves` before any table grows. `LeafId` stays a `u64`
  in memory; no ID is truncated.
- A hole arises in three ways: a split fails after `alloc` but before its
  right half is written; a split fails after the right half is written, so
  `discard` zeroes and frees that page; or `open` selects no page for an ID
  below the largest selected leaf. A hole's entry is `freed` and its page
  is `NO_PAGE`. `read` and `write` return `UnknownLeaf` for it, and no
  separator names it.

## Shadow paging

A committed page is never overwritten. Every write-through writes each of
its pages to a fresh page number, so the committed image stays intact on
disk until a later commit replaces it.

```rust
/// The caller holds the X lock of every leaf in `pages` and passes the
/// reachable leaf first.
pub(super) fn write_through(&self, pages: &mut [(LeafId, &mut [u8])]) -> Result<u64, TreeError> {
    let _in_flight = self.gate.read();
    let seq = self.write_seq.fetch_add(1, Ordering::AcqRel) + 1;
    let (first, rest) = pages.split_first_mut().expect("at least one page");
    for (id, bytes) in rest.iter_mut().chain([first]) {
        let page: &mut Page = zerocopy::FromBytes::mut_from_bytes(bytes)?;
        page.set_write_seq(seq);
        page.set_leaf(Some(*id))?;
        page.seal(); // last: the checksum covers the stamps
        let page_number = self.store.alloc()?;
        if let Err(error) = self.store.write(page_number, bytes) {
            let _ = self.store.free(page_number); // the page names no state
            return Err(error.into());
        }
        let old = self.pages[id].swap(page_number.raw(), Ordering::AcqRel);
        if let Some(old) = PageNumber::new(old) {
            self.superseded.lock().push((old, seq));
        }
    }
    Ok(seq)
}
```

- **One sequence per call.** Every page that one call writes carries the
  same `write_seq`. A split passes both halves in one call, so a commit
  covers both halves or neither, and the committed leaves still tile the
  key space.
- **Order.** The pages after the first are written first, and the reachable
  leaf last. An error before the last write leaves the reachable leaf's
  page table entry unchanged. A split that fails then discards its fresh
  right leaf.
- **Fresh page.** `alloc` returns a freed page or a new zero page. Neither
  is in the committed image: the checkpoint frees a page only after a
  commit no longer references it.
- **A failed write frees its page.** If `store.write` fails, the fresh page
  names no state, so `write_through` returns it to the store before the
  error propagates.
- **The superseded list.** The page that a write replaces is recorded with
  the replacing write's sequence, and it is not freed at once, even if no
  commit ever selected it. A checkpoint that has read its `W` but has not
  yet committed may select that page. The first commit whose `W` covers the
  replacing sequence frees it
  ([Why freeing is safe](#why-freeing-is-safe)). The list costs 16 bytes
  per replaced page until that commit.
- **The gate.** `write_through` holds the write-through gate shared from
  before it draws its sequence until it has written every page, swapped the
  page table, and recorded the superseded pages. The checkpoint takes the
  gate exclusively to read `W`
  ([Why `W` is complete](#why-w-is-complete)).
- **File growth.** Between checkpoints, a leaf written through `n` times
  holds `n + 1` pages: its committed page and `n` later pages. Checkpoint
  frequency bounds file growth. Freed pages are reused before the file
  grows, and the file never shrinks.

`LeafMap::discard` gives up a fresh, never-reachable split half. If the
half was written, `discard` writes a zero page over it and then frees it.
The zeroing is still needed with shadow paging: the discarded page carries
a sequence that the next commit covers, and without the zero write the
next `open` would select it as an extra leaf that overlaps its neighbour.

## Sequences and watermarks

The tree keeps three counters:

| Counter | Where | Drawn or advanced by | Committed as |
| --- | --- | --- | --- |
| `write_seq` | `LeafMap` | each `write_through` call, under the gate | `W` (`State::write_seq`) |
| `last_seq` | `Tree` | each `put` or `remove`, in the sequence closure | not committed |
| `applied` | `Tree` | `fetch_max` with the write's sequence after the leaf lock is released | `N` (`State::applied_seq`) |

The common write path takes a sequence closure:

```rust
/// `seq` runs under the owning leaf's exclusive lock, after the route is
/// validated and before the record is applied. A retry runs it again.
pub(super) fn write(
    &self,
    key: &[u8],
    state: RecordState<'_>,
    seq: &mut dyn FnMut() -> Result<u64, TreeError>,
) -> Result<u64, TreeError>;

fn write_internal(&self, key: &[u8], state: RecordState<'_>) -> Result<(), TreeError> {
    let seq = self.write(key, state, &mut || {
        Ok(self.last_seq.fetch_add(1, Ordering::AcqRel) + 1)
    })?;
    self.applied.fetch_max(seq, Ordering::Release);
    Ok(())
}
```

- **The closure rule.** `write` calls `seq` inside the attempt that holds
  the owning leaf's X lock, after route validation and before the record is
  placed. The record is applied before the lock is released. For each key,
  sequence order therefore equals apply order.
- **Retries.** An attempt that ends with `NeedRoom` or a changed route
  calls `seq` again in the next attempt. The abandoned sequence names no
  applied record. Gaps in the sequence space are harmless.
- **`applied`.** A writer advances `applied` with `fetch_max` after
  `write` returns, so after its leaf lock is released. A write that
  returned to its caller has a sequence at or below `applied`. The
  converse does not hold while writes run: a write on another leaf with a
  lower sequence can still hold its leaf lock. The checkpoint argument
  needs only the first property and the leaf lock
  ([Why `N` is complete](#why-n-is-complete)).
- **Accessors.** `Tree::applied_seq()` returns `applied`.
  `Tree::committed()` returns `store.state()`, the state that a reopen
  selects.
- **After open.** `applied` and `last_seq` start at the committed `N`. New
  writes draw `N + 1` and later. A write that was above `N` and lost in a
  crash therefore has its sequence reused by a later write. Replay from a
  log gives replayed writes new sequences. Plan 0019 adds external
  sequences and one sequence space across both.
- **Two sequence spaces.** Write sequences order page versions, and
  operation sequences order record writes. They are independent: one
  write-through can carry many records, and a record can be written through
  several times.

## The checkpoint protocol

```rust
/// Make every write applied before this call durable as one atomic commit,
/// and return the committed `State`. Writers keep running. Must not be
/// called with a leaf lock held, for example from a scan visitor.
pub fn checkpoint(&self) -> Result<State, TreeError>;
```

`Tree::checkpoint` replaces plan 0013's `flush`. The name changed because
the guarantee changed: the call is an atomic commit point and returns the
committed `State`.

```mermaid
sequenceDiagram
    participant K as checkpoint
    participant T as tree and leaf locks
    participant G as write-through gate
    participant S as store
    K->>K: lock the checkpointing mutex
    K->>K: fail with CheckpointFailed if store_failed
    K->>T: N = applied
    loop index from 0 while below leaves.len, re-read on every step
        K->>T: X-lock the leaf if live
        K->>T: Mini: merge_leaf. Full: write back if dirty
        K->>T: release the split's right half, then the leaf
    end
    K->>G: acquire exclusive
    K->>G: W = write_seq
    K->>G: release
    K->>S: sync
    K->>S: commit State with id + 1, N, W
    K->>T: take superseded entries with sequence at or below W
    K->>S: free those pages
```

The code is `Tree::checkpoint` and `Tree::commit_written` in
`crates/bf-tree/src/lib.rs`:

1. Lock the `checkpointing` mutex for the whole call. Two checkpoints would
   otherwise race on the next checkpoint id and on the free step.
2. If the store-failed flag is set, return `TreeError::CheckpointFailed`.
3. Read `N` from `applied`.
4. **The pass.** Visit every leaf ID from 0. The loop bound is
   `leaves.len()`, read again on every step. Take each live leaf's X lock
   with `write_if_live`. A leaf with a mini-page merges it through
   `merge_leaf`, which may split the leaf; the split's right guard drops
   before the leaf's guard. A leaf with a full page writes it back if it
   is dirty. Other leaves need nothing. The pass holds one leaf lock at a
   time, plus a split's fresh right half.
5. **`commit_written(N)`.** Read `W` with the gate held exclusively
   (`LeafMap::write_watermark`). Sync the store. Commit
   `State { checkpoint_id: current + 1, applied_seq: N, write_seq: W }`.
6. Drain the superseded entries whose sequence is at or below `W`
   (`take_superseded_through`) under the `superseded` mutex, release the
   mutex, and free those pages.
7. Return the committed `State`.

A checkpoint always commits, even when nothing changed. Each call therefore
yields a distinct `checkpoint_id`, and the caller's policy decides when to
call.

### Why `N` is complete

Claim: when the commit lands, every operation with a sequence `s <= N` has
its record in a page with `write_seq <= W`, or in a later page of the same
leaf, or of the split half that took the key, that is also at or below `W`.

`applied` reached `N` only after some operation drew `N`, and sequences
are drawn in increasing order. So `s` was drawn before the checkpoint read
`N`, or `s` is an abandoned retry that names no record. The writer drew
`s` while it held the X lock of some leaf `X`, applied the record, and only
then released the lock. Consider where the record is when the pass reaches
it:

- **Still in `X`, buffered or in a dirty full page.** The pass locks `X`
  after it read `N`, so it waits for the writer to release `X`, and then
  it sees the record. The merge or write-back that the pass runs draws its
  sequence before the gate is taken, so that sequence is at most `W`.
- **Already written through** by an eviction, a merge, a split, a
  promotion, or a write-back. That write-through either finished before
  the gate was taken, or was in flight and the gate waited for it. Its
  sequence is at most `W`.
- **Buffered in a new right leaf `Y`.** A split places its pending record
  into the half that owns the key while both halves are locked. `Y` is
  allocated while the writer still holds `X`, and leaf IDs are append-only,
  so `Y > X`. The pass has not yet locked `X`, so its position is at most
  `X`. Because the loop re-reads the length, the pass reaches `Y` later. A
  pass that took a list of leaf IDs at its start would miss `Y`.

A write that returned before `checkpoint` was called advanced `applied`
before it returned, so its sequence is at most `N`, and the claim covers
it. The committed image can also contain operations above `N`. The
checkpoint is fuzzy: writers never pause. This departs deliberately from
the paper's online snapshot, which pauses writers (§5.7).

### Why `W` is complete

Every write-through holds the gate shared from before it draws its sequence
until it has written every page, swapped the page table, and recorded its
superseded pages. The exclusive acquisition waits for every write-through
in flight. Therefore every write-through with a sequence at or below `W`
has finished its writes before the `sync`, and its superseded entries are
in the list before the free step. Write-throughs that start after the gate
is released draw sequences above `W`, and `open` ignores and erases their
pages. The gate blocks only write-throughs, and only for one counter load.
Buffered writes never take it.

### Why freeing is safe

An entry `(old, s)` with `s <= W` means that a page of the same leaf with
sequence `s` was written and synced before the commit. That page, or a
later page of the leaf at or below `W`, outranks `old`, so the committed
image does not reference `old`. The free step runs after `commit` returns.
A crash before it only leaves extra pages, which `open` releases as
superseded.

An entry with `s > W` stays in the list. Its `old` page may be the page
that this commit selects for its leaf, even if no earlier commit selected
it.

If a free fails after the commit, `checkpoint` returns the store error,
although the commit is durable. The drained entries that were not freed
are not retried in this process; the next `open` releases those pages as
superseded. A free error does not set the store-failed flag.

### Concurrency

- The pass locks one leaf at a time by ID, as `check_invariants` and
  eviction do, so the caller must hold no leaf lock. A checkpoint called
  from a scan visitor would wait for the leaf that its own caller holds.
- The checkpoint takes the gate exclusively while it holds only the
  checkpointing mutex: no leaf lock and no frame lock. The exclusive hold
  covers one atomic load.
- The [concurrency design](bf-tree-concurrency.md#lock-order) gives the
  ranks of the checkpointing mutex, the gate, and the `superseded` mutex.

## Store failures

A failed page write counts as a store failure too. The device may report
the error and still land the bytes, under a sequence that the next commit
would cover; a later `open` could then select a page that no committed
state intended, such as an unreachable split half. So `write_through`
records the failure, `Tree::store_failed` reports it, and no later commit
happens in the process. On reopen the page is above the committed
watermark and is erased (`a_failed_page_write_fails_later_checkpoints`).

After a failed `sync` or `commit`, the operating system may have dropped
dirty pages, and a later sync can report success without them. So a later
commit could name pages that never reached the disk. `Tree::durable` wraps
the `sync` and the `commit` in `commit_written` and sets the store-failed
flag when either fails:

- The failing checkpoint returns the store error and has committed and
  freed nothing. `store().state()` is still the previous state.
- Every later `checkpoint` returns `TreeError::CheckpointFailed`, whatever
  the policy. `Tree::store_failed()` reports the flag.
- Reads always continue.
- Writes follow `TreeConfig::on_store_failure`, checked by `check_store` at
  the start of `Tree::write`:

```rust
/// What writes do after a failed sync or commit. Checkpoints always fail.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StoreFailurePolicy {
    /// Writes return `TreeError::StoreFailed`. Fail-stop.
    #[default]
    RejectWrites,
    /// Writes continue, for deployments whose durability comes from a log.
    AcceptWrites,
}
```

- Reopening clears the flag, because the flag is process state and `open`
  reads only what the last good commit covers.

The default rejects writes. With checkpoints as the only durability, no
later write could become durable in this process, and with a write-ahead
log the log could never be truncated. `AcceptWrites` keeps a tree that is
backed by a log serving until it is restarted. Like the caching settings,
the policy is a runtime setting and is not stored in the superblock.

Store errors in the pass (for example, a merge's write) return the error
without committing and do not set the flag: no sync has failed. Eviction
write-throughs can fail on the same device; their errors reach the writer
that evicted, as before.

## Create

`Tree::create` and `Tree::in_memory` call `with_store`. It allocates the
first leaf, initializes an unbounded page, and writes it through, which
draws write sequence 1. It then calls `commit_written(0)`, which syncs and
commits `State { checkpoint_id: 1, applied_seq: 0, write_seq: 1 }`. The
tree is not yet shared, so the call does not take the checkpointing mutex.

A store that is created and never checkpointed therefore reopens as an
empty tree, not as a store with no committed page.

## Open: select the committed image

`FileStore::open` selects the superblock copy. `Tree::open` takes the
configuration from it (`Tree::open_with` requires a matching one) and reads
the committed `State`: `N` and `W`. `select(store, config, W)` in
`crates/bf-tree/src/open.rs` then reads pages `0..page_count()` once, in
page-number order.

```mermaid
flowchart TD
    A["State from the selected superblock copy: N, W"] --> B[read the next page]
    B --> Z{all zeros?}
    Z -->|yes| F0["free at once"]
    Z -->|no| H["read the raw header: write_seq, leaf. max_seen = max of write_seq"]
    H --> W1{"write_seq above W?"}
    W1 -->|yes| X[erase list]
    W1 -->|no| K{checksum matches?}
    K -->|no| SU[suspect list]
    K -->|yes| V{"validate, leaf, ID bound, fences, limits"}
    V -->|fail| E1["CorruptPage or InconsistentLeaves"]
    V -->|ok| D{"leaf already has a candidate?"}
    D -->|no| C1[candidate]
    D -->|"same write_seq"| E2["InconsistentLeaves: two pages name the same leaf"]
    D -->|"other write_seq"| C2["keep the higher, release the lower"]
    SU --> SR{"after the scan: write_seq 0, or the leaf has a candidate at or above the claim?"}
    SR -->|yes| RL[release]
    SR -->|no| E3["CorruptPage: checksum mismatch"]
    X --> ER["write zeros to each, sync once"]
    ER --> FR["free released and erased pages"]
    C1 --> TL["tile, bulk-build inner levels, holes"]
    C2 --> TL
    TL --> RS["write_seq counter = max of max_seen and W. applied = last_seq = N"]
```

### Per-page steps

For each page, in this order:

1. **All zeros.** The page was allocated and never written, or it was
   erased or discarded. `open` frees it at once, so the next allocation
   reuses it. This test comes first: an all-zero page fails the checksum,
   and its header reads as leaf 0.
2. **Raw header.** `open` reads `write_seq` and `leaf` from the raw header
   with `raw_header_stamps`, before any check, and raises `max_seen` to
   `write_seq`. A page that fails its checksum still contributes to
   `max_seen` and to the erase decision.
3. **Above `W`.** A page whose header claims `write_seq > W` was written
   after the committed checkpoint. A crash can tear such a page, so its
   checksum is not checked. It goes on the erase list.
4. **Checksum.** A page that fails `verify_stored_checksum` goes on the
   suspect list with its claimed `write_seq` and `leaf`. It is classified
   after the scan ([Suspect pages](#suspect-pages)).
5. **Structure** (`check_page`). The checksum passed, so a failure here
   means that a bug wrote the page, not that a crash tore it:
   - `Page::validate` fails: `CorruptPage { page_number, reason }` with the
     validation reason.
   - `leaf()` is `None`: `InconsistentLeaves("a stored page names no leaf")`.
   - The leaf ID is at or above `max(4 * page_count, 65536)`:
     `InconsistentLeaves("a stored page names an implausibly large leaf ID")`.
     The check runs before any table grows, so a damaged stamp cannot make
     `open` allocate a table of billions of entries.
   - No fences, or a fence, key, or value beyond the configured limits:
     `InconsistentLeaves`, as in plan 0008.
6. **Newest per leaf.** The first page of a leaf becomes its candidate. A
   later page of the same leaf with a higher `write_seq` replaces it, and
   the older page goes on the release list; a page with a lower
   `write_seq` goes on the release list directly. Two pages of one leaf
   with the same `write_seq` are
   `InconsistentLeaves("two stored pages name the same leaf")`: one
   write-through writes one page per leaf, and the counter restart keeps
   sequences unique, so a tie means damage or a bug.

The scan returns early with the first error that steps 5 and 6 find.

### After the scan

1. **Suspects** are classified ([Suspect pages](#suspect-pages)). Each is
   released or makes `open` fail with `CorruptPage`.
2. **Erase.** If the erase list is not empty, `open` writes a zero page
   into each listed page number and then syncs once.
3. **Release.** `open` frees every released and erased page. A
   `FreedPage` reply is accepted, because a `MemStore` that was reopened in
   the same process still knows its free list.
4. **Rebuild.** The candidates form the page table. The tiling check and
   the bulk build of plan 0008 run unchanged
   ([leaf storage](bf-tree-leaf-store.md#open)). An empty store is
   `InconsistentLeaves("store holds no leaves")`. The leaf tables are sized
   to the largest selected leaf plus one, and every ID without a page is a
   hole.
5. **Counters.** The write-sequence counter restarts at
   `max(max_seen, W)`, so the next write-through draws a sequence above
   every sequence in the file. `applied` and `last_seq` restart at `N`.

| What a page number holds after a crash | `open` |
| --- | --- |
| all zeros | free: allocated and never written, or erased |
| header `write_seq > W`, intact or failing its checksum | erase and release: written after the checkpoint |
| intact page, `write_seq <= W`, newest for its leaf | **committed page** |
| intact page, `write_seq <= W`, older than another page of its leaf | release: superseded |
| intact page, `write_seq <= W`, same `write_seq` as another page of its leaf | `InconsistentLeaves` |
| page failing its checksum whose header did not land (`write_seq` 0) | release |
| page failing its checksum in a reused page number with an older page's header (its leaf has a candidate at or above the claim) | release: that leaf has a newer committed page |
| page failing its checksum, header `write_seq <= W`, with no candidate of its leaf at or above the claim | `CorruptPage { page_number, reason: "checksum mismatch" }` |
| intact page, `write_seq <= W`, that fails `Page::validate` | `CorruptPage` with the validation reason: a bug wrote it |
| intact page, `write_seq <= W`, with no leaf, an implausible leaf ID, no fences, or out-of-limit records | `InconsistentLeaves` |

Why the leaf-ID bound is not `page_count`: a discarded split half takes a
leaf ID, its page is freed, and the next leaf reuses that page, so leaf IDs
can run ahead of page numbers. Plan 0017's diff 7 hole fixture found this.
The exact multiple has no correctness role.

Open costs one read and one CRC pass per page, plus one zero write per
erased page and one sync if any page was erased. Memory is proportional to
the page count. The erase cost is bounded by the write-throughs since the
last checkpoint.

### The erase rule

`open` must erase every page above `W`, not only release it. Suppose that
`open` only released such a page `P` (leaf `L`, sequence `s > W`). If no
later write reuses `P`'s page number, the next commit has `W' >= s`. A
second crash and open would then find `P` at or below `W'`. If `L` was not
written since the first open, `P` would outrank `L`'s committed page and
bring back state that the first recovery discarded. That state can include
writes that a log never made durable, or one half of a split whose sibling
was lost. `an_uncommitted_page_does_not_return_after_a_later_commit` tests
this case.

The erase is durable before the page is released, for a second reason. The
suspect rule below assumes that a reused page number holds zeros or an
older, superseded page of some leaf. A page above `W` that was released
without a durable erase, then reused and torn by a later write, would leave
a header that claims `s` for a leaf with no page at or above `s`. `open`
would then report `CorruptPage` for a torn write.

The write-sequence counter restarts above `max_seen`, not only above `W`.
Every sequence in the file then stays unique, and the argument does not
depend on every erase having landed: a crash during open before the sync
leaves pages that still claim more than `W`, and the next open erases them
again. An open generation, stored in the superblock and in each page, would
also fence these pages. It needs a header field, and none is free.

### Suspect pages

A page that fails its checksum is torn or damaged. Whether its leaf has a
newer page is known only after every page is read, so `open` collects such
pages during the scan and classifies them at the end. The classification
uses the raw header, because the header lands atomically (see
[Header atomicity](#header-atomicity)):

- A page whose header claims `write_seq > W` never becomes a suspect: it
  is erased in step 3.
- Otherwise the header claims a page that was synced before the commit,
  and a crash cannot tear such a page. A crash can still leave this header
  above a newer, torn body in two cases:
  - A write into a new page number whose header sector did not land. The
    header is zero, and no real page has `write_seq` 0. `open` releases the
    page.
  - A write into a reused page number whose header sector still holds an
    older, superseded page of some leaf. That leaf has a candidate with a
    `write_seq` at or above the claim, because the checkpoint frees a
    superseded page only after a commit covers its replacement, and the
    erase rule zeroes pages above `W` before `open` releases them for
    reuse. `open` releases the page.
- Any other suspect is damage to a committed page, and `open` fails with
  `CorruptPage { page_number, reason: "checksum mismatch" }`.

### Header atomicity

The design assumes that the 32-byte header lies in the first device sector
of a page and lands atomically: whole or not at all. The crash model tears
at 64-byte sectors, so its header always lands whole or not at all. The
code does not check the assumption. On a device that can tear inside the
first 32 bytes, `open` can misclassify a torn page: for example, it can see
a new `write_seq` with an old leaf stamp.

### Known limit: the header of a failed page is trusted

Classification reads `write_seq` and `leaf` from a page whose checksum
failed, and step 3 reads `write_seq` before any check. If corruption
changes a committed page's header to claim `write_seq > W`, `open` erases
that page. Open then fails the tiling check, unless an older superseded
copy of the leaf survives in a freed page. In that case the leaf silently
rolls back to that copy. The same happens if corruption changes the
claimed `write_seq` or `leaf` so that a surviving page outranks the
damaged one. Detecting this needs a second, independent record of each
committed page, such as a persisted page map or fence list.

## Guarantees and their tests

Test paths are relative to each crate's `src/`. Tree tests are in
`crates/bf-tree/src/tests/`.

### Store format (plan 0017)

| Guarantee | Tests |
| --- | --- |
| The CRC32C matches the RFC 3720 check values (`"123456789"` gives `0xE3069283`; 32 zero bytes give `0x8A9136AA`) | `bf-tree-store` `tests/crc.rs`: `crc32c_matches_the_rfc_3720_check_value` |
| The leaf stamp round trips, including `None`, and IDs from `u32::MAX` are `LeafIdTooLarge` | `bf-tree-page` `tests/layout.rs`: `the_leaf_stamp_round_trips_and_rejects_ids_beyond_32_bits` |
| The write sequence round trips and survives compaction and splits | `tests/layout.rs`: `the_write_sequence_round_trips_and_survives_rebuilds` |
| A sealed page verifies, and any single flipped byte fails verification | `tests/layout.rs`: `a_sealed_checksum_detects_any_flipped_byte` (every byte of a 256-byte page) |
| Split outputs inherit the stamp and a stale checksum; only the tree seals | `tests/layout.rs`: `split_outputs_inherit_the_leaf_stamp_and_the_stale_checksum` |
| Every encoded superblock byte is checked, and decode names the failure, including version 1 | `bf-tree-store` `tests/superblock.rs`: `copies_round_trip_with_their_state`, `every_encoded_byte_is_checked`, `decode_names_what_failed`, `encode_needs_room_and_a_valid_configuration` |
| Selection takes the newer intact copy, falls back from a flipped, zeroed, or half-written copy, rejects a configuration mismatch, and reports `NoSuperblock` | `tests/superblock.rs`: `select_takes_the_newer_intact_copy`, `select_falls_back_when_either_copy_is_torn`, `copies_that_disagree_on_the_configuration_are_rejected` |
| `FileStore` writes both copies on create, alternates copies on commit, and reopens at the committed state | `tests/file.rs`: `create_writes_both_superblock_copies_and_refuses_to_clobber`, `commits_alternate_copies_and_survive_reopen` |
| A torn newer copy (garbage, zeros, or half written) reopens at the previous state; a zeroed copy 0 opens from copy 1 | `tests/file.rs`: `a_torn_commit_reopens_at_the_previous_state`, `a_zeroed_first_copy_opens_from_the_second_without_probing` |
| Short, ragged, and foreign files are rejected; pages start after the 8 KiB area | `tests/file.rs`: `short_and_ragged_files_are_rejected`, `foreign_superblocks_are_rejected`, `alloc_extends_the_file_after_the_superblock_area` |
| Both backends share the contract: dense zero pages, freed pages keep their bytes and are reused first, freed and unknown pages are rejected, commits follow the current checkpoint | `tests/contract.rs`: `alloc_hands_out_dense_zeroed_pages`, `freed_pages_keep_their_bytes_and_are_reused_before_growth`, `freed_pages_reject_writes_and_double_frees`, `unknown_pages_are_rejected`, `commits_must_follow_the_current_checkpoint`, and the threaded scenarios |
| `Tree::create` requires an empty store with the default state | `bf-tree` `tests/store.rs`: `create_checks_the_store_against_the_configuration`, `create_rejects_a_store_with_a_committed_state` |
| A reopened `FileStore` tree rebuilds the same page table; every stored page is stamped with its leaf, sealed, and overlays nothing; the tree equals its oracle and passes `check_invariants` | `tests/store.rs`: `reopen_rebuilds_the_page_table_from_the_stamps` (600 keys) |
| Leaf IDs stop at the 32-bit limit with `TooManyLeaves`, and nothing grows | `tests/store.rs`: `leaf_ids_stop_at_the_32_bit_stamp_limit` |
| A flipped byte fails the load with `CorruptLeaf` ("checksum mismatch") and the open with `CorruptPage`; a page naming another leaf fails the load; a page with no leaf or a huge ID fails the open; holes stay unused; zero pages are reused after open | `tests/damage.rs` on both backends: `a_flipped_byte_fails_the_load_and_the_open`, `a_page_naming_another_leaf_fails_the_load`, `a_stored_page_without_a_leaf_or_with_a_huge_leaf_id_fails_the_open`, `holes_stay_unused_and_zero_pages_are_reused_after_open`, `a_hole_below_the_largest_leaf_stays_freed_after_open` |
| Gapped (every copy of a leaf zeroed), duplicated, torn committed, resealed-but-invalid, and unfenced pages are rejected with errors, not panics | `tests/open.rs`: `open_rejects_damaged_leaf_sets_with_errors_not_panics` |

### Shadow paging, checkpoints, and recovery (plan 0018)

| Guarantee | Tests |
| --- | --- |
| A write-through moves the leaf to a fresh page; the checkpoint frees the old page for reuse | `tests/shadow.rs`: `a_write_through_moves_the_leaf_and_supersedes_its_old_page` |
| A replaced page waits for a commit; nothing is overwritten | `tests/shadow.rs`: `supersessions_wait_for_a_flush` |
| Both halves of a split share one write sequence | `tests/shadow.rs`: `split_halves_share_one_write_sequence` |
| After checkpoints and a reopen, each leaf has one page and the counter continues above every page | `tests/shadow.rs`: `after_flush_and_reopen_each_leaf_has_one_page_and_sequences_continue` (`FileStore`) |
| Open keeps the newest copy of a leaf and releases older ones | `tests/shadow.rs`: `open_keeps_the_newest_copy_of_a_leaf_and_releases_older_ones` |
| File growth: `n` write-throughs of a leaf hold `n + 1` pages until a checkpoint, and then the `n` replaced pages are reused | `tests/shadow.rs`: `a_leaf_written_through_n_times_holds_n_plus_one_pages_until_a_checkpoint` |
| Create commits an empty tree (checkpoint 1, `write_seq` 1) that reopens empty | `tests/checkpoint.rs`: `create_commits_an_empty_tree` |
| A checkpoint commits `W` | `tests/checkpoint.rs`: `flush_commits_the_write_watermark` |
| A page written after the commit is ignored and zeroed on open | `tests/checkpoint.rs`: `a_page_written_after_the_commit_is_erased_on_open` |
| A crash between the commit and the free step reopens cleanly, and the superseded pages are reused | `tests/checkpoint.rs`: `a_crash_between_commit_and_free_reopens_cleanly` |
| A write-through in flight holds back the commit | `tests/checkpoint.rs`: `a_write_through_in_flight_holds_back_the_commit` |
| A failed commit keeps the previous state; later checkpoints return `CheckpointFailed`, writes return `StoreFailed`, reads continue, and a reopen clears the flag | `tests/checkpoint.rs`: `a_failed_commit_keeps_the_previous_state_and_fails_later_checkpoints` |
| `AcceptWrites` keeps writes after a failure; checkpoints still fail | `tests/checkpoint.rs`: `accept_writes_keeps_writes_after_a_failure` |
| A page that was never committed does not return after a later commit covers its sequence | `tests/checkpoint.rs`: `an_uncommitted_page_does_not_return_after_a_later_commit` |
| A checkpoint records every write that returned before it, and sequences continue from `N` after a reopen | `tests/checkpoint.rs`: `a_checkpoint_records_every_write_that_returned_before_it` |
| Concurrent checkpoints beside writers commit consecutive ids, and each returned `N` covers the writes before its call | `tests/checkpoint.rs`: `concurrent_checkpoints_commit_consecutive_ids_beside_writers` (3 checkpoint threads, 2 writers, 60 checkpoints, 1600 writes) |
| Every tear of a late write, over a fresh page and over a reused page, reopens at the commit | `tests/torn.rs`: `every_tear_of_a_late_write_reopens_at_the_commit` (all 16 subsets of four 64-byte sectors) |
| Damage to a committed page is `CorruptPage`; a sealed page that fails validation is `CorruptPage` | `tests/torn.rs`: `damage_to_a_committed_page_is_reported`, `a_sealed_page_that_fails_validation_is_reported` |
| After any crash that the fault store injects, including deaths during operations and during open, and commit faults before and after the superblock copy lands, `open` succeeds at the last durable commit, `applied_seq` equals its `N`, the tree equals the oracle at `N`, and replay of all, some, or none of the lost operations matches the oracle, across many generations on the same disk | `tests/crash.rs`: `crashes_recover_the_committed_state_with_a_tiny_buffer`, `crashes_recover_the_committed_state_with_a_large_buffer` |
| With checkpoints beside writers, after a crash each key holds the oracle's value at some sequence in `[N, last]`, and replay above `N` in sequence order gives the full oracle | `tests/crash.rs`: `crashes_with_checkpoints_beside_writers_recover_after_replay` |

The fault store (`FaultStore` in `tests/crash.rs`) keeps a durable image
and the writes since the last sync. A crash loses, lands, or tears each
later write at 64-byte sectors. The store can fail a commit before or after
its superblock copy lands, and it can fail every call after the `k`-th,
which covers deaths in a pass, in a split, and in the erase on open. Two
mutations of the code made the model fail on its first seeds: keeping pages
above `W` on open, and committing an applied watermark three above the one
the checkpoint covered. Plan 0018 records the seeds, rounds, and repeated
runs.

The model, concurrent, contention, and buffered suites run every write
through shadow paging and every `flush` call site as `checkpoint`.

## Not yet guaranteed

- **Writes after the last commit are lost on a crash or a close.** `open`
  selects the committed image and erases every later page, so merged and
  evicted writes after the last checkpoint are lost too, not only buffered
  ones. The tree has no log yet. The write-ahead log is plan 0020, and
  plan 0019 adds `apply` and the log hook.
- **The committed image is fuzzy.** It contains every operation at or
  below `N` and possibly some above `N`, per key. Only replay above `N`
  from a log gives an exact state. Without a log, a reopened tree holds,
  for each key, the value after some operation in `[N, last]`.
- **Header corruption can roll a leaf back silently.** See
  [Known limit](#known-limit-the-header-of-a-failed-page-is-trusted).
- **Header atomicity is assumed, not checked.** See
  [Header atomicity](#header-atomicity).
- **A failed commit is indeterminate on disk.** If the copy write or its
  sync fails, `state()` keeps the previous state, but the target copy can
  hold the new state, the old bytes, or a torn copy. The other copy still
  holds the previous commit. The tree then sets the store-failed flag, so
  this process never commits again.
- **A failed free after a commit is not retried.** The checkpoint returns
  the error, and the pages stay allocated until the next `open` releases
  them.
- **The file never shrinks.** Between checkpoints, a leaf written through
  `n` times holds `n + 1` pages. Only checkpoint frequency bounds growth.
- **Open reads every page.** Open is `O(pages)`. A persisted fence list
  for faster open is deferred.
- **Leaves are never reclaimed.** An empty leaf keeps its fences and its
  page.
- **No snapshot pins pages.** Plan 0019 adds a snapshot pin that the free
  step checks.

## The CRC32C source

Superblocks and pages use the `crc32c` crate (`crc32c = "0.6"` in
`third-party/rust/Cargo.toml`). `bf-tree-store` and `bf-tree-page` depend on
`//third-party/rust:crc32c`. The crate uses hardware CRC instructions
(SSE 4.2 and ARMv8 CRC). Its build script generates the CRC tables in
`OUT_DIR` and sets the `armsimd` cfg on aarch64, so
`third-party/rust/fixups/crc32c/fixups.toml` sets `buildscript.run = true`
([build system](build-system.md)). The write-ahead log
([plan 0020](../plans/0020-engine-wal.md), `engine-wal`) depends on the
same crate directly, so no cross-component export is needed. A
hand-written table function and the software-only `crc` crate were
rejected (plan 0017, resolved question 2).

## Extension points

- **Plan 0019 (apply, log hook, snapshots)** adds `apply`, `put_logged`,
  `remove_logged`, the `Log` trait, and one sequence space across internal
  and external sequences. It uses the same `write` closure, `applied`, and
  `last_seq`. It adds a pre-commit hook between `sync` and `commit`
  (`checkpoint_with`), and a snapshot pin that the free step checks under
  the `superseded` mutex. `select` takes the `W` to select against as a
  parameter, so a snapshot can pass the state it read once. Snapshot files
  use `Superblock::encode` and dense page numbers; pages are copied
  verbatim, so their stamps and checksums stay valid. The plan must not
  change the protocol, `write_through`, or the selection rule.
- **Plan 0021** calls `checkpoint_with` from a policy thread. The
  checkpointing mutex makes concurrent calls safe without stalling writers.
- **Plan 0023 (direct I/O and measurements)** adds an I/O mode to
  `FileStore`, and measures checkpoint cost against dirty leaves and open
  cost against the page count. The 4096-byte copies, full-copy writes, and
  the 8192-byte page origin are already aligned.
- **Leaf reclamation (deferred)** must erase, or otherwise fence, every
  stored page of a reclaimed leaf before it commits. Freed pages keep old
  copies, and selection would otherwise bring the leaf back.
- **A persisted fence list (deferred)** would make open proportional to
  leaves rather than pages. It could live in extra pages that the
  superblock names, which needs a new superblock version. Selection by
  header stays as the repair path.

## References

- [Plan 0017](../plans/0017-bf-tree-store-format.md): the store format,
  its decisions, and its discoveries.
- [Plan 0018](../plans/0018-bf-tree-checkpoint.md): shadow paging, the
  checkpoint protocol, recovery, its verification outcomes, and its
  discoveries.
- [BF-Tree paper](../references.md#bf-tree-paper), §3.3 (base pages and the
  mapping table) and §5.7 (logging, snapshots, and recovery). peppydb's
  checkpoint is fuzzy and does not pause writers, unlike the paper's online
  snapshot.
- [Microsoft implementation](../references.md#microsoft-bf-tree-implementation),
  revision `f442e6172bf7e5483ff320d8e3ac16a70e31bbd6`:
  [`src/fs/mod.rs`](https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/fs/mod.rs)
  (`VfsImpl` allocates offsets; peppydb uses fixed page numbers),
  [`src/storage.rs`](https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/storage.rs)
  (`PageTable`, `PageLocation`), and the CPR checkpoint code that used
  `snapshot_version`:
  [`src/nodes/leaf_node.rs`](https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/nodes/leaf_node.rs),
  [`src/mini_page_op.rs`](https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/mini_page_op.rs)
  (about lines 505 to 545), and
  [`src/snapshot.rs`](https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/snapshot.rs)
  with `doc/snapshot-recovery.md`, an in-place, pause-based snapshot that
  this design avoids.
- RFC 3720 §B.4: CRC32C (Castagnoli) check values.
- SQLite's database file format names store positions page numbers
  (`Pgno`); peppydb borrows the term.
