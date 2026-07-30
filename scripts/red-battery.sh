#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# arrow-kanban export-gate RED battery — the COMMITTED regression net.
#
# an assertion is not coverage until it has FAILED on the defect it claims to
# cover. The export gate's promise is "closed material cannot reach the open tree"; this
# battery proves it by INJECTING each closed-material vector and asserting the gate goes
# RED, then restoring. Run in CI (see ci.yml) and locally. Exit 0 = all vectors caught.
#
# It also carries the positive control for the E2-review Finding-1 pattern gap: the D1
# `captain` pattern MUST match the IDENTIFIER `captain_only` (a trailing \b never matches
# after `captain`, so `captain_only` slipped `\bcaptain\b`). If that pattern regresses,
# this battery fails before any injection.

set -uo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || echo .)" || exit 1
GATE=(bash scripts/export-gate.sh)
fail() { echo "❌ [red-battery] $1" >&2; exit 1; }
pass() { echo "✅ [red-battery] $1"; }

# 0. Baseline: the clean tree must PASS the gate.
"${GATE[@]}" >/dev/null 2>&1 || fail "clean tree does NOT pass the gate — fix the baseline first."
pass "baseline: clean tree passes the gate."

# 0b. Positive control for the widened D1 pattern (E2 review Finding 1). If the gate's
# captain pattern reverts to \bcaptain\b, `captain_only` stops matching and the leak
# returns silently — so pin the property here, not just in the gate.
grep -qE '\bcaptain[_-]?' <<<'captain_only'      || fail "D1 pattern regressed: no longer matches 'captain_only' (the Finding-1 gap)."
grep -qE '\bcaptain[_-]?' <<<'the captain ruled' || fail "D1 pattern regressed: no longer matches the word 'captain'."
pass "positive control: the D1 captain pattern matches BOTH 'captain_only' and 'captain'."

# 0c. Positive controls for the provenance scan. Two properties must BOTH hold, and
# they pull in opposite directions — so pin each one:
#   (a) it MUST catch a real tracker citation in a comment;
#   (b) it MUST NOT catch this tool's own ID grammar or its test fixtures, because a
#       gate that fires on the product working correctly gets switched off.
PROV_ID_CTL='\b(CH|EX|EXP|VY|VOY|SG|PROP|EXPR|HZ|CHORE|PAPER)-[0-9]{4,}(\.[0-9]+)?\b'
PROV_OK_CTL='\b(CH|EX|EXP|VY|VOY|SG|PROP|EXPR|HZ|CHORE|PAPER)-(42|1234|1235|1240)(\.[0-9]+)?\b'
prov_test() { sed -E "s/$PROV_OK_CTL//g" <<<"$1" | grep -qE "$PROV_ID_CTL"; }
prov_test '// CH-6109: a real citation'          || fail "provenance scan regressed: misses a real tracker citation."
prov_test '// e.g. EX-1234 shows the format'     && fail "provenance scan regressed: fires on a synthetic doc example."
prov_test '    let b = items(&["EXP-1", "VY-8"]);' && fail "provenance scan regressed: fires on test fixture IDs."
# a line carrying BOTH an example and a real ref must STILL fail — the allowlist strips
# refs, it never excuses the whole line (that would let a leak ride along on an example).
prov_test '// e.g. EX-1234, introduced by CH-6109' || fail "provenance scan regressed: an example masks a real citation on the same line."
pass "positive controls: provenance scan catches citations, spares the ID grammar + fixtures, and is not masked by an example."

# helper: inject, assert the gate goes RED, restore, and confirm the file is byte-identical.
mutate_expect_red() {
    local desc="$1" file="$2" inject="$3" bak
    bak=$(mktemp)
    cp "$file" "$bak"
    printf '%s' "$inject" >> "$file"
    if "${GATE[@]}" >/dev/null 2>&1; then
        cp "$bak" "$file"; rm -f "$bak"
        fail "$desc — gate stayed GREEN on the injection (VACUOUS gate)."
    fi
    cp "$bak" "$file"; rm -f "$bak"
    pass "$desc — gate goes RED."
}

mutate_expect_red "closed vocabulary -> server/src"                server/src/lib.rs   $'\n// certifiability_class plane-cognition\n'
mutate_expect_red "D1 gate identifier -> server/src"               server/src/lib.rs   $'\n// check_ratification_consistency\n'
mutate_expect_red "D1 fleet vocab (captain_only) -> server/src"    server/src/lib.rs   $'\n// tier: captain_only\n'
mutate_expect_red "closed-crate dep -> server/Cargo.toml"          server/Cargo.toml   $'\nnusy-graph-review = "0.15"\n'
mutate_expect_red "monorepo path-escape dep -> server/Cargo.toml"  server/Cargo.toml   $'\nx = { path = "../nusy-product-team/crates/x" }\n'
mutate_expect_red "fleet-roster list -> server/src"                server/src/lib.rs   $'\n// default agents: DGX,M5,Mini\n'
mutate_expect_red "provenance ticket ref in a comment"            server/src/lib.rs   $'\n// CH-6109: typed edges landed here\n'
mutate_expect_red "upstream CLI alias in a comment"               server/src/lib.rs   $'\n// run `nk show <ID>` to inspect\n'
mutate_expect_red "upstream contributor guide in a comment"       server/src/lib.rs   $'\n// see CLAUDE.md for the guardrail\n'

# final: tree restored + gate green again.
"${GATE[@]}" >/dev/null 2>&1 || fail "gate is not green after restore — a mutation leaked."
echo "✅ [red-battery] all 9 injections RED; clean tree green; positive controls hold."
