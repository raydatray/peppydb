# 0027: Workspace layout by role, not by language

**Status:** Complete

**Design references:** [Build system](../design/build-system.md),
[architecture](../design/architecture.md#code-layout),
[server](../design/server.md).

## Goal and scope

The repository is organized by language: Rust libraries in `crates/`,
Rust executables in `bin/`, and everything Go in the `go/` module, with
executables in `go/cmd/`, libraries in `go/internal/`, and generated code
in `go/kvpb`. Finding something means knowing its language first, and one
API is spread across `proto/`, `crates/server-proto`, and `go/kvpb`.

Buck2 is the source of truth: it builds both languages, Cargo manifests are
generated, and visibility rules enforce the boundaries. The language split
only mirrors Cargo's and Go's habits. This stack reorganizes the repository
by role, so that one directory shows every executable and one shows every
library, whatever the language, and one directory per API holds the
`.proto` file and its generated code.

The change is mechanical and changes no behavior. It is cheapest now: Go
code is three packages, and the roadmap is about to add Raft crates, a
peer API, a gateway, and a placement manager.

- **In scope:**
  - `bin/` for every executable, `lib/` for every library, `proto/` for
    every API with its generated Rust and Go code;
  - `go.mod` at the repository root (module `peppydb`), with an `ignore`
    list;
  - updating every tool, Buck2 macro, script, and configuration that names
    `crates/`, `go/`, or the old Go import paths;
  - renames: `peppy-load` to `peppydb-load`, `server-proto` to `kv-proto`,
    the Go package `kvpb` to `kvv1`;
  - removing the hello-world placeholders `go/cmd/hello` and
    `go/internal/greet`;
  - updating `AGENTS.md` and the design documents.
- **Out of scope:** code changes beyond paths, imports, and names;
  grouping libraries by component (`storage/`, `server/`); rewriting
  completed plans, which stay as historical records with their old paths.
- **Done when:** the old directories are gone; `tools/buck2 test //...`
  equivalents, `cargo test --workspace`, `go vet ./...` and `go test ./...`
  from the root, gopls, and every check in `AGENTS.md` pass; the generated
  code and lock files are unchanged except for paths and names.

## Approach

### The layout

```text
peppydb/
├── AGENTS.md, README.md, .buckconfig
├── Cargo.toml, Cargo.lock            generated workspace; members under bin/, lib/, proto/
├── go.mod, go.sum                    module peppydb (moved from go/)
├── buf.yaml, buf.gen.yaml
├── rust-toolchain.toml, rustfmt.toml
│
├── bin/                              every executable, any language
│   ├── peppydb/                      Rust   peppydb serve
│   └── peppydb-load/                 Go     load generator (was go/cmd/peppy-load)
│
├── lib/                              every library, any language
│   ├── bf-tree/                      Rust   (and the seven other bf-tree-* crates)
│   ├── engine/                       Rust
│   ├── engine-wal/                   Rust
│   ├── server/                       Rust
│   └── load/                         Go     package peppydb/lib/load (was go/internal/load)
│
├── proto/                            every API, with its generated code
│   └── kv/v1/
│       ├── kv.proto                  protobuf package peppydb.kv.v1
│       ├── rust/                     Rust crate kv-proto (was crates/server-proto)
│       │   ├── BUCK, Cargo.toml
│       │   └── src/lib.rs, src/generated/peppydb.kv.v1.rs
│       └── go/                       Go package kvv1, import peppydb/proto/kv/v1/go (was go/kvpb)
│           ├── BUCK
│           └── kv.pb.go, kv_grpc.pb.go
│
├── tools/                            developer tools, including protogen (unchanged place)
├── build/                            Buck2 macros, toolchains, platforms, modes
├── third-party/rust/, third-party/go/
└── docs/
```

Future work lands in the same places: `bin/peppydb-gateway` and
`bin/peppydb-placement` (Go), `lib/raft`, `lib/raft-log` (Rust), and
`proto/raft/v1/{raft.proto, rust/, go/}`.

**Rules:**

- A directory under `bin/` builds one executable; a directory under `lib/`
  builds one library. Its `BUCK` file says which language.
- Names keep the existing convention: one entry library per component,
  named after it (`engine`), and internal libraries named
  `<component>-<part>` (`engine-wal`). Buck2 `visibility` lists the exact
  dependents, for Go packages as well as Rust crates.
- Executables are named `peppydb` or `peppydb-<role>`.
- An API lives in `proto/<name>/<version>/`, with `rust/` and `go/`
  subdirectories for its generated code. Its Rust crate is `<name>-proto`.
  Its Go package is named `<name><version>` (`kvv1`); `go` is a keyword,
  so the directory name cannot be the package name, and importers use an
  alias.

### Go at the repository root

A Go module covers only the tree below its `go.mod`, so Go packages in
`bin/`, `lib/`, and `proto/` need the module at the root:

```text
module peppydb

go 1.27.0

ignore (
	./buck-out
	./target
	./third-party
)

tool (
	github.com/bufbuild/buf/cmd/buf
	google.golang.org/grpc/cmd/protoc-gen-go-grpc
	google.golang.org/protobuf/cmd/protoc-gen-go
)
```

- Every directory with `.go` files is the package `peppydb/<dir>`. Rust
  directories have no `.go` files, so the `go` command passes over them;
  in `proto/kv/v1/`, the `.proto` file and `rust/` do not affect the `go/`
  package.
- **The `ignore` directive** (Go 1.25 and later) is required: `buck-out/`
  holds unpacked third-party Go sources that `go ./...` would otherwise
  treat as packages of the module. Directories that start with `.` or
  `_`, such as `.sl/`, are skipped already.
- `go build ./...`, `go test ./...`, `go vet ./...`, `go mod tidy`,
  `go tool buf`, and gopls run from the root.
- **Go `internal/` privacy is replaced by Buck2 visibility.** `lib/load`
  was importable only inside `go/`; now any package of the module can
  import it with the `go` command, and Buck2 rejects importers outside its
  `visibility`, as it already does for Rust crates.

### The `proto/` directory

- **The protobuf package stays `peppydb.kv.v1`.** It is part of the wire
  protocol: gRPC routes calls by `/peppydb.kv.v1.KvService/Get`, and type
  URLs and reflection use it. A bare `kv.v1` could collide with another
  package that a client links.
- **The file moves to `proto/kv/v1/kv.proto`,** without the `peppydb/`
  directory level. buf's `PACKAGE_DIRECTORY_MATCH` lint rule requires the
  directory to match the package, so `buf.yaml` disables that one rule:

  ```yaml
  version: v2
  modules:
    - path: proto
  lint:
    use: [STANDARD]
    except:
      # Packages are peppydb.<api>.<version>, files are at
      # proto/<api>/<version>/: the peppydb level is implied by the
      # repository (plan 0027).
      - PACKAGE_DIRECTORY_MATCH
  breaking:
    use: [FILE]
  ```

- **Go output goes to `go/`** through `option go_package =
  "peppydb/proto/kv/v1/go;kvv1"` and the plugins' `module=peppydb`
  option, so buf writes it to `proto/kv/v1/go/`.
- **Rust output goes to `rust/src/generated/`;** `tools/gen-proto` already
  passes the directory to `//tools/protogen`.
- buf ignores `rust/` and `go/`: they contain no `.proto` files.
- Changing `go_package` changes the generated Go code but not the wire
  format, so `buf breaking` passes.

### What changes in code

Only paths, names, and imports:

```rust
// lib/server/src/kv.rs
use kv_proto::kv::v1::kv_service_server::KvService; // was server_proto
```

```go
// bin/peppydb-load/main.go
import (
	"peppydb/lib/load"                // was peppydb/go/internal/load
	kvv1 "peppydb/proto/kv/v1/go"     // was peppydb/go/kvpb
)
```

```text
tools/buck2 run //bin/peppydb -- serve --data-dir /tmp/p
tools/buck2 run //bin/peppydb-load -- --addr 127.0.0.1:7420
tools/buck2 test //lib/...         every library's tests, both languages
```

### Tools and configuration to update

| File | Change |
| --- | --- |
| `tools/autocargo` | Query `//bin/... + //lib/... + //proto/...` instead of `//crates/... + //bin/...`; accept path dependencies from any of them |
| `tools/clippy.sh` | Same target set, plus `//tools/...` |
| `tools/fmt.sh` | rustfmt roots `lib/*/src/lib.rs`, `proto/*/*/rust/src/lib.rs`, `bin/*/src/main.rs`, `tools/*/src/main.rs`; gofmt over the whole repository except the ignored directories; the `package_name` check becomes `peppydb/<dir>` for every `BUCK` with Go targets |
| `tools/check-unsafe` | Search `bin`, `lib`, `proto`; allowlist `lib/bf-tree-circular-buffer` |
| `tools/check-tools` | Read the Go version from the root `go.mod` |
| `tools/gobuckify-remote` | `MODULE_DIR` is the root; list `./...` with the module's `ignore` list |
| `tools/gen-proto` | Go commands at the root; Rust output `proto/kv/v1/rust/src/generated`; Go output `proto/kv/v1/go`; the breaking baseline reads the parent revision's `proto/` (for this one stack, the old `proto/peppydb/` tree; see below) |
| `tools/gopackagesdriver` | `root//bin/... root//lib/... root//proto/...` |
| `tools/gcp/targets.txt` | `//lib/...` labels |
| `tools/linux-run` | Examples in comments |
| `build/defs.bzl` | `test_deps` variant matching on `//lib/` instead of `//crates/` |
| `buf.yaml`, `buf.gen.yaml`, `kv.proto` | As above |
| Every moved `BUCK` | Labels (`//lib/engine:engine`), `package_name`, `visibility` |
| `AGENTS.md` | Paths, Go commands from the root, the verification list |

### Documents

- **Design documents** (`docs/design/*.md`): about 80 references to
  `crates/` and 14 to `go/`. They are rewritten to the new paths, because
  current architecture must be accurate. `build-system.md` gains a section
  on the layout and the root Go module; `architecture.md`'s code layout
  table changes.
- **Completed plans** keep their paths. The layout section of
  `build-system.md` states that plans before 0027 use the old paths
  (`crates/<x>` is now `lib/<x>`), which is enough to follow them.
- **Explanations** are checked for paths and updated if any.

## Decisions and trade-offs

- **Organize by role (`bin/`, `lib/`, `proto/`), not by language.** Buck2
  already makes the language a property of each target. *Alternatives:*
  keep the layout and add Buck2 aliases in `bin/BUCK` for one place to run
  binaries (cheaper, but libraries and APIs stay split by language);
  group by component (`storage/`, `server/`, `gateway/`), which suits a
  larger repository and can follow later.
- **Flat `lib/`.** Simple, and enough for about fifteen libraries. Revisit
  when it gets crowded.
- **One Go module at the root,** with an `ignore` list. *Alternative:* a
  module per directory with `replace` directives, which multiplies
  `go.mod` files.
- **Generated code under each API, one subdirectory per language.** Each
  directory holds one language, and one API is one directory.
  *Alternatives:* everything beside the `.proto` file (Go's usual
  `source_relative` style, but mixes languages in one directory); the
  generated code in `lib/kv-proto` and `lib/kvv1` (splits one API across
  three directories).
- **`proto/kv/v1`, not `proto/peppydb/kv/v1`,** with one lint exception.
  *Alternatives:* keep the extra level (matches the package, one redundant
  directory); rename the package to `kv.v1` (loses the wire namespace).
- **Completed plans keep their old paths.** They are historical records;
  rewriting them would change history for no reader benefit.
- **Plan file names group by area.** Workspace plans take the `workspace-`
  prefix, like the `bf-tree-`, `engine-`, and `server-` plans, so related
  plans sort together. This stack renames `0001-crate-layout.md` and
  `0009-buck2-build.md` accordingly; their old commit titles keep their
  original labels (`crate-layout`, `buck2-build`), because history is not
  rewritten. `0024-gcp-remote-bench.md` keeps its name for now: it is
  benchmark tooling, and its area prefix is undecided.
- **The plan label is `workspace-language-agnostic-layout`,** matching the
  file name, although it makes commit titles long.
- **The trade-off.** The layout departs from Go's `cmd/` and `internal/`
  conventions and Rust's `crates/` habit, so a newcomer must learn it, and
  the root `ignore` list must stay current.

## Proposed commit stack

The moves are split so that each diff builds and passes its checks. Moves
show up in Sapling as renames, so review focuses on the edits.

### `[1/6][workspace-language-agnostic-layout][plan] plan the repository layout by role`

- **Purpose:** This plan. It also renames the two earlier workspace plans
  to the `workspace-` prefix (`0001-workspace-crate-layout.md`,
  `0009-workspace-buck2-build.md`) and updates the links to them, so the
  workspace plans group together like the `bf-tree-`, `engine-`, and
  `server-` plans.
- **Verification:** Review.

### `[2/6][workspace-language-agnostic-layout][rust] move the Rust libraries from crates/ to lib/`

- **Purpose:** The largest move, with only Rust involved.
- **Core changes:** `sl mv crates/<x> lib/<x>` for every crate except
  `server-proto`; `BUCK` labels and visibility; `build/defs.bzl`;
  `tools/autocargo`, `clippy.sh`, `fmt.sh`, `check-unsafe`,
  `gcp/targets.txt`, `linux-run`; `tools/autocargo`; `AGENTS.md` paths.
- **Verification:** `tools/buck2 test //lib/... //bin/...`;
  `cargo test --workspace --locked`; fmt, clippy, `autocargo --check`;
  Miri for `lib/bf-tree-circular-buffer`.

### `[3/6][workspace-language-agnostic-layout][go] move the Go module to the repository root`

- **Purpose:** Go packages can live anywhere.
- **Core changes:** `go/go.mod` and `go.sum` to the root with the `ignore`
  list; `go/cmd/peppy-load` to `bin/peppydb-load`; `go/internal/load` to
  `lib/load`; remove `go/cmd/hello` and `go/internal/greet`; import paths;
  `tools/gobuckify-remote`, `check-tools`, `fmt.sh`, `gopackagesdriver`,
  `gen-proto` (Go commands at the root); `go/kvpb` stays in place for this
  diff and becomes package `peppydb/go/kvpb` of the root module.
- **Verification:** `go vet ./... && go test ./...` from the root;
  `tools/buck2 test //bin/... //lib/... //go/...`;
  `tools/gobuckify-remote --check`; `tools/gen-proto --check`.

### `[4/6][workspace-language-agnostic-layout][proto] move the API and its generated code into proto/kv/v1`

- **Purpose:** One directory per API.
- **Core changes:** `proto/peppydb/kv/v1/kv.proto` to `proto/kv/v1/`;
  `crates/server-proto` to `proto/kv/v1/rust` as crate `kv-proto`;
  `go/kvpb` to `proto/kv/v1/go` as package `kvv1`; `go_package`;
  `buf.yaml` lint exception; `buf.gen.yaml`; `tools/gen-proto` output
  paths; imports in `lib/server` and `bin/peppydb-load`; regenerate.
- **Verification:** `tools/gen-proto --check`, with the breaking check run
  against the old tree (`PROTO_BASE` pointing at the parent revision,
  where the file is at its old path; buf compares by package and type, so
  the move is not breaking); all workspace checks.

### `[5/6][workspace-language-agnostic-layout][docs] update the design documents and AGENTS.md`

- **Purpose:** Current documents describe the current layout.
- **Core changes:** paths in `docs/design/*.md` and explanations; the
  layout section in `build-system.md`; `architecture.md`'s code layout;
  `AGENTS.md` (Go commands from the root, the verification list, adding a
  crate or a Go package, the `proto/` rules).
- **Verification:** a link and path check: every `lib/`, `bin/`, and
  `proto/` path named in the design documents exists.

### `[6/6][workspace-language-agnostic-layout][check] verify no old paths remain`

- **Purpose:** Catch stragglers in one place.
- **Core changes:** fix any remaining reference to `crates/`, `go/`,
  `peppy-load`, `server_proto`, or `kvpb` outside completed plans; record
  verification outcomes in this plan.
- **Verification:** `grep` finds no old paths outside `docs/plans/00{01..26}*`;
  the full `AGENTS.md` check list passes.

If diff 6 finds nothing, it is folded into diff 5 and the stack becomes
five diffs.

## Open questions

None. Decisions agreed in discussion: role-based layout, flat `lib/`,
`proto/<api>/<version>/{rust,go}`, the package kept as `peppydb.kv.v1`
with the `PACKAGE_DIRECTORY_MATCH` exception, the root Go module, the
`workspace-` prefix for plans 0001, 0009, and 0027, and the long plan
label.

Deferred, not blocking: the area prefix for `0024-gcp-remote-bench.md`.

## Verification strategy

| Check | Command or procedure | Expected result |
| --- | --- | --- |
| Rust builds and tests | `tools/buck2 test //bin/... //lib/... //proto/...`; `cargo test --workspace --locked` | Pass |
| Go builds and tests from the root | `go vet ./... && go test ./...` | Pass; no package under `buck-out/` is listed |
| Generated code | `tools/gen-proto --check` | Only path and name changes against the previous code |
| Manifests and third-party rules | `tools/autocargo --check`; `tools/gobuckify-remote --check` | Up to date |
| Style and policy | `tools/fmt.sh`; `tools/clippy.sh` (runs `check-unsafe`) | Pass |
| Miri | `lib/bf-tree-circular-buffer` under `@build/mode/miri` | Pass |
| No behavior change | The server's process tests and the full test suite | Same results as before the stack |
| No old paths | `grep -rn 'crates/\|peppydb/go/\|server_proto\|kvpb'` outside completed plans | No matches |

## Progress

- [x] Agree on the layout and commit stack with the user.
- [x] 2/6 Rust libraries to `lib/`.
- [x] 3/6 Go module to the root.
- [x] 4/6 API into `proto/kv/v1`.
- [x] 5/6 Documents and `AGENTS.md`.
- [x] 6/6 Straggler check; stack-wide verification.

## Verification outcomes (2026-11-10)

On macOS (Apple M4 Pro); Docker was not available.

| Check | Result |
| --- | --- |
| `tools/fmt.sh`, `tools/clippy.sh` (with `check-unsafe`) | Pass; 28 Rust targets clean |
| `tools/buck2 test //bin/... //lib/... //proto/...` | Pass: 27 targets (one fewer than before: the hello-world Go test is gone) |
| `cargo test --workspace --locked` | Pass |
| `go vet ./... && go test ./...` from the root | Pass; `go list ./...` lists exactly `bin/peppydb-load`, `lib/load`, and `proto/kv/v1/go`, nothing under `buck-out/` |
| `tools/autocargo --check`, `tools/gobuckify-remote --check` | Up to date |
| `tools/gen-proto --check` | Up to date. The move of `kv.proto` passed `buf breaking`; a deliberate `uint32` to `uint64` change of `page_size` was still reported |
| Miri: `lib/bf-tree-circular-buffer` | Pass |
| Paths in design documents and `AGENTS.md` | Every `lib/`, `bin/`, `proto/`, and `tools/` path exists, except four labelled as future (`bin/peppydb-gateway`, `bin/peppydb-placement`, `proto/raft/v1`, `proto/third_party/`); every link and anchor resolves |
| Old paths | None outside completed plans, except the deliberate old-to-new mapping in `build-system.md` |

## Discoveries and plan changes

- **`buf breaking` compared by file.** With the `FILE` rules, moving
  `kv.proto` from `proto/peppydb/kv/v1/` to `proto/kv/v1/` was reported as
  a deleted file. `buf.yaml` now uses the `PACKAGE` rules, which compare by
  package and type: the move passes, and real changes, such as a field's
  type, are still caught.
- **buf's Go output root.** With the module at the repository root,
  `buf.gen.yaml` writes to `.` with `module=peppydb`, and `go_package`
  (`peppydb/proto/kv/v1/go;kvv1`) places the files in `proto/kv/v1/go/`.
- **gofmt does not read `go.mod`'s `ignore` list.** `tools/fmt.sh` finds Go
  files itself, pruning `buck-out/`, `target/`, `third-party/`, and dot
  directories, and checks `package_name` for each directory that holds Go
  files.
- **CI workflows named the old paths** (`.github/workflows/ci.yaml` and
  `buck2-update.yaml`), so diff 6 was needed. CI also lacked the
  `tools/gen-proto --check` step that plan 0026 added to `AGENTS.md`; it
  now runs in the Buck2 job. CI has no Sapling checkout, so there the
  breaking-change check is skipped and only lint and the generated code
  are checked; locally the check runs against the parent revision.
- **`go.sum` and `Cargo.lock`.** `go mod tidy` at the root marked
  `golang.org/x/sync` indirect, because the hello-world package that
  imported it is gone. `Cargo.lock` changed only in the workspace entry
  for `server-proto`, now `kv-proto`.

## Agent notes

- **Moves:** use `sl mv` so history follows the files.
- **Order matters:** diff 3 must not move `go/kvpb` before diff 4 moves the
  proto, or `tools/gen-proto --check` breaks in between.
- **Lock files:** `Cargo.lock` changes only in workspace package entries
  (`server-proto` to `kv-proto`); `go.sum` should not change.
- **The roadmap's next plan,** the Raft core, becomes plan 0028.
