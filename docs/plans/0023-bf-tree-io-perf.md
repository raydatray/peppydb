# 0023: Direct I/O and performance

**Status:** Agreed

**Design references:** [persistence](../design/bf-tree-persistence.md),
[concurrency](../design/bf-tree-concurrency.md),
[shared pages](../design/bf-tree-pages.md),
[circular buffer](../design/bf-tree-circular-buffer.md),
[engine](../design/engine.md),
[build system](../design/build-system.md).

**Parent roadmap:** [BF-Tree implementation](0002-bf-tree-roadmap.md),
milestone 9 (completes it).

**Builds on:** [Durable store format](0017-bf-tree-store-format.md),
[shadow paging, checkpoint, and recovery](0018-bf-tree-checkpoint.md),
[concurrency](0016-bf-tree-concurrency.md),
[engine `Db`](0021-engine-db.md), [external-log mode](0022-engine-external.md).

## Goal and scope

The mechanisms of milestones 5-9 are complete. Two things remain before the
milestone can close. First, through the page cache the circular buffer's
budget bounds nothing: the kernel caches every page a second time, so a
larger-than-memory benchmark measures RAM, and cached store writes can
interfere with the WAL's fsync. Second, the hot paths carry costs that the
design does not require: redundant fence checks, out-of-line byte
comparisons, dependent cache misses in binary searches, a heap allocation
per write's descent, and possibly a second base-page read per eviction.
This stack adds direct I/O to the file store, removes those costs where
Linux measurements confirm them, commits the benchmarks, and records Linux
numbers in the design documents.

- **In scope:**
  - `Io { Cached, Direct }` for `FileStore`, with `O_DIRECT` on Linux through
    an aligned bounce buffer and no `unsafe`;
  - batched aligned reads for the open-time page scan;
  - `engine::Options::io`;
  - page and lookup fast paths: routed lookups without fence checks, integer
    comparison of previews and short key parts;
  
  - committed `#[ignore]` benchmarks for persistence, the circular buffer,
    the engine, and the WAL;
  - Linux measurements in the design documents.
- **Out of scope:** asynchronous I/O (`io_uring`), direct I/O on macOS,
  persisting the leaf selection to make open proportional to leaves rather
  than pages, background merges, spin wrappers around locks.
- **Done when:** `Io::Direct` passes the store contract tests and the crash
  model on Linux; each fast path either lands with a measured Linux
  improvement or is recorded as rejected; every benchmark in the
  verification table runs and its Linux results are in the design documents;
  the roadmap marks milestone 9 complete; workspace checks pass.

## Approach

### Direct I/O

```rust
/// How a FileStore reaches its file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Io {
    /// Through the page cache. Portable. The kernel caches pages a second
    /// time, outside the circular buffer's budget.
    #[default]
    Cached,
    /// Bypassing the page cache (O_DIRECT). Linux only; 4096-byte pages.
    /// The circular buffer's budget bounds the memory the data occupies,
    /// and a circular-buffer miss reaches the device.
    Direct,
}

impl FileStore {
    pub fn create(path: impl AsRef<Path>, superblock: Superblock) -> Result<Self, StoreError>; // Cached
    pub fn create_with(path: impl AsRef<Path>, superblock: Superblock, io: Io) -> Result<Self, StoreError>;
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError>;                          // Cached
    pub fn open_with(path: impl AsRef<Path>, io: Io) -> Result<Self, StoreError>;
    pub fn io(&self) -> Io;
}
```

`O_DIRECT` requires the buffer address, the file offset, and the length to be
multiples of the device's logical block size. The store meets the offset and
length rules by construction: 0017 fixes superblock copy 2 at offset 4096 and
pages at multiples of the page size, and `Direct` requires a 4096-byte page.
4096 covers devices with 512-byte and 4096-byte logical blocks. The
buffer-address rule needs care, because the tree's buffers do not meet it.
Scratch pages are ordinary heap slices with no alignment guarantee.
Circular-buffer blocks, including `Full` pages, are byte ranges in one slab
that is aligned to 64 bytes; each block follows a 16-byte header and blocks
are packed in the ring, so a block is not 4096-aligned in general.

