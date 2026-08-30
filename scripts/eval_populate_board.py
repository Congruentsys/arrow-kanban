#!/usr/bin/env python3
"""Populate a scratch arrow-kanban board with real repo GitHub issue titles —
the corpus for the semantic-search recall@k eval (see eval_recall.py).

Usage:
  cargo build --release --features fastembed-backend
  python3 scripts/eval_populate_board.py <hash|fastembed> [board-root]

Requires the `gh` CLI, authenticated against a repo with read access to the
issue list (defaults to the upstream github.com/Congruentsys/arrow-kanban).
"""

import json
import os
import re
import subprocess
import sys

REPO = os.environ.get("EVAL_REPO", "Congruentsys/arrow-kanban")
BIN = os.environ.get(
    "ARROW_KANBAN_BIN",
    os.path.join(os.path.dirname(__file__), "..", "target", "release", "arrow-kanban"),
)


def fetch_issues(limit=100):
    result = subprocess.run(
        ["gh", "issue", "list", "--repo", REPO, "--state", "all",
         "--limit", str(limit), "--json", "number,title,body"],
        capture_output=True, text=True, check=True,
    )
    return json.loads(result.stdout)


def main():
    if len(sys.argv) < 2:
        print(f"usage: {sys.argv[0]} <hash|fastembed> [board-root]", file=sys.stderr)
        sys.exit(2)
    provider = sys.argv[1]
    root = sys.argv[2] if len(sys.argv) > 2 else "/tmp/arrow-kanban-eval-board"

    os.makedirs(root, exist_ok=True)
    subprocess.run([BIN, "--root", root, "init", "--theme", "software"],
                    capture_output=True, text=True)

    issues = fetch_issues()
    mapping = {}
    for issue in issues:
        title = issue["title"]
        body = (issue.get("body") or "")[:500]
        result = subprocess.run(
            [BIN, "--root", root, "create", "chore", title,
             "--body", body, "--embedding-provider", provider],
            capture_output=True, text=True,
        )
        m = re.search(r"Created (\S+):", result.stdout)
        if not m:
            print(f"FAILED for issue #{issue['number']}: {result.stdout}{result.stderr}",
                  file=sys.stderr)
            continue
        mapping[issue["number"]] = m.group(1)

    with open(os.path.join(root, "gh_to_kanban.json"), "w") as f:
        json.dump(mapping, f, indent=2)

    print(f"Created {len(mapping)} items at {root} with provider={provider}")


if __name__ == "__main__":
    main()
