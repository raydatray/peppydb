# peppydb

## Planning and implementation
For substantial features or architectural changes:

1. Write a draft plan in `docs/plans/NNNN-<topic>.md` and present the proposal in chat.
2. Iterate on the design and stack with the user before editing code. Update
   the same plan file as decisions are made and open questions are resolved.
3. Once the user gives the go-ahead, finalize the agreed plan and implement
   it autonomously.

Present the proposal in chat:
- Explain the approach, important trade-offs, and open questions.
- Show representative Rust snippets for the core interfaces and algorithms.
- Use Mermaid diagrams when they clarify structure, data flow, or behavior.
- Show the proposed commit stack using the required commit-title format.
  For each diff, explain its purpose and show snippets of the core changes
  where useful.

Plan approval authorizes creating and revising local commits for the agreed
stack, including restacking as needed. Complete and verify the full stack,
and update the plan and relevant design documents before handing the work back.

Keep the stack plan current and provide concise chat updates at meaningful
milestones, such as completing a diff. Surface significant discoveries
and blockers.

Revisit the discussion when discoveries materially change the design or
scope; routine implementation decisions do not require another approval.

Keep planning proportional to the task. Small, well-specified fixes and
mechanical changes can proceed directly when requested.
The user may also explicitly ask to skip planning.

## Documentation

### Stack plans
- Use `docs/plans/NNNN-<topic>.md`, for example
  `docs/plans/0001-bf-tree-insertion.md`. Use the next available four-digit
  sequence number, starting at `0001`.
- Start from `docs/plans/0000-template.md` and adapt it to the stack.
  The number `0000` is reserved for the template.
- Keep one plan per stack, not per commit.
- Include the goal and scope, approach, proposed commit stack, open questions,
  core snippets or diagrams where useful, and verification strategy.
  Link to relevant design documents.
- Track progress with a concise checklist. Record significant discoveries
  and update the plan when the approach or stack changes.
- Retain completed plans as historical records. Current architectural
  intent belongs in `docs/design/`.

### Design documents
- Keep enduring architectural ideas in `docs/design/<topic>.md`, organized
  around components or concepts rather than individual stacks.
- Capture component relationships, important invariants and guarantees,
  and the reasoning behind significant trade-offs.
- Create or update a design document when work establishes or changes
  an enduring architectural decision or guarantee.
- Keep documented guarantees, relevant tests, and implementation
  consistent. Investigate disagreements rather than automatically
  changing one to match another.

### Explanations
- Keep conceptual walkthroughs in `docs/explanations/<topic>.md`.
- Explain how algorithms and systems work with diagrams and concrete examples.
  Cite relevant sources and sections.
- Write standalone explanations for humans rather than chat transcripts.
  Distinguish general concepts from peppydb-specific decisions, which belong
  in `docs/design/`. Link to explanations from relevant plans and design documents.

## References
- Use `docs/references.md` to find relevant background material.
  Read references as needed for the task.
- Cite relevant sources or sections in documentation.
  Record important assumptions and deviations from the source material.

## Documentation style
- Write plans, design documents, and explanations for human review first.
  A reader should be able to understand the goals, decisions, rationale, and
  open questions without reconstructing the conversation.
- Use clear technical English informed by ASD-STE100 principles.
  Treat these as style guidelines rather than a formal compliance requirement.
- Prefer short, direct sentences and active voice. Use consistent terminology
  and define unfamiliar terms or acronyms on first use.
- Make actors, actions, and conditions explicit where ambiguity is possible.
  Preserve precise technical terms, code identifiers, commands, and paths.
- Distinguish requirements, recommendations, assumptions, and open questions.
  Use "must" for requirements and "should" for guidance.
- Use diagrams and representative code snippets where they clarify an idea.
  Record meaningful outcomes rather than transcripts or raw tool logs.
- An optional `## Agent notes` section may contain task-specific execution
  details, such as entry points, verification commands, and implementation
  gotchas. Keep it human-readable too.
- Keep important decisions, constraints, and rationale in the main document.
  Shared agent instructions belong in `AGENTS.md` or skills.

## Build system
Buck2 builds and tests everything. `BUCK` files are the source of truth; the
`Cargo.toml` files outside `third-party/rust/` are generated. See
`docs/plans/0009-buck2-build.md` until `docs/design/build-system.md` exists.

