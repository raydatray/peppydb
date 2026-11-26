# 0036: Dynamic affected-target CI and BuildBuddy hybrid execution

**Status:** In progress

**Design references:** [Build system](../design/build-system.md),
[plan 0035](0035-workspace-hermetic-build.md).

## Goal and scope

Make CI both **visible per package** and **fast**:

1. Replace the monolithic `buck2` CI job with a Buck2 `uquery`-driven `plan`
   job that dynamically fans out one GitHub Actions check per affected package
   (`buck2-test (//lib/bf-tree)`, `buck2-test (//lib/engine)`, etc.), runs Miri
   automatically when unsafe crates or toolchains are affected, and skips lint
   and test jobs on docs-only PRs.
2. Add opt-in **BuildBuddy hybrid execution + remote cache** so GitHub Actions
   matrix runners and your WSL2 Linux machine share cached artifacts and spill
   overflow actions to BuildBuddy's Linux executors, while macOS local
   development (and any contributor without a BuildBuddy API key) stays 100%
   local with zero configuration.

- **In scope:**
  - Opt-in hybrid execution platforms (`//build:execution-platforms-buildbuddy`)
    in `build/platforms.bzl` and `build/BUCK` (`local_enabled = True`,
    `remote_enabled = True`, `remote_cache_enabled = True`,
    `use_limited_hybrid = True`, `allow_limited_hybrid_fallbacks = True`,
    `allow_cache_uploads = True`), pinned to Ubuntu 24.04 by digest, covering
    both normal builds and Miri.
  - Committed, secret-free `build/config/buildbuddy.bcfg` configuring
    `[buck2_re_client]` against `remote.buildbuddy.io` with
    `$BUILDBUDDY_API_KEY`.
  - Ignoring `/.buckconfig.local` in `.gitignore` and `.slignore` so WSL2
    developers can opt in once with `<file:build/config/buildbuddy.bcfg>` and
    never pass extra flags.
  - Dynamic `.github/workflows/ci.yaml`:
    - `plan` job computing affected packages via `tools/buck2 uquery` (`rdeps`
      + `owner`) directly inline (no extra tool scripts), emitting `packages`,
      `run_lint`, and `run_miri`.
    - `buck2-lint` conditional job (`tools/fmt.sh`, `tools/clippy.sh`,
      `tools/gen-proto --check`), skipped on docs-only PRs.
    - `buck2-test` dynamic matrix (one PR check per affected package).
    - `buck2-miri` conditional job (runs when `//lib/bf-tree-circular-buffer`
      or global build/toolchain files are affected).
    - Automatic `.buckconfig.local` activation in CI whenever
      `secrets.BUILDBUDDY_API_KEY` is configured (graceful fallback to local
      builds on fork PRs or before the secret is set).
  - Updating `docs/design/build-system.md` with the CI fanout and BuildBuddy
    setup (including the one-line WSL2 `.buckconfig.local` recipe).
- **Out of scope:**
  - Remote execution on macOS (macOS local dev remains 100% local).
  - Building or publishing a custom Docker image for RBE: because plan 0035
    made all Linux toolchains hermetic inside Buck2, the stock digest-pinned
    Ubuntu 24.04 image suffices.
  - Replacing the secondary `cargo` and `go` CI jobs.
  - Configuring the BuildBuddy account or GitHub repository secret itself (the
    user adds `BUILDBUDDY_API_KEY` in GitHub Settings → Secrets).
- **Done when:**
  - Local macOS builds with plain `tools/buck2 test //...` remain 100% local
    with no API key or network call to BuildBuddy.
  - `tools/buck2 audit providers //build:execution-platforms-buildbuddy`
    resolves cleanly with hybrid execution settings and pinned Ubuntu 24.04
    `container-image`, and `tools/buck2 audit config --config-file
    build/config/buildbuddy.bcfg` shows the expected `[buck2_re_client]` and
    `execution_platforms` settings.
  - The inline `plan` query logic is verified locally against representative
    file-change sets (Rust leaf crate, core page crate crossing into Go
    `lincheck`, `bf-tree-circular-buffer` triggering Miri, Go-only crate,
    `build/` global change, docs-only change skipping lint & tests, and
    deleted file).
  - `.github/workflows/ci.yaml` is valid YAML and gracefully handles both
    `BUILDBUDDY_API_KEY` set and unset.
  - All workspace verification checks pass.

