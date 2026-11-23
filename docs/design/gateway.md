# Gateway and linearizability harness

This document records the design of the Go gateway component (`lib/gateway`,
`lib/gateway-kvclient`, `bin/peppydb-gateway`) and the Porcupine-based
linearizability harness (`lib/lincheck`). [Plan 0033](../plans/0033-server-gateway.md)
built them as milestone S3 of the [server roadmap](../plans/0025-server-roadmap.md).

Related documents: [architecture](architecture.md) (phases and code layout),
[server](server.md) (`peppydb.kv.v1`, `LeaderHint`, `AdminService.Status`),
[server Raft](server-raft.md) (`ProposeError::NotLeader` vs `OutcomeUnknown`),
[build system](build-system.md) (Go packages, visibility, `gobuckify-remote`).

## Components and boundaries

```mermaid
flowchart LR
    client["gRPC client"] -->|"peppydb.kv.v1<br/>grpc.health.v1"| gw
    subgraph gw["bin/peppydb-gateway"]
        srv["lib/gateway.Server<br/>(KvService + Health + /metrics)"] --> router["lib/gateway.Router<br/>(phase 2: StaticRouter)"]
        router --> kvc["lib/gateway-kvclient.Client<br/>(peer conns + leader cache)"]
    end
    kvc -->|"KvService + Admin.Status"| nodes["peppydb serve (Rust)"]
```

| Path | Role | Visible to |
| --- | --- | --- |
| `lib/gateway-kvclient` (`package kvclient`) | Internal Raft-group client: persistent peer connections, `LeaderDiscovery`, `SetLeader`, `LeaderHint` decoding, and linearizability-safe retry rules | `//lib/gateway:` |
| `lib/gateway` (`package gateway`) | Entry package: `Router`, `StaticRouter`, `Server` (`KvService` + `grpc.health.v1.Health`), and Prometheus `/metrics` | `//bin/peppydb-gateway:` |
| `bin/peppydb-gateway` | Thin CLI wrapper (`--listen`, `--backends`, `--metrics-listen`, timeouts, graceful shutdown) | `//lib/lincheck:` (for Buck2 tests) |
| `lib/lincheck` (`package lincheck`) | Reusable Porcupine single-key KV model, unknown-outcome recorder, concurrent workload runner, and multi-process Rust/Go cluster harness | `PUBLIC` |

The gateway is purely data-plane: it serves `peppydb.kv.v1.KvService` and
`grpc.health.v1.Health`. Node-local `AdminService` RPCs (`Status`,
`Checkpoint`, `Snapshot`) remain direct-to-node and move to the phase 3
placement/control plane.

## Leader discovery and `LeaderHint` wire format

`kvclient.Client` tracks one Raft group's leader through two mechanisms:

1. **Pluggable `LeaderDiscovery` + `SetLeader`:** When the cached leader
   address is empty or invalidated, `Client` runs a coalesced `Discover(ctx)`
   call (concurrent callers share one in-flight probe). In phase 2, the default
   discovery queries `AdminService.Status` across known peers. In phase 3, a
   placement-driver watcher can supply the discovery source and push updates
   via `Client.SetLeader(addr)`.
2. **Immediate `LeaderHint` self-correction:** Even with an etcd-backed
   placement cache in phase 3, leader leases lag Raft elections slightly.
   Non-leader `peppydb` nodes attach `peppydb.kv.v1.LeaderHint` inside a
   standard `google.rpc.Status` envelope (`google.protobuf.Any` with type URL
   `type.googleapis.com/peppydb.kv.v1.LeaderHint`) in `grpc-status-details-bin`.
   `kvclient.ExtractLeaderHint` decodes `*kvv1.LeaderHint` through standard
   `status.FromError(err).Details()`.

## Retry and linearizability guarantees

`peppydb` separates safe non-leader rejections (`ProposeError::NotLeader(hint)`,
where openraft never appended an entry) from unknown outcomes
(`ProposeError::OutcomeUnknown`, transport resets, or `SIGKILL`).
`kvclient.Client` enforces asymmetric retry rules:

| RPC kind | `UNAVAILABLE` with `LeaderHint` | `UNAVAILABLE` without `LeaderHint` | Other status |
| --- | --- | --- | --- |
| Read (`Get`, `Scan`) | Update cached leader and retry (up to `MaxRedirects`) | Invalidate cache, back off, probe, and retry (up to `MaxReadRetries`) | Return immediately |
| Write (`Put`, `Delete`) | Update cached leader and retry (up to `MaxRedirects`) | Invalidate cache, trigger background probe, and return `UNAVAILABLE` | Return immediately |

