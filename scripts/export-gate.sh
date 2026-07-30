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

# ── (b2) Move-inward concept scan ────────────────────────────────────────────
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

# ── (b3) Fleet-roster scan (E2 review, Air finding) ─────────────────────────
# A hardcoded comma-list of the fleet's agent names (e.g. a `--agents` default of
# "DGX,M5,Mini") is fleet-specific data — the open engine derives agents from board
# DATA. A SINGLE agent name as an assignee VALUE is fine (opaque data), so match a
# comma/semicolon-separated list of TWO+ roster names, never a lone name.
ROSTER='\b(DGX[12]?|M5|Mini|Air)\b *[,;] *\b(DGX[12]?|M5|Mini|Air)\b'
rosterhits=$(grep -rniE "$ROSTER" $SCAN_DIRS 2>/dev/null | grep -vE 'export-gate|red-battery' || true)
if [ -n "$rosterhits" ]; then
    echo "$rosterhits" | head -20 >&2
    fail "a hardcoded fleet-roster list (two+ agent names comma-separated) is in the open tree (above) — the open engine derives agents from board DATA, never a baked-in roster."
fi

# ── (b4) Provenance-reference scan (CH-6825) ────────────────────────────────
# Internal tracker IDs and the upstream CLI alias must not appear in the COMMENTS of
# the public tree: a reader outside the origin project cannot resolve "CH-6109", so
# it is noise that leaks process without informing anyone.
#
# Scanned on COMMENT LINES ONLY, deliberately. The ID grammar itself is this tool's
# OWN data model — arrow-kanban items ARE named `EX-1234` — and test fixtures
# legitimately carry IDs like "EXP-1" on code lines. A gate that flagged those would
# fire on the product working correctly.
#
# Synthetic doc examples stay legal, allowlisted BY NUMBER so docs can still show the
# format. Allowlisted refs are STRIPPED before the test rather than used to exclude the
# line: a line carrying BOTH a synthetic example and a real ticket ref must still fail
# (excluding the whole line would let a real leak ride along on an example — coverage
# that is not coverage).
# Four-or-more digits only: the origin tracker allocates from 3001 up, while this
# repo's test fixtures use one- and two-digit IDs (EX-1, VY-8) in their explanatory
# comments. Requiring 4+ digits keeps the gate off the product's own tests. A
# three-digit citation would slip; that is the deliberate trade — a gate that fires
# on legitimate fixtures gets disabled, which protects nothing.
PROV_ID='\b(CH|EX|EXP|VY|VOY|SG|PROP|EXPR|HZ|CHORE|PAPER)-[0-9]{4,}(\.[0-9]+)?\b'
PROV_OK='\b(CH|EX|EXP|VY|VOY|SG|PROP|EXPR|HZ|CHORE|PAPER)-(42|1234|1235|1240)(\.[0-9]+)?\b'
provhits=$(grep -rnE "$PROV_ID" $SCAN_DIRS 2>/dev/null \
    | grep -E ':[0-9]+:[[:space:]]*(//|#|\*)' \
    | grep -vE 'export-gate|red-battery' \
    | sed -E "s/$PROV_OK//g" \
    | grep -E "$PROV_ID" || true)
if [ -n "$provhits" ]; then
    echo "$provhits" | head -20 >&2
    fail "internal tracker reference(s) in comments of the open tree (above) — strip the citation and keep the rationale; synthetic doc examples (-42/-1234) are allowed."
fi

# The upstream shell alias and the upstream contributor guide are project-internal.
# ALL LINES, not just comments (E2-followup review, DGX1). The leaks that actually reach a
# user are in PRINTED STRINGS — `format!("nk update ...")` was the copy-paste remediation the
# CLI handed people, while the doc comment four lines above already said `arrow-kanban`. A
# comment-only scan is green in exactly that case. `\bnk ` cannot match inside a word
# ("I think about it" does not match), so all-lines costs no false positives here.
aliashits=$(grep -rnE '(\bCLAUDE\b|\bnk [a-z][a-z-]*\b)' $SCAN_DIRS 2>/dev/null \
    | grep -vE 'export-gate|red-battery' || true)
if [ -n "$aliashits" ]; then
    echo "$aliashits" | head -20 >&2
    fail "upstream alias/guide reference (\`nk <cmd>\` or CLAUDE) in the open tree (above) — the public binary is \`arrow-kanban\`."
fi

# ── (b5) Tracker IDs inside user-facing PROSE (all lines) ───────────────────
# (b4) covers comments. It cannot see a citation inside a STRING LITERAL that the binary
# PRINTS — e.g. `eprintln!("... See HZ-6053.")`, which told a public user to consult an ID
# they cannot resolve. Extending (b4) to all lines is not an option: this repo's tests are
# full of legitimate ID DATA (`"id": "EX-3001"`, `["EX-1300","CH-1301"]`), and a gate that
# fires on the product's own fixtures gets switched off.
#
# The discriminator is PROSE ADJACENCY: a citation sits next to words ("See HZ-6053",
# "planned for VY-3009"), whereas fixture data is a bare quoted token. Requiring a
# lowercase word immediately before the ID separates them — verified in both directions by
# the red battery, which asserts it catches a printed citation AND spares four real
# fixture shapes taken from this tree.
PROSE_ID="[a-z]{2,} +$PROV_ID"
prosehits=$(grep -rnE "$PROSE_ID" $SCAN_DIRS 2>/dev/null \
    | grep -vE 'export-gate|red-battery' \
    | sed -E "s/$PROV_OK//g" \
    | grep -E "$PROSE_ID" || true)
if [ -n "$prosehits" ]; then
    echo "$prosehits" | head -20 >&2
    fail "tracker reference(s) inside user-facing prose (above) — a printed string must not cite an ID the reader cannot resolve."
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
# clean-clone build alone can't be trusted to catch it. Scan
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
missing_spdx=$(for f in $(git ls-files 'src/*.rs' 'tests/*.rs' 'server/src/*.rs' 'server/tests/*.rs' 2>/dev/null); do
    head -1 "$f" | grep -q 'SPDX-License-Identifier: MIT' || echo "$f"
done)
if [ -n "$missing_spdx" ]; then
    echo "$missing_spdx" | head -20 >&2
    fail "the above .rs file(s) are missing the '// SPDX-License-Identifier: MIT' header."
fi

echo "✅ [export-gate] clean — no closed vocabulary, no instance data, license + SPDX present."
exit 0
