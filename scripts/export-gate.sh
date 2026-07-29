#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# arrow-kanban FOSS export gate — the machine-enforced open-source boundary.
#
# This is the BLOCKING gate the public repo runs in CI (and you can run locally).
# It fails the build if any closed-source material leaks into the open tree:
#   (b) closed-vocabulary lexical scan  — no proprietary ontology terms or brand
#   (c) no board-content / research data — the tool is open, instances are not
#   (d) license scan                     — MIT LICENSE + SPDX headers present
# The clean-clone build+test (a) is a separate CI job (cargo build/test --locked).
#
# Exit 0 = clean · non-zero = a violation (message names which).

set -uo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || echo .)" || exit 1

fail() { echo "❌ [export-gate] $1" >&2; exit 1; }
SCAN_DIRS="src tests ontology themes"

# ── (b) Closed-vocabulary lexical scan ───────────────────────────────────────
# Proprietary ontology vocabulary (Y-layers · certifiability · proof types ·
# plane tags) and upstream brand names must NOT appear in the open tree.
VOCAB='y-?layer|certifiab|proof-?type|proof-carrying|neural-?triple|certifiability_class|plane-(state|work|cognition|executive)'
BRAND='\bnusy\b|nusy[-_]|nusy\.dev|\byurtle\b|noesis'
CLOSED_CRATES='nusy-arrow|graph_query|graph-query|graph_review|graph-review|nusy-conductor|nusy-cranelift|codegraph|nusy-evaluator|tutor-record'

hits=$(grep -rniE "$VOCAB|$BRAND|$CLOSED_CRATES" $SCAN_DIRS 2>/dev/null | grep -v 'export-gate' || true)
if [ -n "$hits" ]; then
    echo "$hits" | head -20 >&2
    fail "closed-vocabulary / brand / closed-crate terms found in the open tree (above)."
fi

# ── (c) No board-content or research/eval data ───────────────────────────────
# Persisted board state (Parquet), the runtime data dir, and research/eval
# corpora are instance data, not the tool — they must never be committed.
# Match INSTANCE data only — Parquet board state, the runtime data dir, and
# TOP-LEVEL research/eval/dataset corpora. (Anchored: `ontology/shapes/research/`
# holds the research-board SHACL shape DEFINITIONS — the tool, not instance data.)
badpaths=$(git ls-files 2>/dev/null | grep -iE '\.parquet$|(^|/)\.arrow-kanban/|^(research|eval|eval-data|board-content|datasets)/' || true)
if [ -n "$badpaths" ]; then
    echo "$badpaths" | head -20 >&2
    fail "board-content / research / eval / parquet data is committed (above) — instances stay private."
fi

# ── (c2) No path deps / closed-crate deps in Cargo.toml ──────────────────────
# A published FOSS crate resolves PURELY from crates.io. A `path = "../…"`
# dependency or a `nusy-*`/`noesis-*` dep would pull closed monorepo code — and
# would BUILD FINE on a machine that has the monorepo checked out adjacent, so the
# clean-clone build alone can't be trusted to catch it (EXPR-6756 finding). Scan
# the manifest directly. (`[[bin]] path = "src/main.rs"` is a target path, not a
# dependency — only parent-relative `path = "../` deps are flagged.)
if [ -f Cargo.toml ]; then
    pathdeps=$(grep -nE 'path[[:space:]]*=[[:space:]]*"\.\.' Cargo.toml || true)
    closeddeps=$(grep -nE '^[[:space:]]*(nusy-|noesis)[a-z0-9-]*[[:space:]]*=' Cargo.toml || true)
    if [ -n "$pathdeps$closeddeps" ]; then
        printf '%s\n%s\n' "$pathdeps" "$closeddeps" | grep -v '^$' | head -20 >&2
        fail "Cargo.toml has a parent-relative path dep or a closed-crate (nusy-*/noesis-*) dep (above) — the FOSS core must resolve only from crates.io."
    fi
fi

# ── (d) License scan ─────────────────────────────────────────────────────────
[ -f LICENSE ] || fail "LICENSE file is missing."
grep -qi 'MIT License' LICENSE || fail "LICENSE is not MIT."
missing_spdx=$(for f in $(git ls-files 'src/*.rs' 'tests/*.rs' 2>/dev/null); do
    head -1 "$f" | grep -q 'SPDX-License-Identifier: MIT' || echo "$f"
done)
if [ -n "$missing_spdx" ]; then
    echo "$missing_spdx" | head -20 >&2
    fail "the above .rs file(s) are missing the '// SPDX-License-Identifier: MIT' header."
fi

echo "✅ [export-gate] clean — no closed vocabulary, no instance data, license + SPDX present."
exit 0