## Approach

### How config layering separates macOS, WSL2, and CI

```mermaid
flowchart TB
    base[".buckconfig (committed)<br/>local execution_platforms only"]
    mac["macOS dev<br/>(no .buckconfig.local)"] -->|"100% local, no key"| base
    wsl["WSL2 dev<br/>.buckconfig.local (git-ignored):<br/>&lt;file:build/config/buildbuddy.bcfg&gt;"] --> bcfg["build/config/buildbuddy.bcfg<br/>remote.buildbuddy.io + $BUILDBUDDY_API_KEY<br/>execution-platforms-buildbuddy"]
    ci["GitHub Actions CI<br/>writes .buckconfig.local when<br/>secrets.BUILDBUDDY_API_KEY is set"] --> bcfg
    bcfg -->|"hybrid execution + shared cache/CAS<br/>across WSL2 + CI"| bb["BuildBuddy RBE + Cache"]
```

Writing `.buckconfig.local` in CI (when `BUILDBUDDY_API_KEY` is non-empty)
means **every** `tools/buck2` call — including those inside `tools/fmt.sh`,
`tools/clippy.sh`, `tools/gen-proto`, and `tools/buck2 test` — uses BuildBuddy
automatically without modifying any tool scripts.

### 1. Hybrid execution platforms (`build/platforms.bzl`, `build/BUCK`, `build/config/buildbuddy.bcfg`)

```python
# build/platforms.bzl (sketch)
executor_config = CommandExecutorConfig(
    local_enabled = True,
    remote_enabled = ctx.attrs.remote,
    remote_cache_enabled = ctx.attrs.remote,
    use_limited_hybrid = ctx.attrs.remote,
    allow_limited_hybrid_fallbacks = ctx.attrs.remote,
    allow_cache_uploads = ctx.attrs.remote,
    remote_execution_properties = ctx.attrs.remote_execution_properties,
    remote_execution_use_case = "peppydb" if ctx.attrs.remote else None,
    remote_output_paths = "output_paths" if ctx.attrs.remote else None,
    use_windows_path_separators = host_info().os.is_windows,
)
```

```python
# build/BUCK (sketch)
# Stock Ubuntu 24.04 pinned by digest; all Linux compilers/linkers/sysroot are
# hermetic Buck2 inputs (plan 0035), so no custom RBE image is needed.
_RBE_PROPS = {
    "OSFamily": "Linux",
    "Arch": "arm64" if host_info().arch.is_aarch64 else "amd64",
    "container-image": "docker://docker.io/library/ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852",
}

peppy_execution_platform(
    name = "exec-host-buildbuddy",
    constraints = [host_configuration.cpu, host_configuration.os],
    remote = True,
    remote_execution_properties = _RBE_PROPS,
)

peppy_execution_platform(
    name = "exec-host-miri-buildbuddy",
    constraints = [host_configuration.cpu, host_configuration.os, ":mode-miri"],
    remote = True,
    remote_execution_properties = _RBE_PROPS,
)

peppy_execution_platforms(
    name = "execution-platforms-buildbuddy",
    platforms = [":exec-host-buildbuddy", ":exec-host-miri-buildbuddy"],
    visibility = ["PUBLIC"],
)
```

```ini
# build/config/buildbuddy.bcfg
[buck2]
digest_algorithms = SHA256

[buck2_re_client]
engine_address       = remote.buildbuddy.io
action_cache_address = remote.buildbuddy.io
cas_address          = remote.buildbuddy.io
http_headers         = x-buildbuddy-api-key:$BUILDBUDDY_API_KEY

[build]
execution_platforms = root//build:execution-platforms-buildbuddy
```

### 2. Dynamic affected-package CI fanout (`.github/workflows/ci.yaml`)

```mermaid
flowchart LR
    plan["plan (~15s)<br/>git diff + buck2 uquery"] -->|if run_lint| lint["buck2-lint<br/>fmt + clippy + gen-proto"]
    plan -->|matrix.package| test["buck2-test (//lib/...)<br/>one PR check per package"]
    plan -->|if run_miri| miri["buck2-miri<br/>unsafe crates under Miri"]
    cargo["cargo<br/>autocargo --check + cargo test"]
    go["go<br/>gobuckify --check + go vet/test"]
```

