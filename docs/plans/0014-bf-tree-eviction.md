# 0014: Eviction and the second chance

**Status:** Draft

**Design references:** [Circular buffer](../design/bf-tree-circular-buffer.md),
[concurrency](../design/bf-tree-concurrency.md),
[mini-pages](../design/bf-tree-mini-page.md),
[tree structure](../design/bf-tree-structure.md),
[leaf storage](../design/bf-tree-leaf-store.md).

**Parent roadmap:** [BF-Tree implementation](0002-bf-tree-roadmap.md),
milestone 6 (completes it).

**Builds on:** [Mini-page component](0011-bf-tree-mini-page.md),
[circular buffer component](0010-bf-tree-circular-buffer.md),
[shared tree foundation](0012-bf-tree-shared-tree.md),
[mini-page tree integration](0013-bf-tree-mini-tree.md).

## Goal and scope

After plan 0013, every write buffers into a mini-page that lives in the
circular buffer, and the circular buffer has a fixed byte budget. When the
budget is spent, the one allocation site returns
`TreeError::CircularBufferFull` to the caller. The tree is therefore a
bounded write buffer that stops working when it is full.

This stack makes the budget a cache boundary, not a correctness boundary.
A writer that finds the circular buffer full releases its leaf lock, evicts
the oldest blocks with no lock held, and retries. Eviction merges a victim
mini-page into its base page, splitting the leaf when the merged records do
not fit. Blocks that are accessed while they are old get a second chance:
they are copied to the tail of the circular buffer instead of being evicted. This
completes the paper's §4 circular buffer behavior over the
`CircularBuffer` of plan 0010.

- **In scope:** `Tree::make_room`, `Tree::evict_one`, and the eviction of
  a claimed victim through plan 0013's `merge_leaf`; the write retry loop;
  removal of `TreeError::CircularBufferFull`; `LeafEntry::version`;
  relocation of aging blocks on writes; the `Hint` type and
  `Tree::maintain` with the `Relocate` variant for reads and scans;
  `TreeConfig::second_chance_percent`; budget and durability model tests
  with tiny circular buffers; concurrent eviction tests; design-document updates.
- **Out of scope:** `Full` residency, write-back of full pages, cache fills,
  and promotion (plan 0015, which adds `Hint` variants and a `Full` arm to
  eviction and relocation); version-locked inner frames (plan 0016);
  durable store format and checkpoints (plans 0017 and 0018).
- **Done when:** no public operation returns a circular-buffer error under
  a validated configuration; with circular buffers of two to eight leaf
  pages, the model agrees with its oracle after every step,
  `circular_buffer.used()` never exceeds the capacity, and the store after
  `flush` and reopen equals the oracle; a block accessed in the
  second-chance region moves to the tail while untouched blocks are evicted
  in FIFO order; concurrent writers, readers, and evictors on tiny circular
  buffers pass the concurrent model; workspace checks pass.

## Approach

### Terms

- **Victim:** a circular-buffer block that `CircularBuffer::claim_victim`
  has marked `Evicting` on behalf of one evictor. Only that evictor may
  `retire` or `unclaim` it. A `Victim` grants no access to the block's
  bytes; an evictor reaches them only through the owner leaf's entry under
  the leaf lock.
- **Owner:** the leaf whose `LeafEntry` holds a block, recorded in the
  circular buffer at allocation time.
- **Second-chance region:** the oldest part of the circular buffer, near
  the head. `CircularBuffer::in_second_chance(addr)` reports whether a
  block is in it.
- **Relocation:** a copy of a block, within the circular buffer's slab, to a freshly
  allocated block at the tail, followed by `free` of the old block. The old
  block is in the second-chance region, so `free` makes it a tombstone.
