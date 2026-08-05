# arrow-kanban

An **Arrow-native work-graph engine**: work items, their typed relationships, and their
history live in Apache Arrow tables persisted to Parquet — not in a pile of markdown files —
so the board is columnar, queryable, and durable by construction. It runs as a local library,
as a single-writer NATS server, or in-process, and the three share one proven semantics.

If you need a work tracker whose data you can *query* rather than scrape, whose durability is a
tested contract rather than a hope, and whose vocabulary you extend by editing data rather than
forking code — keep reading.

## Why arrow-kanban

A kanban CRUD is not interesting. These are the things this engine does that most don't:

- **Arrow-native, end to end.** Items, relations, comments, and status history are Arrow
  `RecordBatch`es written atomically to Parquet, with a graph-native commit log. Analysis is a
  columnar query, not an export step.
- **Durable by construction, and it is a *tested* contract.** A commit point plus a
  transactional outbox, single-writer fencing, fail-closed replica gap detection, and a declared
  staleness bound — each pinned by an acceptance battery you can run yourself
  ([Durability and consistency contract](#durability-and-consistency-contract)).
- **Data-driven vocabulary.** Item types, lifecycles, WIP policy, and the typed-relationship
  vocabulary load from a `.ttl` (`ontology/kanban.ttl`). **A new relationship is a data edit, not
  a code change** — add an `owl:ObjectProperty` with its domain, range, and inverse and the CLI
  accepts it on the next run.
- **Typed, directional relationships.** `dependsOn`, `blocks`, `implements`, `validates`,
  `measures`, `partOf`, … so you can ask *which experiment validates this hypothesis?* without
  parsing prose — while the flat dependency columns planning views need are kept as a projection
  of the same edges.
- **Three deployment shapes, one semantics.** Local, NATS server, and in-process embedding share
  identical core behaviour, and that is not a claim — it is
  [conformance fixtures](#conformance-fixtures--run-them-against-your-own-integration) you can
  execute against your own integration.
- **Built to be extended.** Five public traits are the seams for commands, storage, fencing,
  replication, and snapshotting ([Extension points](#extension-points)); the core stays
  domain-zero, enforced by a test.

**What it deliberately is *not*:** an agenda authority, a governance/promotion policy, or a domain
model. It stores and serves work items; deciding what an item *means* or who may write is the
consumer's job — which is exactly why the engine is generic. See
[What this crate deliberately does NOT do](#what-this-crate-deliberately-does-not-do).

## Install

```bash
# From a local checkout
cargo install --path .

# Or from git
cargo install --git https://github.com/hankh95/arrow-kanban
```

Requires a Rust toolchain supporting **edition 2024** (Rust 1.85 or newer). This installs the
`arrow-kanban` binary. The default build has no network dependencies at all.

## Quickstart

```bash
# Initialize a board in the current directory (creates ./.arrow-kanban/).
# Themes are presets for the type/lifecycle vocabulary: `nautical`, `software`, or `hdd`.
arrow-kanban init --theme software

# Create an item, move it along, inspect it (`create` prints the new item's id)
arrow-kanban create feature "Wire up the ingest pipeline"
arrow-kanban move FT-1300 in_progress --assign "alice"
arrow-kanban show FT-1300

# Query and plan over the dependency graph
arrow-kanban list --status in_progress
arrow-kanban query "in-progress features"
arrow-kanban roadmap

# Validate against the SHACL shapes
arrow-kanban validate --all
```

Run `arrow-kanban --help` (or `arrow-kanban <command> --help`) for the full command list.
Highlights:

| Command | What it does |
|---------|--------------|
| `init` | Initialize a board (theme: `nautical`, `software`, or `hdd`) |
| `create` / `update` / `move` / `comment` | Item CRUD and lifecycle |
| `list` / `board` / `show` / `query` | Views and search |
| `roadmap` / `critical-path` / `worklist` / `blocked` | Dependency-graph planning |
| `stats` / `history` | Board metrics and recent activity |
| `validate` | SHACL conformance checking |
| `hdd` | Research-board commands (papers, hypotheses, experiments, measures) |
| `export` | Export a board or item (markdown, json, html, …) |
| `migrate` | Import markdown files (with turtle frontmatter) into the store |
| `backup` / `restore` | Snapshot and restore the Arrow store |
| `templates` / `boards` / `next-id` / `rank` / `next` | Utilities |

## NATS multi-agent mode (optional)

Build with the `client` feature to talk to a NATS server so multiple agents can collaborate on
one board with single-writer semantics:

```bash
cargo build --features client
arrow-kanban --server nats://localhost:4222 board
arrow-kanban mcp-server   # expose the board over the Model Context Protocol (stdio)
```

## Storage layout

A board lives in `./.arrow-kanban/`:

```
.arrow-kanban/
  config.yaml          # boards, WIP limits, state graphs
  items.parquet        # work items
  relations.parquet    # typed edges
  item_comments.parquet
  runs.parquet         # status-change history
  _commits.json        # graph-native commit log (audit trail)
```

## Arrow compatibility policy

**This crate hands `RecordBatch` values across its API boundary, so its Arrow major version is
part of its public contract.** An in-process consumer linking a different Arrow major gets
type-mismatch errors at compile or link time, not at runtime — and the failure names Arrow
internals rather than this crate.

| | |
|---|---|
| Arrow major | **57** (`arrow = "57"`, root and `arrow-kanban-server`) |
| Bump policy | an Arrow **major** bump is a **breaking change** for in-process consumers and is released as a minor version bump of this crate while pre-1.0 |
| Not affected | consumers using **only** the CLI or the NATS wire protocol — those exchange JSON, not `RecordBatch` |

If you embed the library, pin the same Arrow major. If you drive it over NATS or the CLI, you do
not need to care.

## Extension points

The engine is generic; the domain lives outside it. Five public traits are the seams:

| trait | where | what it lets you do |
|---|---|---|
| `EngineExtension` | `server/src/extension.rs` | add your own commands — the engine routes an unrecognised verb to the extension before refusing it |
| `ExtensionSnapshot` | `server/src/extension.rs` | give your extension's state a snapshot/restore path, so it survives restart with the core |
| `StorageBackend` | `server/src/storage.rs` | replace the durable substrate; this is the commit point and the transactional-outbox source |
| `WriterLease` | `server/src/lease.rs` | supply your own single-writer fencing |
| `ReplicaSource` | `server/src/replica.rs` | feed a read replica from something other than the local filesystem |

**Worked example:** a composite `EngineExtension` can multiplex the engine's single extension slot
across several namespaced sub-extensions over a custom `StorageBackend`. Nothing in that
arrangement is privileged; it is the documented seam used as intended.

## Durability and consistency contract

What is guaranteed, and where to read it:

- **Commit point + transactional outbox.** `StorageBackend` is *"the durable storage boundary —
  the commit point + transactional outbox source"* (`server/src/storage.rs`). `committed_seq()`
  is the outbox relay's source and re-derives the unpublished tail after a crash, so a crash
  between commit and publish does not lose an event.
- **Single-writer fencing.** `WriterLease` (`server/src/lease.rs`); pinned by
  `server/tests/lease_fencing_acceptance.rs`.
- **Replica gap detection, fail-closed.** A replica's tail must be contiguous; a hole yields
  `ReplicaError::Gap { expected, found }` rather than a silently short board
  (`server/src/replica.rs`). Pinned by `server/tests/replica_gap_acceptance.rs`.
- **Declared staleness bound.** `StalenessBound { max_staleness, max_lag_seqs }`
  (`server/src/replica.rs`) — a time bound, optionally with a position ceiling. A replica past
  its bound **refuses reads** rather than answering stale ones; confirmation is a currency probe
  that either shows it level with the source or lets it catch up.

## Conformance fixtures — run them against your own integration

These exist to be executed by consumers, not only by this repo's CI. They are the evidence that
the local, server and in-process paths share identical core semantics:

```bash
cargo test -p arrow-kanban-server --test spec08_conformance          # the core semantic contract
cargo test -p arrow-kanban-server --test lease_fencing_acceptance    # single-writer fencing
cargo test -p arrow-kanban-server --test outbox_atomicity_acceptance # commit/publish atomicity
cargo test -p arrow-kanban-server --test replica_gap_acceptance      # fail-closed gap detection
cargo test -p arrow-kanban-server --test write_durability_acceptance # survives restart
cargo test -p arrow-kanban-server --test backward_compat_acceptance  # older stores still load
cargo test -p arrow-kanban-server --test structural_core_only_acceptance # no domain in the core
```

If you implement `StorageBackend` or `EngineExtension`, the persistence and write-path batteries
(`extension_persistence_acceptance`, `extension_writepath_acceptance`) are the ones to point at
your implementation.

## What this crate deliberately does NOT do

Stated so you can tell what you still have to build:

- **No agenda authority.** It stores and serves work items; deciding what an agent should do next
  is not its job.
- **No promotion or governance policy.** There is no notion of a claim being validated, ratified,
  or promoted between confidence levels.
- **No domain model.** The core is domain-zero by design — `structural_core_only_acceptance`
  exists to keep it that way. Your vocabulary goes in an extension, not in the engine.
- **No opinion about who may write.** `WriterLease` gives you fencing; the policy is yours.

## License

MIT — see [LICENSE](LICENSE).

## Trademarks

Apache Arrow is a trademark of The Apache Software Foundation; arrow-kanban is
not affiliated with or endorsed by the ASF.
