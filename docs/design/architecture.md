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
records the contract.

## Code layout

The repository is one [Buck2](build-system.md) build for Rust and Go.

| Component | Crates | Role |
| --- | --- | --- |
| BF-Tree | `bf-tree` (entry), `bf-tree-node-id`, `bf-tree-page`, `bf-tree-inner`, `bf-tree-store` | node identifiers, the shared page format, inner nodes, the page store, and the tree that composes them |
| Executable | `bin/peppydb` | thin wiring |
| Go services | `go/` | the planned gateway and placement driver; a hello-world program for now |

Each component exposes one entry crate. Buck2 visibility rules stop other
components from depending on internal crates, such as `bf-tree-page`.
