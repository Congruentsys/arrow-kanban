#!/bin/bash
# build-board.sh — mine a full export from a running kanban server and render the
# interactive triage board (see render-board.py for the page itself).
#
# Usage:
#   KANBAN_CLI="nusy-kanban --server nats://host:4222" \
#     scripts/triage-board/build-board.sh [OUT_DIR] [--ranks "tag-a,tag-b,…"] [--title "…"]
#
#   # or render an existing export without touching a server:
#   scripts/triage-board/build-board.sh board.json OUT_DIR --ranks "…"
#
# KANBAN_CLI is any client exposing `export --format json --offset N --limit N`
# (server-side pagination). The rank list is data the caller injects — this tool
# carries no deployment-specific vocabulary.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"

if [ $# -ge 1 ] && [ -f "$1" ] && [[ "$1" == *.json ]]; then
  SRC="$1"; shift
  OUT="${1:?out dir}"; shift
  mkdir -p "$OUT"
  python3 "$HERE/render-board.py" "$SRC" "$OUT" "$@"
  exit 0
fi

OUT="${1:-${TMPDIR:-/tmp}/triage-board}"; [ $# -ge 1 ] && shift
mkdir -p "$OUT"
: "${KANBAN_CLI:?set KANBAN_CLI to the export-capable client invocation}"

PAGE=300 off=0
echo "[" > "$OUT/export.json"
first=1
while :; do
  chunk="$($KANBAN_CLI export --format json --offset "$off" --limit "$PAGE" 2>/dev/null)" || {
    echo "[board] export failed at offset $off" >&2; exit 1; }
  n="$(python3 -c 'import json,sys; print(len(json.load(sys.stdin)))' <<<"$chunk")"
  if [ "$n" -gt 0 ]; then
    body="$(python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin))[1:-1])' <<<"$chunk")"
    [ "$first" = 1 ] || printf ',' >> "$OUT/export.json"
    printf '%s' "$body" >> "$OUT/export.json"
    first=0
  fi
  [ "$n" -lt "$PAGE" ] && break
  off=$((off + PAGE))
done
echo "]" >> "$OUT/export.json"
echo "[board] export complete ($(wc -c < "$OUT/export.json") bytes)"
python3 "$HERE/render-board.py" "$OUT/export.json" "$OUT" "$@"