```rust
/// One page, aligned for O_DIRECT. Safe: alignment is a type property, and
/// zerocopy views a slice of these as bytes without casts.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, align(4096))]
struct AlignedPage([u8; 4096]);

thread_local! {
    /// Reused per thread; grows to the largest run read (open's batches).
    static BOUNCE: RefCell<Vec<AlignedPage>> = const { RefCell::new(Vec::new()) };
}

fn write_page(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
    match self.io {
        Io::Cached => self.file.write_all_at(buf, offset),
        Io::Direct if buf.as_ptr().addr() % 4096 == 0 => self.file.write_all_at(buf, offset),
        Io::Direct => with_bounce(1, |bounce| {
            bounce.copy_from_slice(buf);
            self.file.write_all_at(bounce, offset)
        }),
    }
}
```

- **No `unsafe` in `bf-tree-store`.** (In the workspace, only
  `bf-tree-circular-buffer` may use `unsafe`.) `#[repr(align(4096))]` and
  `Box`/`Vec` allocation give an aligned bounce buffer;
  `zerocopy::IntoBytes::as_mut_bytes` views `[AlignedPage]` as bytes. `<*const u8>::addr` is a safe pointer-to-integer
  read; when the caller's buffer happens to be aligned the copy is skipped.
- **One copy per I/O otherwise.** A 4 KiB `memcpy` costs far less than a
  device access.
- **Durability unchanged.** `O_DIRECT` bypasses the page cache, not the
  device's volatile cache, and does not persist file-size changes. `sync` and
  `commit` still call `sync_data`.
- **Linux only.** macOS's equivalent is `fcntl(F_NOCACHE)`, which needs a
  `libc` call that is `unsafe`. On other systems `create_with`/`open_with`
  with `Io::Direct` return `StoreError::DirectIo("direct I/O requires Linux")`;
  a page size other than 4096 returns `StoreError::DirectIo("direct I/O
  requires 4096-byte pages")`.
- **`tmpfs` rejects `O_DIRECT`** with `EINVAL` at open. Tests and benchmarks
  that use `Direct` take their directory from `PEPPYDB_BENCH_DIR` or a
  directory on a real file system, and report a skip, not a pass, when
  direct open fails with `EINVAL`.

**Where the flag comes from.** `OpenOptionsExt::custom_flags(i32)` is safe;
only the value of `O_DIRECT` is needed, and it differs by architecture
(`0o40000` on x86_64, `0o200000` on aarch64). Options:

| Option | Cost | Notes |
| --- | --- | --- |
| `libc` as an unconditional third-party dependency | One crate built on every platform, used only under `cfg(target_os = "linux")` | Correct constants for every Linux architecture. `autocargo` needs no change. Recommended. |
| Extend `tools/autocargo` and `peppy_rust_library` for target-specific deps (Buck2 `select()` on `config//os:linux`; `[target.'cfg(..)'.dependencies]` in the manifest) | Tooling work and a new pattern | Exact, and useful later; more than this stack needs. |
| Hand-written constants per architecture, no dependency | A small `cfg(target_arch)` table; `Direct` unsupported elsewhere | No new dependency; a wrong constant is caught by the test below. |

Whichever option is chosen, a Linux test proves `O_DIRECT` is in effect: a
read into a deliberately misaligned buffer through the store's file handle
(crate-private test access) fails with `EINVAL`. With the recommended option,
add `libc = "0.2"` to `third-party/rust/Cargo.toml`, run `tools/buckify`, add
`third-party/rust/fixups/libc/fixups.toml` with `buildscript.run = true` if
reindeer warns, add `//third-party/rust:libc` to `crates/bf-tree-store/BUCK`,
and run `tools/autocargo`.

### Batched reads at open

