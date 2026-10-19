# References

## BF-Tree paper

**Bf-Tree: A Modern Read-Write-Optimized Concurrent Larger-Than-Memory Range Index**
by Xiangpeng Hao and Badrish Chandramouli. Proceedings of the VLDB Endowment,
volume 17, number 11, pages 3442–3455, 2024.

- [Published paper (PDF)](https://www.vldb.org/pvldb/vol17/p3442-hao.pdf)
- **Use:** Primary research reference for the tree design, mini-pages,
  record-level caching, buffered writes, and buffer-pool management.
- **Reading guidance:** Consult the relevant algorithms and design rationale
  when planning each component. Evaluate performance claims against the paper's
  workloads and hardware assumptions.

## Maintainer's BF-Tree overview

- [Overview and supporting material](https://github.com/XiangpengHao/bf-tree-docs)
- **Use:** Visual explanations of mini-pages and the circular buffer pool,
  including record caching, buffered writes, growth, promotion, and eviction.
- **Relevant sections:** Mini-pages and Buffer pool for mini-pages.
- Some repository-status notes predate the public Microsoft implementation.

## Microsoft BF-Tree implementation

- [Source repository](https://github.com/microsoft/bf-tree)
- [Design documentation](https://github.com/microsoft/bf-tree/tree/main/doc)
- [Revision inspected during initial architecture planning: `f442e6172bf7`](https://github.com/microsoft/bf-tree/tree/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6)
- At that revision: [source modules](https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/src/lib.rs)
  and [snapshot/recovery documentation](https://github.com/microsoft/bf-tree/blob/f442e6172bf7e5483ff320d8e3ac16a70e31bbd6/doc/snapshot-recovery.md).
- **Use:** Implementation reference for node layouts, mini-page operations,
  concurrency, page storage, write-ahead logging, snapshots, recovery, and tests.
- **Reading guidance:** Compare the code with the paper; the implementation
  evolves beyond the published design. Cite the specific revision when a plan
  or design document relies on implementation details.

## RocksDB overview

- [RocksDB Overview](https://github.com/facebook/rocksdb/wiki/RocksDB-Overview)
- **Use:** Reference for the responsibilities of a complete embedded engine,
  including its database API, LSM-based storage, write-ahead log, and recovery.
- **Relevant sections:** High Level Architecture and Features.

## ZippyDB engineering overview

**How we built a general purpose key value store for Facebook with ZippyDB**
by Sarang Masti. Engineering at Meta, August 6, 2021.

- [Engineering article](https://engineering.fb.com/2021/08/06/core-infra/zippydb/)
- **Use:** Reference for composing an embedded storage engine, replication,
  and shard management into a distributed key-value database. Describes RocksDB,
  Data Shuttle, and ShardManager, including their consistency and durability choices.
- **Relevant sections:** Architecture, Data model, and Consistency.
