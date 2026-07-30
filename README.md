# arrow-kanban

An **Arrow-native, graph-native kanban engine**. Work items live in Apache Arrow
tables persisted to Parquet — not in a pile of markdown files — so the board is
columnar, queryable, and cheap to snapshot. Items are validated against **SHACL
shapes**, organized across **dual boards** (a development board and a research
board), and linked with **typed, directional relationships** so the dependency
graph is machine-queryable.

- **Arrow/Parquet storage** — items, relations, comments, and runs are Arrow
  `RecordBatch`es written atomically to Parquet, with a graph-native commit log.
- **SHACL-validated shapes** — every item type has a shape (`ontology/shapes/`);
  `arrow-kanban validate` checks conformance and can suggest fixes.
- **Dual boards** — `development` (expeditions, chores, voyages, …) and
  `research` (papers, hypotheses, experiments, measures, ideas, literature),
  each with its own per-type lifecycle.
- **Typed relationships** — `dependsOn`, `blocks`, `implements`, `validates`,
  `measures`, `partOf`, … (defined in `ontology/kanban.ttl`), so you can ask
  *which experiment validates this hypothesis?* without reading prose.
- **Planning views** — roadmap, critical-path, worklist, blocked, and stats,
  computed over the dependency graph.
- **Local by default** — no network dependencies. An optional `client` feature
  adds NATS multi-agent collaboration.

## Requirements

A Rust toolchain supporting **edition 2024** (Rust 1.85 or newer).

## Install

```bash
# From a local checkout
cargo install --path .

# Or from git
cargo install --git https://github.com/hankh95/arrow-kanban
```

This installs the `arrow-kanban` binary.

## Quickstart

```bash
# Initialize a board in the current directory (creates ./.arrow-kanban/)
arrow-kanban init

# Create an item
arrow-kanban create expedition "Wire up the ingest pipeline"

# See the board
arrow-kanban board

# Move it along
arrow-kanban move EX-1300 in_progress --assign "alice"

# Query and inspect
arrow-kanban list --status in_progress
arrow-kanban show EX-1300
arrow-kanban query "in-progress expeditions"

# Validate against SHACL shapes
arrow-kanban validate --all
```

Run `arrow-kanban --help` (or `arrow-kanban <command> --help`) for the full
command list. Highlights:

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

Build with the `client` feature to talk to a NATS server so multiple agents can
collaborate on one board with single-writer semantics:

```bash
cargo build --features client
arrow-kanban --server nats://localhost:4222 board
arrow-kanban mcp-server   # expose the board over the Model Context Protocol (stdio)
```

The default build has no network dependencies at all.

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

## License

MIT — see [LICENSE](LICENSE).

## Trademarks

Apache Arrow is a trademark of The Apache Software Foundation; arrow-kanban is
not affiliated with or endorsed by the ASF.