- Run `tools/buck2`, never a `buck2` from `PATH`. It is pinned with DotSlash
  (`brew install dotslash`).
- Every tool is pinned. `tools/check-tools` fails if `rustc` or `go` on
  `PATH` differ from their pins. Put rustup's `~/.cargo/bin` before any other
  Rust installation on `PATH`.
- After changing a `BUCK` file, run `tools/autocargo` and commit the result.
- To add, change, or remove a crates.io dependency, edit
  `third-party/rust/Cargo.toml`, run `tools/buckify`, and add
  `//third-party/rust:<name>` to the `deps` of the targets that use it. If
  reindeer warns about a build script, add
  `third-party/rust/fixups/<crate>/fixups.toml` with `buildscript.run = true`.
- To add a crate, create its directory with a `BUCK` file that uses
  `peppy_rust_library` from `//build:defs.bzl`, list exactly the targets that
  may depend on it in `visibility`, and run `tools/autocargo`. Do not run
  `cargo init` or `cargo new`.
- Release builds: `tools/buck2 run @build/mode/release <target> -- <args>`.

## Verification
Run commands from the repository root. During iteration, use focused checks
such as `tools/buck2 test //crates/peppydb-bftree/...`, or run one unit-test
binary with a filter:
`tools/buck2 run //crates/peppydb-bftree:peppydb-bftree-unittest -- open::`.

Before completing work that changes code or `BUCK` files, run:

```sh
tools/fmt.sh
tools/clippy.sh
tools/buck2 test //crates/... //bin/...
tools/autocargo --check
cargo test --workspace --locked
```

`buck2 test` reports one result per target; use `tools/buck2 run` on a
`-unittest` target to see individual tests. Clippy diagnostics do not fail
`buck2 build`; `tools/clippy.sh` collects them and fails if there are any.

Record additional algorithm-specific checks or benchmarks in the stack plan.
Record verification outcomes in the plan and summarize them at handoff.
Report any unresolved failures or blockers explicitly.

## Version control
- Use Sapling (`sl`) exclusively for version-control operations, including
  status, diffs, history, commits, and stack management.

## Stacked diffs
- Organize work as an ordered stack of small, cohesive commits. Each commit
  represents one reviewable diff and builds on the preceding commit.
- When a stack starts with a plan commit, count it as diff 1 and include it
  in the total number of diffs. Number implementation commits after it.
- Aim for about 300 changed lines or fewer per diff (additions plus deletions
  relative to its parent). This is a reviewability guideline, not a hard limit.
  Large renames, mechanical moves, and similar mechanical changes can exceed
  it. Split larger substantive changes when doing so makes review easier.
- Keep each commit buildable and its relevant tests passing. Include tests
  for a behavior change in the same diff as the implementation.
- Inspect the stack with `sl smartlog`. Check pending changes with
  `sl diff --stat` and individual commits with `sl diff -c <rev> --stat`.
- After revising an earlier commit, use `sl restack` as needed to move its
  descendants onto the updated commit.

## Commit titles
Use this format:

```text
[n/N][plan][section] brief commit title
```

- `n`: the commit's position in the stack, starting at 1 from the base.
- `N`: the total number of commits in the stack; use `[1/1]` for a single diff.
- `plan`: a short label for the overall idea or plan, consistent across the stack.
- `section`: the component or area touched by this diff.
- Use a concise, imperative title. Keep numbering accurate when the stack changes.

Example:

```text
[1/3][bf-tree-insert][layout] define leaf node layout
[2/3][bf-tree-insert][search] implement leaf lookup
[3/3][bf-tree-insert][insert] implement leaf insertion
```

## Rust style
- Prefer iterators whenever practical, including transformations, filtering,
  searches, and reductions.
- Use a `for` loop when it makes side-effecting work clearer. Do not force
  such loops into `for_each` just to use an iterator.
- Prefer type inference and binding annotations over turbofish syntax.
  When an explicit type is needed, put it on the binding rather than the
  call (e.g. `collect`, `parse`, or pointer `cast`). Use turbofish only when
  the required generic arguments cannot be inferred from the binding or context.

Preferred:

```rust
let values: Vec<_> = iter.collect();
let count: u64 = input.parse()?;
let bytes: *const u8 = ptr.cast();
```

Avoid:

```rust
let values = iter.collect::<Vec<_>>();
let count = input.parse::<u64>()?;
let bytes = ptr.cast::<u8>();
```
