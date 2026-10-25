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