**Invariant:** The gateway must never retry a write that failed without a
`LeaderHint`. Without client session deduplication (Raft dissertation §6.3),
retrying an unknown-outcome write can apply it a second time after an
intervening concurrent write to the same key, violating linearizability.

## Metrics

When `--metrics-listen <ADDR>` is set, `peppydb-gateway` exposes Prometheus
metrics on `GET /metrics` (404 on other paths) via `prometheus/client_golang`
on an isolated registry:

- Go runtime (`go_*`) and process (`process_*`) collectors;
- `peppydb_gateway_rpc_requests_total{method,code}`;
- `peppydb_gateway_rpc_duration_seconds{method}` (16 buckets from 100 µs to 10 s, matching `peppydb serve`);
- `peppydb_gateway_inflight_requests`;
- `peppydb_gateway_leader_redirects_total`, `peppydb_gateway_read_retries_total`, `peppydb_gateway_leader_probes_total`;
- `peppydb_gateway_process_uptime_seconds`.

Buck2 Go targets that link `prometheus/client_golang` set `cgo_enabled = False`
so Buck2 builds match `tools/gobuckify-remote`'s `CGO_ENABLED=0` dependency
graph on macOS (avoiding `prometheus`'s Darwin cgo memory collector `.c` file).

## Linearizability harness (`lib/lincheck`)

`lib/lincheck` checks client-visible linearizability with
[Porcupine](https://github.com/anishathalye/porcupine):

- **Model:** Single-key `Get`, `Put`, and `Delete` partitioned by key.
  Empty values (`Value != nil && len == 0`) are distinguished from absent keys
  (`Value == nil`). Range scans are tested separately because `kv.proto` does
  not guarantee linearizability across storage leaves or scan pages.
- **Unknown-outcome writes:** Failed `Get` RPCs and `INVALID_ARGUMENT` writes
  are omitted. Writes ending in `UNAVAILABLE`, `DEADLINE_EXCEEDED`, `CANCELLED`,
  or `INTERNAL` retain their `Call` timestamp and are normalized to
  `Return = maxCompletedReturn + 1` before calling Porcupine. Porcupine can
  linearize an applied unknown write at any point after `Call`, or place an
  unapplied unknown write after every observation in the history.
- **Cross-language Buck2 execution:** `//lib/lincheck:lincheck-test` injects
  `PEPPYDB_BIN` and `PEPPYDB_GATEWAY_BIN` via Buck2 `$(location ...)`, so
  `tools/buck2 test //lib/lincheck/...` compiles and spawns real Rust `peppydb`
  clusters alongside Go `peppydb-gateway` processes. Under plain `go test ./...`,
  process-spawning tests skip when `PEPPYDB_BIN` is unset while unit and
  negative oracle tests always run.

## GCP benchmarking (`gateway-perf`)

`tools/gcp-bench run --suite gateway-perf` runs a 5-VM topology in one
`us-east1` zone (`# mode: cluster`, `# nodes: 3`, `# gateway: true`): three
`c4d-standard-8-lssd` `peppydb` server VMs (`server-1..3`), one `n4-standard-8`
`peppydb-gateway` VM (`gateway`), and one `n4-standard-8` `peppydb-load` client
VM (`client`). The client drives `$GATEWAY_ADDR` and scrapes `/metrics` from
`gateway` and all three servers after every step, isolating gateway proxy
overhead and Go runtime behavior against direct `cluster-perf` runs
([remote benchmarks](remote-benchmarks.md)).

## Extension points

- **S8 (membership and fault testing):** Imports `lib/lincheck` (`Cluster`,
  `RunWorkload`, `Check`, `WriteVisualization`) to drive randomized `SIGKILL`,
  restart, membership change, and `iptables` partition runs under Porcupine.
- **Phase 3 (sharding):** Replaces `StaticRouter` with a placement-backed
  `Router` (e.g. `lib/gateway-placement`) that maps keys via μshard to p-shard
  and wires each p-shard's `kvclient.Client` to an etcd/placement-driver
  `LeaderDiscovery` + `SetLeader` watcher. `lib/gateway.Server` and
  `lib/gateway-kvclient` do not change shape.
