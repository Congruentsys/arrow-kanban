# arrow-kanban — instructions for coding agents

Read [CONTRIBUTING.md](CONTRIBUTING.md) before writing code. This file restates only the rules an
agent breaks by habit; where the two ever disagree, CONTRIBUTING.md and `.github/workflows/ci.yml`
win.

## 1. The export gate — run it before every push

```bash
bash scripts/export-gate.sh
```

It scans `src tests ontology themes server/src server/tests docs` and fails on:

- **Upstream brand terms.** This engine was extracted from a private project. Never name that
  project, its crates or its vocabulary in the scanned tree.
- **Internal tracker IDs** (`CH-`, `EX-`, `HZ-`, `VY-`, `PROP-` … followed by 4+ digits) in
  comments, or next to prose in a printed string. **Strip the citation, keep the rationale:**

  ```rust
  // `resolution`/`closed_by` were absent from this projection      <- good
  // CH-1234 (upstream): `resolution`/`closed_by` were absent       <- fails
  ```

  Synthetic examples using `-42`, `-1234`, `-1235` or `-1240` are allowed, and fixture DATA such
  as `"EX-3001"` is fine.
- **Closed ontology vocabulary and closed crate names** (the `VOCAB` and `CLOSED_CRATES` lists in
  the gate).
- **Fleet-governance concepts that belong in the private composition layer, not the engine**: rank
  and ratification gate identifiers, decision ledgers, training and runtime-config terms (the
  `D1_CONCEPTS` list). A tag value is opaque data and is fine; the enforcement machinery is not.
- **A hardcoded agent roster**: two or more agent names in a comma or semicolon list. The engine
  derives agents from board data. A single assignee value is fine.
- The upstream CLI alias (`nk <cmd>`) or the word `CLAUDE` anywhere in the scanned tree. The
  public binary is `arrow-kanban`.
- Committed board state or data: `*.parquet`, `.arrow-kanban/`, or top-level `research/`,
  `eval/`, `datasets/`.
- A `path = "../…"` dependency that escapes the workspace, or a dependency on a closed crate.
- A `.rs` file under `src/`, `tests/`, `server/src/` or `server/tests/` whose first line is not
  `// SPDX-License-Identifier: MIT`.

**The rules mask each other.** The gate stops at the first violation class it finds, so fixing a
brand term can reveal a tracker-ID failure on the same line. "I fixed the reported line" is not
"the gate passes": re-run it until it prints `✅ [export-gate] clean`.

## 2. Always pass `--workspace`

The repo is a workspace: the root crate `arrow-kanban` plus the member `server/`
(`arrow-kanban-server`). A bare `cargo test` or `cargo clippy` checks the root crate only and
silently skips `server/`, where most of the server code and its acceptance tests live. Run what CI
runs:

```bash
cargo fmt --all --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked --all-features -- -D warnings
```

CI also builds and tests with `--no-default-features` and `--all-features`. A change behind a
feature flag must pass all three.

## 3. Two binaries, so `cargo install` needs a name

| package | binary | what it is |
|---|---|---|
| `arrow-kanban` (root) | `arrow-kanban` | the CLI and engine |
| `arrow-kanban-server` (`server/`) | `arrow-kanban-server` | the NATS request-reply server |

From git, name the package: `cargo install --git https://github.com/Congruentsys/arrow-kanban arrow-kanban`.
From a checkout: `cargo install --path .` (CLI) or `cargo install --path server` (server).

## 4. What the engine is not

The core is **domain-zero**: no agenda authority, no governance or promotion policy, no domain
model, no policy about who may write (see the README's "What this crate deliberately does NOT
do"). `server/tests/structural_core_only_acceptance.rs` pins the persisted core to exactly four
tables (`items`, `runs`, `item_comments`, `relations`). Domain concepts go in an extension, never
in the core.

## 5. Vocabulary is data, not Rust

- Typed relationships and item-type classes are declared in `ontology/kanban.ttl`. Adding a
  relationship is a data edit, and it should be pinned with a loader test (the pattern is in
  `src/relation_vocab.rs`).
- Board lifecycles, state graphs and WIP limits live in each board's `.arrow-kanban/config.yaml`
  (defaults in `src/config.rs`). Theme vocabulary files are in `themes/`.

Reach for Rust only when the data cannot express the change.

## 6. Toolchain

Edition 2024, so Rust 1.85 or newer.