- **Budget:** plan 0010's physical budget, `used = tail - head <=
  capacity`. Dead blocks (freed holes, tombstones, and evicted blocks) keep
  their space until the head advances past them.

### The write loop

A writer never evicts while it holds a leaf lock. Eviction must lock the
victim's owner, and the victim can be the writer's own leaf, or the leaf of
another writer that is evicting for its own allocation. Holding one leaf
while waiting for another would allow both a self-deadlock and an
ABBA deadlock. The rule from plan 0012's lock order therefore stays:
eviction runs with no leaf lock held and then locks exactly one leaf.

```rust
/// The result of one attempt to apply a write under the leaf lock.
enum Attempt {
    Done,
    /// The circular buffer could not supply a block. Every guard of the
    /// attempt has been dropped; the caller must make room and route again.
    NeedRoom,
}

pub(crate) fn write(&self, key: &[u8], state: RecordState<'_>) -> Result<(), TreeError> {
    loop {
        match self.write_attempt(key, state)? {
            Attempt::Done => return Ok(()),
            Attempt::NeedRoom => self.make_room(self.leaves.classes().largest())?,
        }
    }
}
```

`write_attempt` is plan 0013's write path. The one place that mapped
`BufferError::Full` to `TreeError::CircularBufferFull` now returns
`Attempt::NeedRoom`. Plan 0011's `buffer` leaves the leaf unchanged on
`BufferError::Full`, so nothing needs to be undone. If the failure happens while
buffering the pending record after a merge, the merge is already complete
and written through; the retry routes again and buffers into whichever
half now owns the key.

### Making room

```rust
/// What one call to `Tree::evict_one` achieved.
enum Eviction {
    Evicted,
    /// Every live block is claimed by another evictor.
    Busy,
    /// The circular buffer holds no live block.
    Empty,
}

fn make_room(&self, need: usize) -> Result<(), TreeError> {
    while !self.leaves.circular_buffer().has_room(need) {
        match self.evict_one()? {
            Eviction::Evicted => {}
            Eviction::Busy => std::thread::yield_now(),
            Eviction::Empty => break,
        }
    }
    Ok(())
}
```

- The target is the largest mini-page class `M`, not the size that failed.
  Any allocation that `buffer` makes is at most `M`, so the retry succeeds
  unless another thread takes the room first. `has_room` also accounts for
  the dead filler block that an allocation needs when it would cross the
  end of the slab. Evicting until `M` bytes are free also batches
  evictions, so a writer does not evict once per small allocation.
  Upstream reaches the same effect with a fixed target of 1024 bytes per
  eviction round (`TARGET_EVICT_SIZE` in `src/tree.rs`).
- Room appears only when the head advances. Eviction at the head retires
  the block and moves the head past it and past any dead blocks behind it.
  An eviction that is not at the head makes no room at once: the evicted
  block keeps its space until the head passes it.
- `Busy` means other threads are evicting every live block. Waiting for
  them is better than restarting the write and contending for the same
  claims. The loop yields and checks `has_room` again, because a
  retirement by another thread may already have advanced the head far
  enough.
- `Empty` means the circular buffer has no live block to evict. Plan
  0010's configuration rule (capacity at least twice the largest class
  plus its 16-byte charge; plan 0013 makes that class the leaf page size
  `L`) then guarantees that any allocation fits, including a possible wrap
  filler, so the caller retries. This assumes that `claim_victim` advances
  the head past the dead blocks it passes, so `Empty` implies that the head
  has reached the tail and no budget is charged.
- Because the budget is physical, a claim held by a slow evictor at the
  head blocks room for every thread until that evictor retires or
  unclaims the block. Under pressure, the other threads then evict live
  blocks behind the slow claim without gaining room, until every live
  block is claimed and they see `Busy`. This over-eviction is bounded by
  the number of live blocks. See the decision on stalled heads.

Liveness: each `Evicted` round evicts one live block, and the number of
live blocks is finite. If a slow claim holds the head, the other threads
eventually see `Busy` and wait. The holder of that claim locks at most one
leaf, and no waiting thread holds a leaf lock, so it finishes, retires or
unclaims, and the head advances. A writer can lose its room to another
thread and loop again, but some thread always makes progress. No round
waits on a lock that a waiting thread holds.

### Evicting one victim

```rust
fn evict_one(&self) -> Result<Eviction, TreeError> {
    match self.leaves.circular_buffer().claim_victim() {
        Claim::Victim(victim) => self.evict(victim).map(|()| Eviction::Evicted),
        Claim::Busy => Ok(Eviction::Busy),
        Claim::Empty => Ok(Eviction::Empty),
    }
}

