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
SCAN_DIRS="src tests ontology themes server/src server/tests"

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

# ── (b2) D1 move-inward concept scan (SG-6801 disposition) ───────────────────
# Fleet-governance concepts that must never re-enter the open engine. The engine
# treats TAGS as opaque data, so a bare "pending-ratification" tag STRING is fine —
# what moves inward is the enforcement GATE machinery (its identifiers), the
# Captain role/rank framing, the decision-ledger, and COG/training semantics.
# \bcaptain[_-]? (NOT \bcaptain\b) so the identifier `captain_only` is caught — `_` is a
# word char, so a trailing \b never matches after `captain` (E2 review Finding 1, DGX2).
# The tier/being/genome/ship terms cover the autonomy-tier + being-runtime config vocab.
D1_CONCEPTS='\bcaptain[_-]?|check_ratification|PENDING_TAG|body_says_pending|RatificationFinding|format_ratification|voyage_id_from_tag|decision-?ledger|training_queue|training_runner|cog-?training|being_approved|sealed_genome|being_config|ship_config|awakening|cognitive_params|autonomy[_ -]?tier'
d1hits=$(grep -rniE "$D1_CONCEPTS" $SCAN_DIRS 2>/dev/null | grep -v 'export-gate' || true)
if [ -n "$d1hits" ]; then
    echo "$d1hits" | head -20 >&2
    fail "D1 move-inward concept (Captain-rank / pending-ratification GATE / decision-ledger / training) found in the open tree (above) — these belong in fleet composition (nusy-agenda), not the open engine. Tags-as-opaque-strings are fine; the enforcement gate is not."
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
# Scan EVERY workspace manifest (root + members), not just the root — a closed dep
# in server/Cargo.toml would otherwise escape (E2 finding). A member's workspace-internal
# `path = ".."` (server → root lib) is ALLOWED: it does not escape the published
# workspace, so an escaping path is matched only when a SEGMENT follows ".." (`../x`) or
# the path names the monorepo.
for cargo in Cargo.toml server/Cargo.toml; do
    [ -f "$cargo" ] || continue
    closeddeps=$(grep -nE '^[[:space:]]*(nusy-|noesis)[a-z0-9-]*[[:space:]]*=' "$cargo" || true)
    escpaths=$(grep -nE 'path[[:space:]]*=[[:space:]]*"([^"]*(nusy-product-team|/crates/)|\.\./[^"])' "$cargo" || true)
    if [ -n "$closeddeps$escpaths" ]; then
        printf '%s\n%s\n' "$closeddeps" "$escpaths" | grep -v '^$' | head -20 >&2
        fail "$cargo has a closed-crate (nusy-*/noesis-*) dep or a path dep escaping the workspace (above) — the FOSS core must resolve only from crates.io + the workspace root."
    fi
done

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
