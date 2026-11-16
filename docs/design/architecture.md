# Architecture

The goal is a [ZippyDB-like](../references.md#zippydb-engineering-overview)
replicated, sharded key-value database with a storage engine built on our own
[BF-Tree implementation](../references.md#bf-tree-paper).

## Development phases

1. **Embedded engine:** Build a RocksDB-like local key-value engine using BF-Tree.
2. **Replication:** Replicate one shard containing the entire database across nodes.
3. **Sharding:** Partition the database into multiple replicated shards while
    preserving one logical database for applications.

The embedded engine starts with [shared BF-Tree pages](bf-tree-pages.md), the
packed record-storage primitive for base leaves and mini-pages.
Routing uses [packed inner nodes](bf-tree-inner-nodes.md), a separate format
with variable-length separators and typed child references. The
[tree structure](bf-tree-structure.md) composes them, and
[leaf storage](bf-tree-leaf-store.md) gives leaves a durable home behind a
page-store boundary.

The tree stays log-agnostic: it persists no log. For the engine
(plans 0020 to 0022), it exposes `Tree::apply`, `put_logged`, and
`remove_logged` for sequenced writes, `checkpoint_with` for a pre-commit
hook that syncs the log, and `write_snapshot` for a copy that a follower
can install. The [persistence design](bf-tree-persistence.md#logs-and-sequenced-writes)
records the contract. The engine's [write-ahead log](engine.md)
(plan 0020) meets it.

`engine::Db` (plan 0021) is the application-facing API of the embedded
engine: open with recovery, durable writes, atomic write batches, reads,
background checkpoints that truncate the WAL, and close. The
[engine design](engine.md#the-db) records its guarantees.

Phase 2 starts with a network server. `peppydb serve` (plan 0026) puts one
`engine::Db` behind the `peppydb.kv.v1` gRPC API; the
[server design](server.md) records its semantics and guarantees. The
[server roadmap](../plans/0025-server-roadmap.md) continues from there to
Raft replication with openraft, and records the expected shape of phase 3:
μshards mapped to p-shards, one Raft group per server, and a placement
manager on its own etcd cluster.

## Code layout

The repository is one [Buck2](build-system.md) build for Rust and Go.

Directories are organized by role
([build system](build-system.md#layout-by-role-not-by-language)):
`bin/` holds every executable, `lib/` every library, and `proto/` every
API with its generated code, whatever the language.

| Component | Directories | Role |
| --- | --- | --- |
| BF-Tree | `lib/bf-tree` (entry), `lib/bf-tree-*` (internal) | node identifiers, the shared page format, inner nodes, mini-pages, the circular buffer, the page store, and the tree that composes them |
| Engine | `lib/engine` (entry, `PUBLIC`), `lib/engine-wal` (internal) | `Db`, the application-facing [embedded database](engine.md#the-db): recovery, durable writes, batches, reads, and the checkpoint policy; and the [write-ahead log](engine.md): segment files, group commit, replay, and truncation |
| Server | `lib/server` (entry), `bin/peppydb` | the [gRPC server](server.md): the `peppydb.kv.v1` services over one `engine::Db`, the stored key layout, the blocking bridge, shutdown, fail-stop, and the `peppydb` command line; `bin/peppydb` only calls `server::cli` |
| KV API | `proto/kv/v1` (`kv.proto`, `rust/` crate `kv-proto`, `go/` package `kvv1`) | the `peppydb.kv.v1` gRPC API and its generated code |
| Load generator | `bin/peppydb-load`, `lib/load` (Go) | drives a running server and reports throughput and latency |

Later: `lib/server-raft*` (Rust, part of the server component; plan 0028),
`proto/raft/v1`, `bin/peppydb-gateway` and
`bin/peppydb-placement` (Go).

Each component exposes one entry crate. Buck2 visibility rules stop other
components from depending on internal crates, such as `bf-tree-page`.