Open (0018) reads every page. With `Direct`, each `read` is a synchronous
device round trip, so open runs at the device's queue-depth-one rate. This
stack adds a default method to the store trait and uses it in open:

```rust
pub trait PageStore: Send + Sync {
    // ... existing methods ...
    /// Read `buf.len() / page_size` consecutive pages starting at `first`.
    /// The default reads them one at a time.
    fn read_run(&self, first: PageNumber, buf: &mut [u8]) -> Result<(), StoreError> {
        buf.chunks_mut(self.page_size())
            .zip(first.raw()..)
            .try_for_each(|(page, raw)| {
                let page_number = PageNumber::new(raw).expect("page numbers stay below u64::MAX");
                self.read(page_number, page)
            })
    }
}
```

`FileStore` overrides it with one positioned read of up to 64 pages
(256 KiB) through the bounce buffer. The method is additive; existing
callers and `MemStore` are unchanged.

### Engine option

`engine::Options` gains `io: bf_tree::Io` (default `Cached`), passed to
`FileStore::create_with` / `open_with`. `bf-tree` re-exports `Io`. This is
the extension point 0021 named.

### Page and lookup fast paths

Each fast path must keep behaviour identical and is checked by the existing
page, inner-node, and tree model tests. Each lands only if the Linux
benchmark named in its diff improves beyond run-to-run noise; otherwise the
diff is dropped and the result recorded under Discoveries.

1. **Routed lookups skip fence checks.** `Page::check_key` compares the key
   with both fences on every `get` (decoding the fences each time). A key that
   reached the leaf by a validated route is inside the fences by
   construction. Add routed variants, `Page::get_routed` and
   `Page::find_index_routed`, that skip the comparison and keep it as a
   `debug_assert!`. The tree uses them only after route validation.
2. **Integer comparisons.** Leaf slot previews (2 bytes) and inner-node
   inline bytes (4 bytes) are zero-padded. Compare them as big-endian
   integers against a query word computed once per search; read the stored
   bytes only on a tie. A strict integer inequality gives the order of the
   full keys: at the first differing byte either both bytes are real, or the
   padded side is a shorter key that is a prefix of the other and therefore
   smaller. In `KeyRef::cmp_bytes`, compare parts of at most 8 bytes by loading
   them big-endian into `u64` with fixed-length loads, then by length; longer
   parts use slice comparison.

   ```rust
   #[inline]
   fn cmp_short(a: &[u8], b: &[u8]) -> Ordering {
       let common = a.len().min(b.len());
       if common > 8 {
           return a.cmp(b);
       }
       load_be(&a[..common]).cmp(&load_be(&b[..common])).then(a.len().cmp(&b.len()))
   }
   ```

### Benchmarks

All are `#[ignore]` tests, run with
`tools/buck2 run @build/mode/release //crates/<x>:<x>-unittest -- --ignored --nocapture <filter>`.
Contended numbers come from Linux only: the macOS std `RwLock` parks readers
under reader contention, and its `Mutex` does not spin before sleeping, so
macOS profiles overstate lock costs.

| Benchmark | Crate | What it measures |
| --- | --- | --- |
| `baseline_threads`, `baseline_hot_set`, `baseline_components` (from 0016) | `bf-tree` | Re-run after the fast paths; record before and after |
| `bench_persistence` | `bf-tree` | Load of N keys; checkpoint of all leaves and of 10% dirty leaves; file size before and after checkpoints; open rate (pages/s); random `get` with the circular buffer smaller than the data; each with `Cached` and `Direct` |
| `bench_eviction_io` | `bf-tree` | Replacing puts into a file larger than the circular buffer; ops/s, p50, p99 |
| `bench_circular_buffer_free` | `bf-tree-circular-buffer` | Size-class free lists against tombstone-on-free (a crate-private switch used only by this benchmark): alloc/free throughput, budget utilisation, eviction count under a recorded trace |
| `bench_circular_buffer_lock` | `bf-tree` | 12-thread `put` under a 32 MiB circular-buffer budget; share of time waiting on the circular-buffer mutex (`perf` profile on Linux) |
| `bench_head_stall` | `bf-tree` | 12-thread `put` under a small circular-buffer budget while one head eviction is artificially delayed (a test-only hook in the eviction write-back); evictions per allocation failure and p99 `put` latency, with and without the delay. Decides whether plan 0014 needs `has_room_after_claims` and a `Condvar` wait |
| `bench_engine_write` | `engine` | `put` ops/s, p50, p99, max on 1-12 threads; `flush_interval` 0 and 1 ms; `Io::Cached` and `Io::Direct`; average group size |
| `bench_wal_append` | `engine-wal` | Time inside the pending mutex for 16-byte and 1 KiB values |
| `bench_wal_recovery` | `engine` | Replay rate (records/s) onto `Cached` and `Direct` stores |

