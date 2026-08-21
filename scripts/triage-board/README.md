# Triage Board

A lightweight, self-contained triage view over an arrow-kanban export: one HTML page
(rank strip → filterable/sortable table → container hierarchy → per-item notes) plus a
CSV twin for spreadsheet workflows.

```bash
# From a running server (any client exposing `export --format json` with offset/limit):
KANBAN_CLI="your-kanban-client --server nats://host:4222" \
  scripts/triage-board/build-board.sh out/ --ranks "campaign-a,campaign-b" --title "Fleet Board"

# From an existing export file, no server needed:
scripts/triage-board/build-board.sh export.json out/ --ranks "campaign-a,campaign-b"
```

- **Ranking is injected data** (`--ranks`, highest first): an item's campaign is its
  first matching rank tag; unmatched items group under `(unranked)` at the end. With no
  `--ranks`, everything is one unranked group — the tool carries no deployment vocabulary.
- **Open items only**: terminal statuses (done/complete/abandoned/retired/obsolete) are
  excluded at load.
- **Hierarchy**: items whose `related`/`depends_on` name a voyage/campaign in the same
  group indent beneath it.
- **Notes** live in the viewer's browser (localStorage) — *Copy notes* exports
  `ID → note` lines to the clipboard for handing decisions back to whatever process
  owns the board. Nothing writes to the store: the page is a read-only view.

The page is a single file with no external dependencies beyond a Google-Fonts
stylesheet, renders in light and dark themes, and stays usable at ~1000 rows.
