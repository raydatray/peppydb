# 0001: Phased crate layout

**Status:** Complete

**Design references:** [Architecture](../design/architecture.md).

## Goal and scope

Build a BF-Tree-based key-value system in three phases:

1. An embedded, persistent local database with our own BF-Tree implementation.
2. One logical database stored as a single shard, replicated across multiple nodes.
3. Partition that same logical database into multiple replicated shards, with
   routing and placement.

In phase 2, one shard owns the entire database keyspace and has one replication
group. In phase 3, each shard owns part of that keyspace and has its own
replication group. Applications continue to access one logical database.

This documentation stack records the agreed crate ownership and dependency
direction. Algorithms, storage formats, public APIs, consensus selection, and
executable features need their own implementation plans.

We will use the BF-Tree paper and upstream implementation as references for our
own implementation. RocksDB-like means an embedded ordered key-value engine;
the exact API and feature set remain to be agreed.

Phase 1 delivers the complete local engine: its database API, BF-Tree storage,
caching, file I/O, and durability/recovery machinery. BF-Tree is a B-tree-family
alternative to LSM-based storage.

## Approach

```text
bin/
  peppydb/                 executable and local tools
crates/
  peppydb-bftree/          tree and tree-specific storage mechanisms
  peppydb-engine/          embedded database API and lifecycle
```

The engine and BF-Tree crates together form the embedded database and run in
the caller's process. The engine owns the public API and database lifecycle;
BF-Tree owns the tree's tightly coupled storage mechanisms. Their persistence
contract needs a joint design. The executable remains thin wiring.

The future embedded API could look like this; signatures are illustrative:

```rust
use peppydb_engine::{Db, Options};

let db = Db::open("data", Options::default())?;
db.put(b"key", b"value")?;
let value = db.get(b"key")?;
```

The workspace already contains the executable and BF-Tree crate. Add the engine
when database-level behavior needs it, and later crates as their responsibilities
become concrete. The [architecture document](../design/architecture.md)
summarizes the overall project direction.

### Mutation, logging, and checkpoint boundaries

Phase 1 must keep three responsibilities distinct:

- **Applying mutations:** Update the database state in the BF-Tree.
- **Recording durable history:** Persist the ordered operations needed for recovery.
- **Creating and restoring checkpoints:** Capture and recover a consistent state
  together with the position in mutation history that it represents.

The standalone engine uses a local write-ahead log (WAL). A future replication
layer may supply durable history through a Raft log. Replacing the local WAL
requires correct recovery from a durable checkpoint and subsequent committed
entries, including safe checkpoint publication and log retention.

These are module or API boundaries; they do not require three separate crates.
Their ordering and recovery invariants must be designed together.

## Decisions and trade-offs

The phase definitions, separation of mutation application from durable history
and checkpoints, and architectural directions below are agreed. Detailed APIs
and implementation choices remain deferred to their respective plans.

- Separate the index/storage mechanisms from the embedded database contract.
  This keeps the engine reusable by local applications and replication, at the
  cost of an interface that must be designed carefully around persistence.
- Prefer modules inside the BF-Tree crate while its internals evolve. Separate
  pager or buffer-pool crates would require stable interfaces too early.
- Add networking and distribution incrementally. The phase 1 dependency graph
  stays small, and distributed choices remain open until their plans are written.
- Keep a concrete BF-Tree-backed engine initially. Introduce abstraction traits
  where an actual boundary or test harness needs them.

## Commit stack

### `[1/1][crate-layout][design] document phased crate boundaries`

- **Purpose:** Record the agreed architecture and the three-phase roadmap.
- **Core changes:** Finalize this plan and add `docs/design/architecture.md` with the
  crate responsibilities, dependency graph, and confirmed assumptions. Collect
  the shared background sources in `docs/references.md`.
- **Verification:** Review paths, source links, dependency direction, and the
  distinction between current code, agreed intent, and future proposals.

The first BF-Tree implementation stack will follow once we select its scope.

## Deferred implementation questions

Resolve these questions in the relevant implementation plans as those parts of
the system are developed.

- Which phase 1 guarantees are required initially: atomic batches, stable read
  snapshots, concurrent writes, and which crash-durability modes?
- Should the embedded API start synchronous? This proposal leaves network
  runtime choices outside the local engine; storage I/O strategy needs its own
  evaluation.
- How closely should we follow upstream memory layouts? The current workspace
  forbids unsafe Rust, which constrains possible implementations.
- For later phases, which consensus implementation and partitioning scheme
  should we use? Range partitioning and hash partitioning have different scan
  and balancing trade-offs.

## Verification strategy

Check that the layout fits the existing workspace, the intended dependencies
are acyclic, and the engine has no dependency on distributed crates. Review
local document links, source citations, and consistency with the agreed decisions.

Future Rust stacks must run the checks in `AGENTS.md`. Algorithm plans should
add appropriate model-based tree tests, concurrency checks, recovery fault
injection, or benchmarks when those behaviors are introduced.

### Outcomes

- Reviewed the layout against `Cargo.toml`; new crates remain future work.
- Reviewed the dependency graph: it is acyclic and keeps distribution above the engine.
- Checked local document links, reference anchors, and source citations.
- Confirmed that the plan and design document preserve the agreed phase and
  durability boundaries while keeping detailed implementation choices open.
- Rust checks were not needed for this documentation-only stack.

## Progress

- [x] Agree on the phase definitions and high-level design principles.
- [x] Finalize the architecture document and reference index.
- [x] Review documentation consistency and links.

## Sources and assumptions

Source links and reading notes are collected in [References](../references.md).

- The [RocksDB overview](../references.md#rocksdb-overview) informs the embedded
  engine's product boundary: the database API and storage/recovery machinery
  together form the engine.
- The [BF-Tree paper](../references.md#bf-tree-paper) and
  [maintainer's overview](../references.md#maintainers-bf-tree-overview) motivate
  keeping mini-pages and buffer-pool mechanisms close together. Detailed
  algorithm review belongs in the implementation plans.
- The [Microsoft implementation](../references.md#microsoft-bf-tree-implementation)
  at revision `f442e6172bf7` contains WAL, storage, and snapshot modules within
  the tree crate. Our proposed engine boundary is an adaptation.
- The [ZippyDB overview](../references.md#zippydb-engineering-overview) informs the
  storage, replication, and shard-management boundaries. This does not select
  ZippyDB's Multi-Paxos protocol or its micro-shard model.