**Questions the measurements must answer** (state each answer, with data, in
the design documents):

- Does a positive `flush_interval` ever beat 0 on the test device?
- Does cached store I/O slow the WAL's fsync? Hypothesis to test: on ext4
  with `data=ordered`, an fsync that commits the journal (the WAL file grows,
  so its size changes) also forces out the store's dirty data. Procedure:
  measure WAL fsync latency alone; with concurrent cached store write-through;
  with `Direct`; and, if available, with the WAL on a different file system.
  If confirmed, document `Io::Direct` as the Linux recommendation and record
  WAL segment preallocation as a candidate mitigation; do not implement it in
  this stack.
- Is the circular-buffer mutex contended on Linux? std's Linux mutex already
  spins briefly before it sleeps. If contention is significant, record it and
  propose shortening critical sections in a later plan. Spin wrappers are not
  an option.
- How much do size-class free lists save over tombstone-on-free? Plan 0010
  keeps free lists to stay faithful to the paper and upstream; this
  measurement is informational. Changing the default would need its own
  plan.

**Environment.** Record CPU, core count, kernel, file system and mount
options, device model, and whether the machine is virtual. A virtual disk's
fsync latency is the hypervisor's, not a device's; label such numbers. Prefer
bare-metal Linux with NVMe for the recorded tables. No shared test
instrumentation (counters, logging) may run in release benchmarks; a shared
counter in a lock path can halve a contended rate.

## Decisions and trade-offs

- **No slot-directory warming (review, 2026-11-07).** An earlier draft
  touched one byte per cache line of the slot directory before a binary
  search, so that a cold leaf's dependent cache misses would overlap.
  Dropped for now: upstream does no warming or prefetching (its
  `LeafNode::lower_bound` is a plain binary search), the gain is
  hardware-dependent and possibly covered by hardware prefetchers, and a
  cold leaf's cost is dominated by its page read. It can return as its own
  measured change if a profile shows cold binary searches as a hotspot; a
  denser preview layout (fewer lines to touch) would come first.

- **Bounce buffer over aligned tree buffers.** Every caller works unchanged,
  and an aligned caller skips the copy. Alternative: align the tree's scratch
  pages; a later optimisation if the copy ever shows up. Circular-buffer
  blocks cannot take this route: their 16-byte headers and packed layout
  rule out guaranteed 4096-byte alignment, so a `Full` page's write-back
  normally goes through the bounce buffer.
- **`Cached` stays the default.** Portable and fast for data that fits in RAM.
  The design documents recommend `Direct` on Linux when data exceeds memory
  or when the WAL shares a file system with the store.
- **Measurement gates each fast path.** Several of these optimisations depend
  on the CPU's cache behaviour. Code that does not pay for itself on the
  target platform is not merged.
- **Routed variants instead of removing the check.** The checked methods stay
  for callers whose keys did not come from a route.
- **`read_run` as a default trait method.** Additive; no backend must change.
- **No `io_uring`.** It needs `unsafe` or a large dependency. Batched reads
  cover the open path; the remaining synchronous writes are measured.

## Extension points

