# Contributing to arrow-kanban

Thanks for contributing! Two authoring rules in this repo routinely surprise
new contributors (and CI enforces both), so they come first.

## Before you write a comment: the export gate

`scripts/export-gate.sh` runs in CI on every PR and scans the open tree
(`src tests ontology themes server/src server/tests`). It enforces two rules
that are easy to violate out of habit if you also work in a private tracker:

1. **No upstream brand terms.** The scanned tree must not mention the
   private upstream project this engine was extracted from (the gate's
   `BRAND` pattern). arrow-kanban is a standalone open-source project; its
   code should read as one.

2. **No internal tracker IDs in comments or user-facing strings.** A
   reference like `CH-1234` (any letters-dash-4+-digits work-item shape)
   points at a tracker a public reader cannot resolve, so it must not appear
   in the open tree. **Strip the citation, keep the rationale** — the reason
   a change exists is welcome and encouraged; the opaque ID is not:

   ```rust
   // `resolution`/`closed_by` were absent from this projection      <- good: the rationale
   // CH-1234 (upstream): `resolution`/`closed_by` were absent       <- both rules violated
   ```

   Synthetic doc examples with short numbers (`-42`, `-1234`) are allowed —
   the gate only matches 4-or-more-digit IDs, which is the real allocator's
   range.

**The two rules mask each other.** The gate reports the first violation
class it hits, so removing a brand term can *reveal* a tracker-ID failure
on the same line — a fix for one is not evidence of the other. (This has
cost real review rounds: a reviewer proposed the brand-only fix, and the
second failure surfaced only after it was applied.)

**Check locally before pushing** — it is the same gate CI runs, and it
takes a second:

```bash
bash scripts/export-gate.sh
```

`red-battery` (the second CI check) establishes its baseline by running the
export gate on the clean tree first, so an export-gate failure cascades into
two red checks. One cause, fix it once.

## The usual mechanics

- Branch from `main`; open a PR against `main`. PRs need a passing
  `export-gate`, `red-battery`, and `build-test`, plus a review.
- `cargo test` and `cargo clippy --all-targets -- -D warnings` should be
  clean; `cargo fmt` before committing.
- The relationship vocabulary lives in `ontology/kanban.ttl` (data, not
  Rust) — see the README's "Typed relationships" section before adding
  predicates, and pin new pairs with a loader test so a dropped block goes
  red (`src/relation_vocab.rs` has the pattern).
- Issues welcome for anything you'd have wanted documented here.
