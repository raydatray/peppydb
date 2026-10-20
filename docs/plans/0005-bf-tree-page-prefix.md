# 0005: Fence-derived page prefix compression

**Status:** Agreed

**Design references:** [Shared BF-Tree pages](../design/bf-tree-pages.md).

**Parent roadmap:** [BF-Tree implementation](0002-bf-tree-roadmap.md), milestone 2.

**Builds on:** [Completed page mutation and ranges](0004-bf-tree-page-mutation.md).

**Implementation background:** [Upstream fences, prefixes, and previews](../explanations/bf-tree-upstream-pages.md#fences-prefixes-and-previews).

## Goal and scope

Store a shared key prefix once per fenced page while retaining allocation-free
borrowed reads. This completes the planned page-layout work in milestone 2 and
provides the logical-key interface needed by later tree and overlay operations.

- **In scope:** Fence-derived prefixes; suffix storage for ordinary records and
  the high fence; borrowed logical keys; prefix-aware lookup, range iteration,
  mutation, accounting, and compaction; independent model verification.
- **Later work:** Tree splits and fence replacement, page roles and overlay
  policy, stored-page validation, concurrency, and persistence. Compression does
  not change when a write grows, merges, or compacts a page.
- **Done when:** Compressed pages preserve 0004's logical behavior and failure
  guarantees; byte accounting matches independently decoded physical storage;
  key views compare by logical contents; and required workspace checks pass.

The 32-byte header, 8-byte slots, supported capacities, explicit infinity tags,
and little-endian encoding remain the format's foundation. Ordinary record keys
remain nonempty; values and finite fence/query bounds may be empty.

## Approach

### 1. Select a prefix from immutable fences

For two finite fences, initialization chooses their longest common prefix.
If either bound is unbounded, or the page has no fences, the prefix is empty.
Every key inside the half-open fence interval shares the selected prefix.

The prefix stays fixed while the fences stay fixed. Record insertion, removal,
and compaction must not recompute it from current records. A later split will
rebuild destination pages using their new fences and prefixes.

Activate `Header::prefix_len` at bytes `6..8`. The prefix bytes are the first
`prefix_len` bytes of the low fence's full key; they need no separate allocation
or slot. New initialization selects the maximum fence-derived prefix. Decoding
and mutation honor the stored length. A zero-prefix page remains a valid
representation even when its finite fences would permit compression.

For valid pages, a nonzero prefix requires two finite fences, must not exceed
their common-prefix length, and must match every ordinary logical key. With a
zero prefix, obtain an empty slice directly rather than following an infinite
fence's offset tag. Reopening arbitrary stored bytes remains separate work.

### 2. Store suffixes and retain logical lengths

| Entry | Stored key bytes | Slot key-length field | Preview |
| --- | --- | --- | --- |
| Finite low fence | Full key | Full length | First two full-key bytes |
| Finite high fence | Suffix after the page prefix | Full length | First two suffix bytes |
| Ordinary record, any kind | Suffix after the page prefix | Full length | First two suffix bytes |
| Unbounded fence | No payload; existing offset tag | Zero | Zero |

Previews are zero-padded. Offsets remain page-relative and address the stored
key bytes; values immediately follow the stored suffix. Kind and reference bits
keep their existing meanings. Infinite fences must be recognized before any
prefix subtraction or payload slicing.

Example: bounds `[acct/100, acct/200)` select `acct/` (five bytes).

| Entry | Logical key | Stored key |
| --- | --- | --- |
| Low fence | `acct/100` | `acct/100` |
| High fence | `acct/200` | `200` |
| Record | `acct/123` | `123` |
| Record | `acct/150` | `150` |

With one-byte values on both records, a 128-byte page uses 98 bytes without
compression and 83 with compression. Free space increases from 30 to 45 bytes.
The saving is five bytes each for the high fence and the two ordinary records.

For two finite fences and `n` ordinary records, packed size is:

```text
p           = stored prefix length
fence_bytes = 16 + lower.len() + (upper.len() - p)
packed_size = 32 + fence_bytes
            + sum(8 + (logical_key_len - p) + stored_value_len)
```

For an unbounded fence, omit its payload; its eight-byte slot remains. Such
pages have `p = 0`. Unfenced pages have no fence overhead and also use `p = 0`.
The existing payload-boundary equation still holds:

```text
payload_start = 32 + 8 * total_slot_count + free_bytes()
reclaimable   = capacity - packed_size - free_bytes()
```

`stored_value_len` includes value allocations retained by tombstones. Logical
absence views must not be used to calculate physical payload sizes.

### 3. Expose borrowed logical keys

A compressed key generally occupies two disjoint slices. Add `page/key.rs`:

```rust
#[derive(Clone, Copy, Debug)]
pub struct KeyRef<'p> {
    prefix: &'p [u8],
    suffix: &'p [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordRef<'p> {
    pub key: KeyRef<'p>,
    pub state: RecordState<'p>,
    pub referenced: bool,
}
```

Agreed `KeyRef` operations:

- `prefix()` and `suffix()` return their original borrowed slices; `len()` and
  `is_empty()` describe the logical key.
- `bytes()` iterates the concatenated bytes without allocation.
- `cmp_bytes(&[u8])` compares with an ordinary query key.
- `From<&[u8]>` creates an uncompressed view with an empty prefix.
- `to_vec()` explicitly materializes an owned full key when a caller needs it.

Implement `PartialEq`, `Eq`, `PartialOrd`, and `Ord` using logical concatenated
bytes, not structural comparison of the two fields. For example, `ab` + `c`
must compare equal to `a` + `bc`. This is required when comparing a compressed
base key with an uncompressed mini-page key. A chained byte iterator provides
a straightforward allocation-free reference implementation.

Accessors must preserve the underlying page lifetime rather than tie results
to a temporary `KeyRef` value. Reads and iteration keep borrowing `Page`; values
remain contiguous slices in `RecordState`. Point operations continue accepting
full `&[u8]` keys. The view does not implement an implicit contiguous-slice
conversion, since flattening can require allocation.

### 4. Generalize the fence view

The compressed high fence also needs a segmented view. Agreed API:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fence<K> {
    Unbounded,
    Key(K),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fences<K> {
    pub lower: Fence<K>,
    pub upper: Fence<K>,
}
```

| API | Key representation |
| --- | --- |
| `Page::init_with_fences` input | `Fences<&[u8]>`: full input keys |
| `Page::fences() -> Option<Fences<KeyRef<'_>>>` | Borrowed logical bounds |
| `RecordRef::key` | Borrowed logical key |
| Point-operation inputs and `range` bounds | Full byte slices, as in 0004 |

`K` has no explicit bound on the enum or struct. Page methods select the concrete
representations above; this stack introduces no generic key-access trait. The
derived traits apply conditionally, such as `Copy` when `K: Copy` and `Eq` when
`K: Eq`. An `AsRef<[u8]>` bound would be inappropriate because a segmented
`KeyRef` cannot generally expose one contiguous slice without copying.

The low fence is exposed as an uncompressed `KeyRef`; the high fence uses the
page prefix plus its suffix. `Unbounded` remains explicit, and a finite empty
key remains distinct from infinity. The outer `None` still means no fence slots.
Update the public re-exports in `page/mod.rs` and `src/lib.rs` together.

This shares one fence-kind definition between full-key inputs and segmented
outputs. The alternative is separate input and output fence enums; the generic
family avoids duplicating their semantics. Generic fence annotations may require
callers to specify `Fences<&[u8]>` or use slice inputs instead of array references.

### 5. Classify the query before searching suffixes

Point operations first retain 0004's empty-key and fence-membership checks.
Membership compares logical fence views with the full query key. Range endpoints
may be empty, shorter than the prefix, or outside the page's interval.

A single `search` replaces 0004's `lower_bound`. It returns `Ok(index)` for a
stored key and `Err(index)` with the ordinary-record insertion position in
`0..=len()` otherwise. Strip the shared prefix once before binary-searching
suffixes:

```rust
match key.strip_prefix(prefix) {
    // Compare meaningful suffix previews, then stored suffixes on a tie.
    Some(query) => self
        .record_slots()
        .binary_search_by(|slot| self.compare_suffix(slot, query)),
    None if key < prefix => Err(0),
    None => Err(self.len()),
}
```

A query without the prefix either sorts before it, including a query that ends
inside the prefix, or diverges above it and sorts after every record. This
preserves full-key ordering without allocating or repeatedly comparing the
shared prefix at every binary-search probe. A query equal to the prefix has an
empty suffix and must be handled normally. With `p = 0`, the algorithm reduces
to full-key search.

Keys on a page are unique, so `binary_search_by` returns the lower bound in
both variants. Point operations use the variant directly: `get`, `remove`, and
`set_referenced` need no second key comparison, `try_insert_record` rejects
`Ok` as a duplicate, and `try_upsert` replaces on `Ok` and inserts on `Err`.
Range endpoints use the index from either variant. Iteration reconstructs
views, not owned keys.

### 6. Adapt mutation, accounting, and compaction

Initialization validates capacity and finite fence ordering, calculates the
prefix and compressed fence footprint, then changes the buffer. Oversized
fences return `FencesTooLarge` with the original bytes intact.

Insertion and upsert validate the full key first, then calculate physical space:

```text
stored_key_len = logical_key_len - prefix_len
new_slot_size  = 8 + stored_key_len + incoming_value_len
growing_payload_size = stored_key_len + incoming_value_len
```

Check logical key and value lengths against the encoded field widths before
narrowing integers. `RecordTooLarge` uses an otherwise empty page with its actual
compressed fences and prefix. `PageFull` still describes unavailable contiguous
space; no mutation implicitly compacts or credits an obsolete payload.

Smaller replacements locate the value after the stored suffix. Growing
replacements copy only the incoming suffix and value. Metadata-only deletion
preserves its suffix, retained value allocation, and reference bit. Physical
removal returns its slot immediately and leaves its physical payload as a hole.

`compact` preserves the stored prefix, logical keys, fences, kinds, references,
retained values, and backing/recovery metadata. It copies physical payloads from
the original page into scratch storage. The shared prefix remains part of the
low-fence payload and is counted once. The packing helper needs each slot's
role: the low fence is uncompressed, finite high/ordinary keys skip `p`, and
infinite fences have no payload. It must not derive physical sizes from logical
`KeyRef::len()` or `RecordState::value()`.

An ordinary logical key can equal the prefix. For `[abc, abcd)`, record key
`abc` has an empty suffix. With an empty value it needs a slot and zero payload
bytes. Such offsets may address an empty range at the payload boundary, and
previews are zero. The logical key remains nonempty. Empty finite fences select
a zero prefix and retain their ordinary offsets, distinct from infinity tags.

## Decisions and trade-offs

- **Fence-derived, stable prefix:** Compress every possible in-range key without
  changing the prefix after record mutations. New fences require a later rebuild.
- **Full logical lengths in slots:** Follow the shared format's intended meaning;
  make physical suffix lengths explicit in helpers and accounting.
- **Borrowed views:** Avoid allocating full keys for every read. Callers that
  need contiguous owned keys request materialization explicitly.
- **Generic fences — agreed:** Use `Fence<K>` and `Fences<K>` without a type-level
  bound on `K`, with full slices for initialization and `KeyRef` for borrowed
  output. This shares the fence-kind definition across both representations.
- **Compress the high fence:** Follow the upstream layout and save another
  prefix copy. This requires a logical fence-view API as well as record views.
- **Keep compaction and record policy separate:** Compression changes physical
  representation, not which records may be discarded or when a page grows.
- **Safe page access:** Use checked byte views and slices. This stack does not
  require raw-pointer page ownership or a new synchronization protocol.

## Agreed commit stack

Seven diffs: the plan followed by six implementation/model diffs. Prepare reads
and writes before enabling nonzero prefixes in public initialization. Every diff
must compile and pass its relevant tests. Include focused behavior tests and
design-note updates with the code; the final model diff checks interactions.
Aim for about 300 changed lines per substantive diff. The API migration can
exceed that guideline if its excess is mechanical caller/test adaptation.

### `[1/7][bf-page-prefix][plan] define fence-derived compression contracts`

- **Purpose:** Record the agreed layout, key/fence interfaces, and stack boundaries.
- **Changes:** Finalize this plan and link it from the roadmap and page design.
- **Verification:** Review byte accounting, API lifetimes, and source links.

### `[2/7][bf-page-prefix][key] add borrowed logical key views`

- **Purpose:** Establish an allocation-free key abstraction independent of layout.
- **Changes:** Add `page/key.rs`, its public export, and the `KeyRef` operations
  above. Keep equality/ordering independent of how bytes are split.
- **Verification:** Compare different segmentations with flattened byte strings,
  including empty parts, prefix keys, zero bytes, and ordering ties.

### `[3/7][bf-page-prefix][api] expose logical keys through record and fence views`

- **Purpose:** Migrate consumers before changing physical storage.
- **Changes:** Use `KeyRef` in `RecordRef`; generalize `Fence`/`Fences`; update
  `fences`, membership comparisons, exports, examples, and tests. Current pages
  return uncompressed views and initialization still writes zero prefix length.
- **Verification:** Existing behavior and bytes remain valid; borrowed views
  outlive temporary view objects through their page lifetime. Compare fence
  results with input bounds by logical bytes despite their different key types.

### `[4/7][bf-page-prefix][read] decode compressed keys and search suffixes`

- **Purpose:** Read prefixed layouts and handle arbitrary queries safely.
- **Changes:** Separate logical key lengths from physical ranges in `layout.rs`;
  add prefix access, logical key/fence decoding, and prefix-classified search.
  Use controlled compressed fixtures while constructors still select `p = 0`.
- **Verification:** Point/range results against full-key ordering; mismatching
  and shorter query prefixes; empty suffixes; all kinds; meaningful previews;
  and borrowed values at suffix-relative offsets. Activate prefix-field invariants
  in old fixtures that previously treated bytes `6..8` as inactive metadata.

### `[5/7][bf-page-prefix][write] account for suffixes in page mutations`

- **Purpose:** Preserve physical mutation guarantees on compressed pages.
- **Changes:** Adapt slot construction, `record_size`, `record_capacity`, value
  offsets, payload writes, reclamation accounting, and compaction. Centralize
  physical-range handling so structural slots and infinity tags are treated
  correctly. Test mutations on controlled compressed fixtures.
- **Verification:** All state transitions, retained tombstone reuse, shrink/grow
  cycles, zero-byte payloads, physical removal, exact fits, and unchanged bytes
  on errors. Compaction preserves prefixes, hints, and unrelated metadata.

### `[6/7][bf-page-prefix][layout] initialize pages with fence-derived prefixes`

- **Purpose:** Enable compressed storage through the normal page constructor.
- **Changes:** Compute the maximal prefix and exact fence footprint in
  `init_with_fences`; encode the full low fence and compressed high fence.
  Unfenced and one-sided/unbounded pages continue selecting `p = 0`.
- **Verification:** Constructor failure atomicity; empty finite versus unbounded
  bounds; golden bytes; prefixes of length 0, 1, 2, and longer; compressed-only
  fence fits; and the worked example's exact 15-byte saving.

### `[7/7][bf-page-prefix][tests] verify compressed pages against logical records`

- **Purpose:** Check interactions across compressed reads, mutations, and fences.
- **Changes:** Extend the independent model and raw-byte validator; finalize
  design documentation, roadmap progress, and verification outcomes.
- **Verification:** Mixed sequences with full logical keys, varied fence prefixes,
  capacities, states, and value lengths; all required workspace checks.

## Open questions

None.

## Verification strategy

Extend the existing `BTreeMap` model with independently calculated fence prefixes
and physical suffix lengths. Keep full logical keys and retained physical values
in the model. Track the contiguous gap through operation history as in 0004.
Determine fit using physical bytes, not production sizing helpers. Compare key
views with owned logical keys and require every failed operation to preserve
the complete page image.

The raw-byte validator checks header prefix length, full encoded key lengths,
suffix previews, role-specific physical ranges, value offsets, infinity tags,
slot/payload non-overlap, and holes. Mark the low-fence allocation once; repeated
logical key views borrowing its prefix are not duplicate physical allocations.

| Case | Required result |
| --- | --- |
| Unfenced, unbounded, one-sided, or no-common-prefix page | Zero prefix; existing semantics and zero-prefix encoding. |
| Bounds `[acct/100, acct/200)` with two one-byte-valued records | Five-byte prefix; 15 bytes saved versus full-key storage. |
| Bounds `[abc, abcd)` with record `abc` and empty value | Nonempty logical key, zero suffix/value payload, one ordinary slot. |
| Query shorter than the prefix or mismatching inside it | Correct lower bound without out-of-range slicing. |
| Finite empty range/fence bound | Distinct from infinity; ordinary empty-key rejection still applies. |
| Identical keys with different prefix/suffix splits | Equal and identically ordered. |
| Record/fences fit only after compression | Accepted with exact physical free-space accounting. |
| Delete followed by replacement and compaction | Retained physical value and reference behavior match 0004. |
| Remove every ordinary record | Fences and stored prefix survive; space accounting is correct. |
| Stored zero prefix with compressible bounds | Decodes and mutates according to the stored length. |

Use all supported capacities for constructor boundaries and the existing model
capacities for mixed sequences, including 448, 832, 1600, and 4032 bytes. Retain
unaligned-buffer, count-above-255, embedded-zero, and preview-collision coverage.

Run from the repository root:

```sh
cargo test -p peppydb-bftree
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc -p peppydb-bftree --no-deps
```

Verify concrete byte savings separately from throughput. No performance claim
or new benchmark framework is part of this stack.

## Progress

- [x] Review the completed 0004 API, physical accounting, and tests.
- [x] Draft the compression proposal and seven-diff stack.
- [x] Agree on generic fences and their concrete input/output representations.
- [x] Agree on the complete design and seven-diff stack.
- [x] Finalize diff 1: approved plan for implementation handoff.
- [x] Implement and verify diff 2: logical key views.
- [x] Implement and verify diff 3: record/fence API migration.
- [x] Implement and verify diff 4: prefix-aware decoding and search.
- [x] Implement and verify diff 5: physical mutation and compaction accounting.
- [ ] Implement and verify diff 6: compressed initialization.
- [ ] Implement and verify diff 7: model checks and final verification.
- [ ] Update the design note and mark milestone 2 complete after delivery.

## Agent notes and review outcomes

- **Planning verification:** Reviewed the completed 0004 API, compressed byte
  accounting, lifetime/ordering contracts, local links, and seven-diff sequencing.
  Diff 1 is documentation-only; Rust checks remain for the implementation diffs.
- **Implementation handoff:** The plan is agreed and implementation has not
  started. Begin with diff 2, `page/key.rs` and its logical-key tests. The plan
  commit is diff 1; use `[n/7][bf-page-prefix][section]` for the remaining stack.
- **Baseline inspected:** `f6914397bdcf`, the completed ten-diff 0004 stack.
- **Key entry points:** `page/layout.rs` currently combines full length and
  physical range in `Slot::key_range`; `read.rs` owns logical record indexing and
  preview search; `fence.rs` owns initialization, membership, and record capacity;
  `write.rs` owns size preflight and payload placement; `compact.rs` copies stored
  payloads rather than logical record values.
- **Avoid double counting:** Current `record_capacity` sums lengths obtained
  through `fences()`. Once those are logical key views, use physical fence bytes
  instead. Infinity slots must be skipped before subtracting a nonzero prefix.
- **Prefix source:** Read the prefix directly from the low fence's physical
  allocation. Avoid a recursive dependency between prefix access and `fences()`
  decoding of the high fence.
- **Existing fixtures:** `tests/write.rs` currently fills `6..32` with arbitrary
  inactive metadata. Bytes `6..8` become active; preserve valid prefixes and use
  `8..32` for unrelated metadata tests. Adapt the physical validator and model's
  `structural_bytes`/`packed_bytes` without reusing production encoding helpers.
- **Documentation:** Extend `docs/design/bf-tree-pages.md` with the delivered
  compressed format and logical-key contract. Keep detailed upstream discussion
  in the linked explanation. Record implementation check results here as each
  diff lands; planning review does not substitute for those checks.

## Sources

- [BF-Tree paper](../references.md#bf-tree-paper), §3.5: fence-derived prefix
  compression and look-ahead bytes; §3.2: the shared page format.
- [Upstream page explanation](../explanations/bf-tree-upstream-pages.md#fences-prefixes-and-previews),
  Microsoft revision `f442e6172bf7e5483ff320d8e3ac16a70e31bbd6`: stored logical
  lengths, full low fence, suffix previews, and zero-prefix ordinary mini-pages.
- [Completed 0004 contract](0004-bf-tree-page-mutation.md) and its current
  implementation in `crates/peppydb-bftree/src/page/`.