- `Io` gains variants only if a new safe mechanism appears (for example, a
  safe `io_uring` wrapper). `FileStore` callers do not change.
- `PageStore::read_run` can serve `write_snapshot` and a future persisted
  leaf selection.
- The benchmarks are the baseline for phase 2: replication adds network
  latency on top of `bench_engine_write` and snapshot transfer on top of
  `bench_persistence`.
- Later plans must not need to change the store trait's existing methods,
  the page format, or the engine API to use direct I/O.

## Proposed commit stack

Ten diffs.

### `[1/10][bf-io-perf][plan] plan direct I/O and the performance pass`

- **Purpose:** Agree on direct I/O, the fast paths, and the measurements.
- **Core changes:** This plan; link it from the roadmap's milestone 9 row.
- **Verification:** Review. Check local links.

### `[2/10][bf-io-perf][tools] make the Linux container build and give benchmarks a native volume`

- **Purpose:** Run the Linux tests and benchmarks in the container from
  plan 0016, which had never run (no Docker engine then).
- **Core changes:** `tools/linux/Dockerfile` installs `clang` and `lld`:
  the Buck2 prelude's system C++ toolchain links with `clang++`.
  `tools/linux-run` mounts a named volume at `/bench` and sets
  `PEPPYDB_BENCH_DIR=/bench`, so benchmark and `Io::Direct` data live on
  the VM's own ext4 disk, not on the bind-mounted checkout from macOS.
- **Verification:** `tools/linux-run tools/buck2 test //crates/...` passes;
  `stat -f /bench` inside the container reports ext4.

### `[3/10][bf-io-perf][store] add direct I/O to the file store`

- **Purpose:** Let the circular buffer's budget bound memory on Linux.
- **Core changes:** `Io`, `create_with`, `open_with`, `io()`,
  `StoreError::DirectIo`, the bounce buffer with the aligned fast path, the
  `O_DIRECT` source chosen in the open question, third-party and `BUCK`
  changes, `tools/buckify` and `tools/autocargo` output.
- **Verification:** The store contract suite runs with `Direct` on Linux;
  the misaligned-read test proves `O_DIRECT`; a store written with `Direct`
  reopens with `Cached` and the reverse; non-Linux and non-4096 page sizes
  return `DirectIo`; the crash model (0018) passes over a `Direct` store.

### `[4/10][bf-io-perf][store] read pages in aligned runs at open`

- **Purpose:** Avoid one device round trip per page during open.
- **Core changes:** `PageStore::read_run` with a default; the `FileStore`
  override; open's scan uses runs.
- **Verification:** Open results identical with runs of 1, 7, and 64 pages
  and with a page count that is not a multiple of the run; `bench_persistence`
  open rate with `Direct` before and after.

### `[5/10][bf-io-perf][engine] pass the store I/O mode through engine options`

- **Purpose:** Make direct I/O available to applications.
- **Core changes:** `bf-tree` re-exports `Io`; `engine::Options::io`; open
  and create use it.
- **Verification:** Engine tests pass with `Io::Direct` on Linux (selected by
  a test that iterates both modes and skips `Direct` elsewhere).

### `[6/10][bf-io-perf][page] skip fence checks on routed lookups`

- **Purpose:** Remove a fence decode and two comparisons per base lookup.
- **Core changes:** `Page::get_routed`, `Page::find_index_routed` with debug
  assertions; tree call sites after route validation.
- **Verification:** Page and tree model tests; a debug-build test that a
  routed call with an out-of-range key panics; `bench_components` random
  `get` before and after.

### `[7/10][bf-io-perf][search] compare previews and short key parts as integers`

- **Purpose:** Remove out-of-line comparisons from every binary-search probe.
- **Core changes:** Preview and inline words in `bf-tree-page` and
  `bf-tree-inner` layouts; query words computed once per search;
  `cmp_short` in `KeyRef::cmp_bytes`.