- **Global triggers** (`build/`, `third-party/`, `tools/`, `.buckconfig`,
  `go.mod`, `go.sum`, `rust-toolchain.toml`, `.github/workflows/ci.yaml`):
  query all testable packages via
  `tools/buck2 uquery 'kind("rust_library|rust_test|go_test", //bin/... + //lib/... + //proto/...)'`
  and set `run_lint=true` and `run_miri=true`.
- **Scoped changes:** for changed files under `bin/`, `lib/`, `proto/`:
  - Existing files → `owner("<path>")`.
  - `BUCK` files or deleted files → nearest owning package (`//<dir>:`).
  - Run `tools/buck2 uquery 'kind("rust_library|rust_test|go_test", rdeps(//bin/... + //lib/... + //proto/..., <seeds>))'`.
  - Deduplicate targets (`root//lib/foo:bar` → `//lib/foo`) into a sorted JSON
    array `packages`.
  - Set `run_lint=true` whenever any non-doc file changed (anything outside
    `docs/` and `*.md`).
  - Set `run_miri=true` if `//lib/bf-tree-circular-buffer` is in `packages`.
- **Docs-only PRs:** `packages` is `[]`, `run_lint` is `false`, and `run_miri`
  is `false`, so `buck2-lint`, `buck2-test`, and `buck2-miri` all skip cleanly.
- **Zero hand-maintained lists:** adding a new crate under `lib/` or `bin/`
  automatically makes it discoverable by `plan`.

## Decisions and trade-offs