/// Evict a claimed block. The caller holds no leaf lock.
pub(crate) fn evict(&self, victim: Victim) -> Result<(), TreeError> {
    let mut leaf = self.leaves.lock_write(victim.owner)?;
    if !self.leaves.holds(&leaf, &victim) {
        // The owner grew, relocated, or merged the block after the claim
        // and freed it. A freed block stays claimed, so only retire it.
        drop(leaf);
        self.leaves.circular_buffer().retire(victim);
        return Ok(());
    }
        match with_scratch(|scratch| self.merge_leaf(&mut leaf, scratch)) {
        // The merge freed the block; it stays claimed until retired. A
        // split's right-half guard drops here, before the owner's guard.
        Ok(split) => {
            drop(split);
            drop(leaf);
            self.leaves.circular_buffer().retire(victim);
            Ok(())
        }
        // The entry still holds the block and its dirty records.
        Err(error) => {
            drop(leaf);
            self.leaves.circular_buffer().unclaim(victim);
            Err(error)
        }
    }
}
```

`holds` checks the entry's block with `Block::is(&victim)`, which compares
the block's address with the victim's. The dispatch
is on the entry's residency, not on `victim.kind`: plan 0015 adds a `Full`
arm (write back if dirty, then free) without changing this function's
shape. An I/O error during the merge must not leak the claim, because a
leaked claim would hold the head forever; `unclaim` returns the block to
its owner with its records intact.

```mermaid
sequenceDiagram
    participant W as Writer
    participant L as Leaf A (writer's leaf)
    participant P as CircularBuffer
    participant V as Leaf B (victim's owner)
    participant I as Inner nodes
    participant S as Page store
    W->>L: lock X, buffer(record)
    L-->>W: BufferError::Full
    W->>L: unlock (drop every guard)
    loop until has_room(M)
        W->>P: claim_victim()
        P-->>W: Victim { addr, owner: B }
        W->>V: lock X
        alt entry still holds addr
            W->>S: load_base(B) into scratch
                        alt merge fits
                W->>S: write_through(B)
            else needs a split
                W->>S: write_through(B, right)
                W->>I: install separator (write lock; finds the parent by the separator)
            end
            W->>P: free(block) (stays claimed)
        end
        W->>V: unlock
        W->>P: retire(victim)
    end
    W->>L: route again, lock X, buffer(record)
```

### Merging a leaf reached by its ID

Eviction reaches a leaf by its ID, not by routing a key, so it has no
routing path. It needs none. Plan 0013's `merge_leaf` already serves
`flush`, which also locks leaves by ID: it loads the base page into
thread-local scratch once, merges in place or splits into a fresh, locked
right leaf, writes the result through, and frees the block last. A split
calls plan 0012's `install`, which finds the parent by descending with the
separator under the inner write lock. That descent is correct while the
left leaf is X-locked: the separator lies inside the left leaf's range, and
only the holder of the left leaf's lock can change that range. Concurrent
splits of other leaves cannot misdirect it. Eviction therefore calls
`merge_leaf` unchanged and adds no routing rule.

Calling `install` while holding a leaf lock respects the lock order of plan
0012 (leaf, then inner, then store and table internals). No thread waits for
a leaf lock while it holds the inner lock.

**The block is freed last.** The mini-page stays in the entry until the
write-through succeeds, so an I/O error loses no buffered record. Merges
never allocate from the circular buffer: both outputs go to the store and
the leaf becomes `Base`. Eviction can therefore always complete under a
full circular buffer.

**Scratch.** `evict` borrows the thread-local scratch once for
`merge_leaf`. `make_room` is called from `write` after the attempt has
returned, so no scratch borrow is live; `with_scratch` must never nest.

### Second chance: relocation on access

A block near the head is the next victim. If it is accessed there, the
paper gives it a second chance by copying it to the tail (§4.3, §5.5).
Upstream does this at access time (`cache_page_about_to_evict` and
`move_cache_page_to_tail` in `src/mini_page_op.rs`), not at eviction
time, and so does this stack. Eviction stays strictly FIFO.

```rust
/// Copy the locked leaf's block to the tail if it has aged into the
/// second-chance region. Best effort: never evicts.
fn relocate(&self, leaf: &mut LeafWrite<'_>) {
    let Some(block) = leaf.block() else { return };
    if !self.circular_buffer.in_second_chance(block.addr()) {
        return;
    }
    // The block's bytes are exactly its size class (plan 0011), so the
    // copy keeps the same class.
    let size = block.as_ref().len();
    let Ok(fresh) = self.circular_buffer.alloc(size, Kind::Mini, leaf.id()) else {
        return;
    };
    // A second chance for the page, not for its cold clean records.
    let fresh = leaf.mini().copy_into_block(fresh, /* discard_cold_clean */ true);
    let old = leaf.replace_mini(fresh);
    leaf.bump_version();
    // The old block is in the second-chance region, so it becomes a
    // tombstone. Its space returns when the head passes it.
    self.circular_buffer.free(old);
}
```

The copy stays inside the slab: `alloc` reserves and zeroes a range at the
tail, `copy_into_block` writes the records through the new block's
`AsMut<[u8]>`, and `free` turns the old range into a tombstone. There is
no per-block heap allocation or deallocation. Until the head passes the
tombstone, a relocation charges the budget for both copies.

- **Writes** relocate under their X lock **after** buffering. A write that
  grew the mini-page has already moved it to the tail, so relocating first
  would copy twice.
- **Reads and scans** hold only an S lock, which cannot change the entry.
  They return a hint instead:

```rust
/// Cache maintenance that a read under the shared lock found worth doing.
/// Plan 0015 adds `Cache` and `Promote`.
pub(crate) enum Hint {
    /// The block has aged into the second-chance region.
    Relocate,
}

/// Apply a hint after the shared lock is released: try the exclusive lock
/// once, and apply the hint only if the leaf is unchanged since `seen`.
pub(crate) fn maintain(&self, leaf: LeafId, seen: u32, hint: Hint) -> Result<(), TreeError> {
    let Some(mut guard) = self.leaves.try_lock_write(leaf)? else {
        return Ok(());
    };
    if guard.version() != seen {
        return Ok(());
    }
    match hint {
        Hint::Relocate => self.relocate(&mut guard),
    }
    Ok(())
}
```

The leaf-level read returns `(R, Option<Hint>)` and the version it saw.
`get_with` and each leaf of `scan_with` call `maintain` after they release
the S lock. A reader never waits for a hint: `try_lock_write` fails under
contention and the hint is skipped. Under sustained contention a hot block
may never be relocated, and FIFO eviction then merges it. That is the
correct failure mode: maintenance is best effort, as in upstream, whose
`try_upgrade` succeeds only for a sole reader.

This stack adds `LeafEntry::version: u32`, its first reader. It changes
(wrapping) on every block change (allocation, growth, relocation, merge)
and every write-through; the bumps sit in the `LeafWrite` methods that make
those changes, so later plans inherit them. A relocation hint is skipped
when the version changed because the block it named may be gone. Plan
0015's hints use the same check.

Before 0015, mini-pages hold only dirty records, so relocation drops
nothing yet. The `discard_cold_clean` argument is already `true` so that
plan 0015 needs no change here.

### Configuration

```rust
pub struct TreeConfig {
    // ...
    /// Share of the circular buffer, in percent, nearest the head in which
    /// an accessed block is copied to the tail instead of being left for
    /// eviction.
    /// 0 disables relocation. Default 10.
    pub second_chance_percent: u8,
}
```

`validate` rejects values above 100. Before this stack, plan 0013 builds
the circular buffer with an empty second-chance region, which is the
special case `second_chance_percent = 0`: nothing is in the region, so `free` never
tombstones for the region and nothing relocates. The default of 10 follows
upstream (`cb_copy_on_access_ratio = 0.1` in `src/config.rs`); the paper
discusses the region in §4.1. The value is a process setting and is not
stored in the superblock.

## Decisions and trade-offs

- **Accept over-eviction behind a stalled head, as upstream does.** With
  the physical budget of plan 0010, a slow evictor that holds the claim at
  the head blocks room for every thread. The other threads evict live
  blocks behind it without gaining room, until they see `Busy` (see
  [Making room](#making-room)). The protocol stays live; the cost is extra
  evictions, and only while a head eviction is stalled. Upstream has the
  same behaviour: its `evict_one` reserves the block at an `evicting_addr`
  cursor, evicts it without the lock, and advances the head only over
  `Evicted` blocks. Each `evict_from_circular_buffer` call processes about
  `TARGET_EVICT_SIZE` (1024) bytes, and the writer retries in a loop, so
  every retry behind a stalled head evicts about another 1 KiB
  (`src/circular_buffer/mod.rs` `evict_one`,
  `try_bump_head_address_to_evicting_addr`; `src/tree.rs`
  `evict_from_circular_buffer`). Rejected for now: counting room that
  in-flight evictions will free (`has_room_after_claims`) and waiting on a
  `Condvar` for the head to move. It adds no protocol change, so plan 0023
  can add it if `bench_head_stall` shows a problem.
- **Release, evict, retry from routing.** The alternative, evicting under
  the writer's lock, deadlocks when the victim is the writer's own leaf or
  another evicting writer's leaf. Retrying from routing costs a second
  descent on the slow path only.
- **Evict until the largest class fits.** Batches evictions and makes the
  retry's allocation succeed whatever class it needs. A fixed byte target
  (upstream's 1024) would be a second tuning knob with no clear benefit.
- **Lock the owner by ID and reuse `merge_leaf`.** The victim names its
  owner, so no key is routed before the lock, and the base page is read
  once. Upstream instead reads a key of the victim without a lock,
  re-descends, and re-checks the location under X.
- **Unclaim on error.** A failed merge leaves the block claimed and the head
  stuck. Returning the claim keeps the circular buffer usable and the
  records intact.
- **Relocation after the write, best effort, no eviction.** A relocation
  that evicted could merge other leaves to save one block, which inverts
  the priority. The copy keeps the same size class.
- **Hints for reads.** A read under S cannot change the entry. Two-phase
  maintenance with a version check keeps readers from blocking each other.
  An upgradable lock would serialize readers of one leaf.

## Extension points

- **Plan 0015** adds `Hint::Cache` and `Hint::Promote`, the `Full` arms of
  `evict` (write back if dirty, free, retire) and `relocate` (copy the
  page bytes, keep `dirty`), the "no dirty record" shortcut in
  `merge_leaf` (free without reading the base), and a deferred promotion
  that calls `evict` for `Full` victims and `CircularBuffer::unclaim` for
  others.
  It must not need to change `make_room`, `evict_one`, the `Attempt` loop,
  or `maintain`'s version check.
- **Plan 0016** replaces the inner `RwLock` and `inner_version` behind
  routing and `install` with frames. `merge_leaf` and `evict` stay
  unchanged.
- **Plans 0017 and 0018** change only `write_through` and `load_base`
  internals. Because the write loop restarts from routing after
  `NeedRoom`, any per-attempt work in `write_attempt` runs again on a retry;
  plan 0018's sequence closure must tolerate that (a drawn sequence that
  names no applied write is harmless).
- **Plan 0023** measures the circular buffer's free lists and the
  circular-buffer mutex under eviction pressure. It must not need to change
  the eviction protocol.

## Proposed commit stack

Five diffs. Eviction reuses plan 0013's `merge_leaf`, so the eviction diff
contains only the new protocol.

### `[1/5][bf-evict][plan] plan eviction and the second chance`

- **Purpose:** Agree on the eviction protocol, the lock rules, relocation,
  and the stack.
- **Core changes:** This plan; link it from milestone 6 of the roadmap.
- **Verification:** Review.

### `[2/5][bf-evict][evict] evict instead of failing with CircularBufferFull`

- **Purpose:** Make the budget a cache boundary.
- **Core changes:** `Eviction`, `evict_one`, `evict`, `make_room`,
  `Attempt` and the retry loop; remove `TreeError::CircularBufferFull` and
  its mapping. About 300 lines including tests.
- **Verification:** With a two-page circular buffer, thousands of writes
  succeed and `used() <= capacity` after each; a test claims a victim, then
  grows its block through a write, then calls `evict` with the stale claim,
  which retires without merging; an eviction whose merge splits grows the tree;
  an injected store error during eviction unclaims and leaves the records
  readable; a writer whose own leaf is the oldest block evicts it and
  succeeds.

### `[3/5][bf-evict][second-chance] relocate aging blocks on access`

- **Purpose:** Keep accessed blocks out of FIFO eviction.
- **Core changes:** `TreeConfig::second_chance_percent` and its validation;
  circular-buffer construction from it; `LeafEntry::version` and its
  bumps; `relocate`; relocation after writes; `Hint` with `Relocate`; `maintain`; `get_with` and `scan_with` return and apply
  hints. About 280 lines.
- **Verification:** A hot leaf's logical block address increases under
  pressure while untouched leaves are evicted in FIFO order; a read hint is
  skipped when another S guard is held (`try_lock_write` fails) and when the
  version changed; relocation never evicts (circular-buffer metrics
  unchanged except the copy and the old block's tombstone); `0` disables
  relocation and `100` relocates on every access, both agreeing with the
  oracle.

### `[4/5][bf-evict][model] verify the budget and durability under eviction`

- **Purpose:** Extend the oracles to tiny circular buffers and concurrent
  eviction.
- **Core changes:** The single-threaded model runs with circular buffers of
  two to eight leaf pages and asserts the budget after every step and the
  oracle after `flush` and reopen; the concurrent model (plan 0012) runs
  with tiny circular buffers, so writers, readers, scans, `flush`, and evictors race on the same
  leaves; a test where every live block is claimed makes a writer see
  `Busy` until the claims retire, under a watchdog.
- **Verification:** Both models on `MemStore` and `FileStore`; repeated
  runs; `check_invariants` at quiescence.

### `[5/5][bf-evict][docs] record eviction in the design documents`

- **Purpose:** Make the enduring rules discoverable.
- **Core changes:** `bf-tree-circular-buffer.md`: eviction, `make_room`, the
  second chance, configuration, guarantees. `bf-tree-concurrency.md`:
  eviction locks one leaf with none held, routing under a held leaf lock,
  hints and `maintain`, the lock-order table row. Roadmap: milestone 6
  complete. This plan: outcomes.
- **Verification:** Documents agree with the code and tests; workspace
  checks.

## Open questions

- **Region bounds.** Should `validate` cap `second_chance_percent` below
  100? Values near 100 relocate on almost every access and make `free`
  tombstone almost every block. Recommendation: accept 0 to 100, document
  the cost, and let plan 0023 measure.
- **Relocation into a smaller class.** A relocation that drops many cold
  clean records (after plan 0015) could copy into a smaller class.
  Recommendation: keep the same class now; revisit with measurements.

## Verification strategy

| Check | Command or procedure | Expected result |
| --- | --- | --- |
| No circular-buffer error | Writes and reads with circular buffers of 2 to 8 leaf pages | No `TreeError` from the circular buffer; `CircularBufferFull` no longer exists |
| Budget | `used() <= capacity` after every model step and at every circular-buffer operation (plan 0010's debug assertion) | Never exceeded |
| Durability | Model with tiny circular buffers; `flush`; reopen; compare with the oracle | Equal; no buffered record lost |
| Stale claims | Claim, then grow or relocate or merge the block, then `evict` | Retired without a merge; data intact |
| Split on eviction | A victim whose merge does not fit | Height grows; invariants hold |
| I/O errors | A store wrapper that fails a write during eviction | Claim returned; records still readable |
| Second chance | Hot leaf under pressure | Address moves to the tail; cold leaves evicted FIFO |
| Hints | Contended and stale `Relocate` hints | Skipped; reader never blocks |
| Concurrency | Concurrent model with tiny circular buffers, `MemStore` and `FileStore`, watchdog | Oracle agreement; no deadlock; invariants at quiescence |
| Focused tests | `tools/buck2 run //crates/bf-tree:bf-tree-unittest -- evict::` | Pass |
| Workspace | `tools/fmt.sh`; `tools/clippy.sh`; `tools/buck2 test //crates/... //bin/... //go/...`; `tools/autocargo --check`; `tools/gobuckify-remote --check`; `cargo test --workspace --locked`; `(cd go && go vet ./... && go test ./...)` | Clean |

## Progress

- [ ] Agree on the design and commit stack with the user.
- [ ] Diff 2: eviction and the retry loop; `CircularBufferFull` removed.
- [ ] Diff 3: `LeafEntry::version`, relocation, `Hint`, and `maintain`.
- [ ] Diff 4: budget, durability, and concurrent models.
- [ ] Diff 5: design documents and roadmap.
- [ ] Stack-wide verification.

## Discoveries and plan changes

None yet.

## Agent notes

- **Entry points:** `crates/bf-tree/src/write.rs` (the write path and the
  `CircularBufferFull` mapping site), `crates/bf-tree/src/leaf_map.rs`
  (`LeafEntry`, `write_through`, `load_base`, the `LeafMap::buffer`
  allocation site),
  `crates/bf-tree/src/read.rs` and `scan.rs` (leaf reads that will return
  hints), `crates/bf-tree/src/lib.rs` (`TreeConfig`, `TreeError`),
  `crates/bf-tree/src/tests/model.rs` (oracle harness),
  `crates/bf-tree-circular-buffer/src/` (`claim_victim`, `unclaim`,
  `retire`, `in_second_chance`, `has_room`).
- **Upstream at `f442e6172bf7e5483ff320d8e3ac16a70e31bbd6`:** `src/tree.rs`
  (`evict_from_circular_buffer` "make sure you're not holding any lock",
  `TARGET_EVICT_SIZE`, `eviction_callback`, which re-descends by a key of
  the victim and re-checks the location under X); `src/circular_buffer/mod.rs`
  (`evict_n`, `ptr_is_copy_on_access`, `get_fuzzy_tail_addr`);
  `src/mini_page_op.rs` (`cache_page_about_to_evict`,
  `move_cache_page_to_tail`, `try_merge_mini_page`); `src/storage.rs`
  (`move_mini_page_to_tail`, which copies with `discard_cold_cache`).
- **Gotchas:**
  - Never call `make_room` or `evict` with a leaf guard alive. Structure the
    write so that `write_attempt` returns before eviction starts; a guard
    held in the loop's scope is a deadlock waiting for a test.
  - Never nest `with_scratch`. Eviction and `maintain` borrow it from the
    top.
  - A freed block that is `Evicting` stays claimed (plan 0010). `evict`
    must still `retire` it when `holds` is false.
  
  - Tests that need a stale claim drive `CircularBuffer::claim_victim` and
    `Tree::evict` directly; keep such helpers crate-private.
    - Every block change must bump the leaf version, including growth inside
    `buffer`, or a stale `Relocate` hint can pass the check.

## Sources

- [BF-Tree paper](../references.md#bf-tree-paper): §4 (circular buffer,
  regions, allocation and eviction; §4.3 concurrent evictions that finish
  in order), §5.5 (copy to the tail on access, without eviction).
- Upstream at revision `f442e6172bf7e5483ff320d8e3ac16a70e31bbd6`:
  [`src/tree.rs`](https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/tree.rs),
  [`src/circular_buffer/mod.rs`](https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/circular_buffer/mod.rs),
  [`src/mini_page_op.rs`](https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/mini_page_op.rs),
  [`src/storage.rs`](https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/storage.rs),
  [`src/config.rs`](https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/config.rs).
  peppydb differs by claiming one block at a time from the circular buffer
  instead of reserving a range, by locking the victim's owner by ID
  instead of re-checking after an unlocked key read, and by placing the
  pending record after a merge instead of restarting.