- **Verification:** Exhaustive comparison of `cmp_short` with slice order for
  all pairs of byte strings up to length 3 over the alphabet {0, 1, 255},
  plus random longer pairs; page and inner model tests; hot-leaf and descent
  benchmarks before and after.

### `[8/10][bf-io-perf][bench] add persistence and circular-buffer benchmarks`

- **Purpose:** Commit the tree-side measurements.
- **Core changes:** `bench_persistence`, `bench_eviction_io`,
  `bench_circular_buffer_lock`, `bench_circular_buffer_free` with its crate-private switch,
  `bench_head_stall` with its test-only delay hook.
- **Verification:** Each benchmark runs in release on Linux and macOS
  (direct parts skip on macOS); default test runs ignore them.

### `[9/10][bf-io-perf][bench] add engine and WAL benchmarks`

- **Purpose:** Commit the engine-side measurements.
- **Core changes:** `bench_engine_write`, `bench_wal_append`,
  `bench_wal_recovery`.
- **Verification:** As above.

### `[10/10][bf-io-perf][docs] record Linux measurements and complete milestone 9`

- **Purpose:** Record results and close the milestone.
- **Core changes:** `bf-tree-persistence.md` (direct I/O, persistence
  numbers, the ext4 finding), `bf-tree-concurrency.md` (Linux thread scaling,
  hot set, circular-buffer lock), `bf-tree-pages.md` (routed lookups,
  integer compares), `bf-tree-circular-buffer.md` (free-list
  result), `engine.md` (write latency, recovery rate, interval and I/O-mode
  answers); this plan's outcomes; roadmap milestone 9 complete.
- **Verification:** Workspace checks; every number in the documents names its
  benchmark, configuration, and environment.

## Open questions

None.

Resolved (review, 2026-11-08):

- **`O_DIRECT` constant source: `libc` as an unconditional dependency.**
  `libc::O_DIRECT` is correct on every Linux architecture, which matters
  here: the benchmark path runs on aarch64 (Docker on Apple Silicon) and
  x86_64 (WSL2, GCP), whose values differ (`0o200000` and `0o40000`). It
  is used only under `cfg(target_os = "linux")`.
- **Benchmark environments, in order.** Each result is labelled with its
  environment:
  1. *Docker on macOS (aarch64 Linux VM), indicative.* Correctness of
     `Io::Direct` and first measurements. Data must be on a named Docker
     volume (`PEPPYDB_BENCH_DIR`), never the bind-mounted checkout. I/O
     numbers reflect a virtual disk backed by the Mac's SSD.
  2. *WSL2 (x86_64), indicative.* CPU fast paths and plan 0016's contended
     benchmarks; the repository and data on the Linux file system, not
     `/mnt/c`. I/O through a virtual disk.
  3. *GCP with a Local SSD (NVMe), reference.* The numbers the design
     documents treat as the baseline. Default persistent disks are network
     storage and would distort fsync latency.

## Verification strategy

| Check | Command or procedure | Expected result |
| --- | --- | --- |
| Direct I/O correctness | Store contract suite and crash model with `Io::Direct` on Linux | Pass |
| `O_DIRECT` in effect | Misaligned read through the store's handle | `EINVAL` |
| Fast paths preserve behaviour | Page, inner-node, tree, and engine model tests | Pass unchanged |
| Comparison order | Exhaustive `cmp_short` test | Matches slice order |
| Fast paths pay off | Before/after Linux benchmarks named in each diff | Gain beyond noise, or diff dropped and recorded |
| Measurements recorded | `tools/buck2 run @build/mode/release //crates/bf-tree:bf-tree-unittest -- --ignored --nocapture bench_`; same for `bf-tree-circular-buffer`, `engine-wal`, `engine` | Tables in the design documents with environment |
| Workspace | `tools/fmt.sh`; `tools/clippy.sh`; `tools/buck2 test //crates/... //bin/... //go/...`; `tools/autocargo --check`; `tools/gobuckify-remote --check`; `cargo test --workspace --locked`; `(cd go && go vet ./... && go test ./...)` | All pass |

## Progress