- **Limited hybrid execution (`use_limited_hybrid = True`,
  `allow_limited_hybrid_fallbacks = True`) immediately:**
  - *Why:* Always checks local state first, then BuildBuddy's remote cache; on
    misses, keeps fast local WSL2 / CI cores busy and spills overflow work to
    BuildBuddy executors (falling back cleanly to local if remote queues or an
    action is local-only).
  - *Stock Ubuntu 24.04 image:* Because plan 0035 made every Linux toolchain
    hermetic inside Buck2, the stock digest-pinned `ubuntu:24.04` image
    (glibc 2.39, matching LLVM 23.1.6's host requirement) works with zero
    custom RBE Dockerfile to maintain.
  - *Trade-off:* First cold remote action that invokes LLVM uploads the
    unpacked LLVM toolchain digest to BuildBuddy CAS; subsequent runs reuse it
    from CAS.
- **Skip `buck2-lint` on docs-only PRs:**
  - Documentation-only PRs skip `buck2-lint`, `buck2-test`, and `buck2-miri`
    completely, finishing in ~15 seconds.
- **Package-level matrix (`//lib/bf-tree`) over per-target matrix:**
  - One PR check per crate/package (covering both unit tests and doctests),
    readable in GitHub's PR UI and well under GitHub's 256-job matrix limit.
- **Inline Python in `ci.yaml` instead of a new script in `tools/`:**
  - Keeps `tools/` focused on developer commands; the query logic lives beside
    the matrix that consumes it.
- **Graceful fallback when `BUILDBUDDY_API_KEY` is unset:**
  - Fork PRs and repos before setting the secret run locally without auth
    errors.

## Proposed commit stack

### `[1/4][workspace-rbe][plan] plan dynamic CI fanout and BuildBuddy hybrid execution`

- **Purpose:** Record the hybrid BuildBuddy config layering and dynamic CI design.
- **Core changes:** Add `docs/plans/0036-workspace-remote-execution.md`.
- **Verification:** Review and link check.

### `[2/4][workspace-rbe][config] add BuildBuddy hybrid execution platforms and config`

- **Purpose:** Enable opt-in BuildBuddy hybrid execution + remote caching for
  WSL2 and CI without affecting default local builds.
- **Core changes:**
  - Add `remote` and `remote_execution_properties` attributes to
    `peppy_execution_platform` in `build/platforms.bzl`.
  - Add `exec-host-buildbuddy`, `exec-host-miri-buildbuddy`, and
    `execution-platforms-buildbuddy` (with digest-pinned Ubuntu 24.04) in
    `build/BUCK`.
  - Add `build/config/buildbuddy.bcfg`.
  - Ignore `/.buckconfig.local` in `.gitignore` and `.slignore`.
- **Verification:**
  - `tools/buck2 audit providers //build:execution-platforms-buildbuddy`
    resolves cleanly with `local_enabled = True`, `remote_enabled = True`,
    `use_limited_hybrid = True`, `remote_cache_enabled = True`, and the pinned
    Ubuntu 24.04 `container-image`.
  - `tools/buck2 audit config --config-file build/config/buildbuddy.bcfg`
    shows `[buck2_re_client]` and `execution-platforms-buildbuddy`.
  - Plain `tools/buck2 test //lib/bf-tree-node-id/...` still runs 100% locally.

### `[3/4][workspace-rbe][ci] dynamically fan out affected Buck2 packages in CI`

- **Purpose:** Show one PR check per affected package, skip untouched packages
  (and skip lint on docs-only PRs), run Miri automatically when relevant, and
  activate BuildBuddy whenever the secret is set.
- **Core changes:**
  - Rewrite the `buck2` job in `.github/workflows/ci.yaml` into `plan`,
    `buck2-lint`, `buck2-test` (dynamic matrix), and `buck2-miri`.
  - Add the conditional `.buckconfig.local` step using
    `secrets.BUILDBUDDY_API_KEY`.
- **Verification:**
  - Run the inline Python query snippet locally against simulated diff lists
    (leaf crate, `bf-tree-page` crossing into Go `lincheck`,
    `bf-tree-circular-buffer` triggering Miri, Go-only crate, `build/` global
    change, docs-only change skipping lint & tests, and deleted file) and
    verify exact JSON output.
  - Validate `.github/workflows/ci.yaml` syntax.

### `[4/4][workspace-rbe][docs] document dynamic CI and BuildBuddy setup`

- **Purpose:** Keep `docs/design/build-system.md` accurate and document the
  one-line WSL2 `.buckconfig.local` setup.
- **Core changes:**
  - Update `docs/design/build-system.md` with CI dynamic test selection and
    BuildBuddy hybrid execution instructions.
  - Mark this plan complete with recorded verification outcomes.
- **Verification:** Full `AGENTS.md` verification suite passes.

## Open questions

None. Confirmed during planning:
- Use limited hybrid execution (`use_limited_hybrid = True`,
  `allow_limited_hybrid_fallbacks = True`) immediately with digest-pinned stock
  Ubuntu 24.04.
- Skip `buck2-lint` on docs-only PRs.

## Verification strategy

| Check | Command or procedure | Expected result |
| --- | --- | --- |
| Default local build unchanged | `tools/buck2 test //lib/bf-tree-node-id/...` (no API key) | Runs 100% locally, passes |
| BuildBuddy hybrid platform & config | `tools/buck2 audit providers //build:execution-platforms-buildbuddy` and `tools/buck2 audit config --config-file build/config/buildbuddy.bcfg` | Resolves `local_enabled = True`, `remote_enabled = True`, `use_limited_hybrid = True`, `remote_cache_enabled = True`, `allow_cache_uploads = True`, Ubuntu 24.04 image, and `remote.buildbuddy.io` |
| `plan` query logic | Run the exact `ci.yaml` Python logic against 7 synthetic file-change sets | Exact expected `packages`, `run_lint`, and `run_miri` outputs for every case |
| Workspace checks | `tools/fmt.sh`, `tools/clippy.sh`, `tools/buck2 test //bin/... //lib/... //proto/...`, `tools/autocargo --check`, `tools/gobuckify-remote --check`, `tools/gen-proto --check`, Miri, `cargo test`, `go vet && go test` | All pass |

## Progress

- [x] Agree on the design and commit stack with the user.
- [x] `[1/4][workspace-rbe][plan]`
- [ ] `[2/4][workspace-rbe][config]`
- [ ] `[3/4][workspace-rbe][ci]`
- [ ] `[4/4][workspace-rbe][docs]`
- [ ] Complete stack-wide verification and design-document updates.

## Discoveries and plan changes

None yet.