- [x] Agree on the design and commit stack with the user.
- [x] Diff 2: Linux container and benchmark volume.
- [x] Diff 3: direct I/O.
- [ ] Diff 4: batched open reads.
- [ ] Diff 5: engine I/O option.
- [ ] Diff 6: routed lookups.
- [ ] Diff 7: integer comparisons.
- [ ] Diff 8: persistence and circular-buffer benchmarks.
- [ ] Diff 9: engine and WAL benchmarks.
- [ ] Diff 10: measurements and milestone completion.
- [ ] Stack-wide verification.

## Discoveries and plan changes

- **Plan 0016's container had never built anything.** With a working
  Docker engine, `tools/linux-run tools/buck2 test` failed at the first
  link: the prelude's system C++ toolchain calls `clang++`, which the image
  lacked. Diff 2 installs `clang` and `lld`. The plan 0016 stacks were
  already landed, so the fix is a new diff here rather than an amend. After
  it, all 21 Buck2 test targets pass in the container on aarch64 Linux
  (Docker Desktop, 12 CPUs, 8 GB, kernel 6.12 linuxkit).
- **Mount types in the container:** the checkout is a virtiofs share from
  macOS, `/tmp` is overlayfs, and the `/bench` volume is ext4 on the VM's
  disk.
- **The crash model cannot run over a file store**, because it uses the
  in-memory fault store. Diff 3 instead checks the real tree over a direct
  store (`a_tree_over_a_direct_store_reopens_at_its_commit`: 20,000 writes
  with checkpoints, an uncommitted tail, reopened in both modes).
- **The intermittent test failure from plan 0021 was the writer-latency
  test.** Under the parallel load of a full `buck2 test` run in the
  container, the slowest of 204 puts took 62 ms against a 1.19 s
  checkpoint, a factor of 19 where the test demanded 20. A writer stalled
  on a lock held for the whole checkpoint would take about the full
  checkpoint, so the bound is now a factor of 4; six full container runs
  then passed.

## Agent notes

- **Entry points:** `crates/bf-tree-store/src/file.rs` (`read_at`,
  `write_at`, `open`), `crates/bf-tree-page/src/read.rs` (`search`,
  `compare_suffix`), `crates/bf-tree-page/src/fence.rs` (`check_key`),
  `crates/bf-tree-page/src/key.rs` (`KeyRef::cmp_bytes`),
  `crates/bf-tree-inner/src/read.rs` (`route`), the descent and eviction code
  from 0014 and 0016.
- **Upstream reference:** `src/fs/` (file backends) and `src/nodes/leaf_node.rs`
  (lookup) at `f442e6172bf7e5483ff320d8e3ac16a70e31bbd6`.
- **Gotchas:**
  - `O_DIRECT` fails at open on `tmpfs`; `/tmp` is `tmpfs` on many
    distributions.
  - With `O_DIRECT`, a short read at end of file returns fewer bytes; use
    `read_exact_at` and map `UnexpectedEof`.
  - Keep `debug_assert!` fence checks in routed paths; debug test runs then
    still catch routing bugs.
  - Run before/after comparisons in one session on one machine, alternating
    builds, and report ranges over several runs.
  - Profile on Linux with `perf record -g`; macOS `sample` profiles show
    lock waits that Linux does not have.

## Sources

- [BF-Tree paper](../references.md#bf-tree-paper), §3.5 (key previews and
  prefix compression), §6 (evaluation on Linux and NVMe; §6.1 disables
  logging, so its numbers are not durability-enabled results).
- [Microsoft implementation](../references.md#microsoft-bf-tree-implementation)
  at `f442e6172bf7e5483ff320d8e3ac16a70e31bbd6`:
  [`src/nodes/leaf_node.rs`](https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/nodes/leaf_node.rs),
  [`src/fs/mod.rs`](https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/fs/mod.rs).
- Linux `open(2)`, section "O_DIRECT": alignment rules and the absence of a
  durability guarantee without `fsync`.
