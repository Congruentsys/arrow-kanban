#!/usr/bin/env python3
"""Semantic-search recall@k eval — the hand-labeled query set behind the
README's recall/MRR numbers. Run against a board populated by
eval_populate_board.py.

Usage:
  python3 scripts/eval_populate_board.py fastembed /tmp/eval-board
  python3 scripts/eval_recall.py fastembed /tmp/eval-board
"""

import json
import os
import subprocess
import sys

BIN = os.environ.get(
    "ARROW_KANBAN_BIN",
    os.path.join(os.path.dirname(__file__), "..", "target", "release", "arrow-kanban"),
)

# Hand-labeled by reading the real arrow-kanban issue titles. Ground truth is
# deliberately phrased with low lexical overlap with the target title, so
# recall reflects semantic match rather than substring/keyword overlap.
QUERIES = [
    ("duplicate typed relationship", [51]),
    ("reopening a closed item does not clear its old resolution", [65]),
    ("published event does not record which agent performed the action", [92, 98, 71]),
    ("internal tracker reference leaked into the public open-source tree", [44, 3, 8, 47, 13, 11]),
    ("add a new predicate to the vocabulary without restarting the server", [73, 36]),
    ("import command destroys the existing board with no confirmation", [62, 63]),
    ("a typed edge that was written cannot be read back", [26, 78]),
    ("semantic search over kanban items using embeddings", [104]),
    ("counter for how many times an item has been requeued", [80, 40]),
    ("two agents claiming the same item at the same time is not atomic", [76]),
    ("roadmap view mixes labelled and unlabelled groups inconsistently", [69]),
    ("README has no section listing known rough edges", [67]),
]


def run_query(root, provider, text, top_k=10):
    result = subprocess.run(
        [BIN, "--root", root, "query", "--semantic", text,
         "--top", str(top_k), "--json", "--embedding-provider", provider],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        print(f"query failed: {result.stderr}", file=sys.stderr)
        return []
    return [row["id"] for row in json.loads(result.stdout)]


def main():
    if len(sys.argv) < 2:
        print(f"usage: {sys.argv[0]} <hash|fastembed> [board-root]", file=sys.stderr)
        sys.exit(2)
    provider = sys.argv[1]
    root = sys.argv[2] if len(sys.argv) > 2 else "/tmp/arrow-kanban-eval-board"

    with open(os.path.join(root, "gh_to_kanban.json")) as f:
        gh_to_kanban = {int(k): v for k, v in json.load(f).items()}

    hit5 = hit10 = 0
    rr_sum = 0.0
    per_query = []
    for text, relevant_gh in QUERIES:
        relevant_ids = {gh_to_kanban[n] for n in relevant_gh if n in gh_to_kanban}
        ranked = run_query(root, provider, text)
        rank = next((i + 1 for i, rid in enumerate(ranked) if rid in relevant_ids), None)
        in5 = rank is not None and rank <= 5
        in10 = rank is not None and rank <= 10
        hit5 += int(in5)
        hit10 += int(in10)
        rr_sum += (1.0 / rank) if rank else 0.0
        per_query.append({
            "query": text, "relevant_gh_issues": relevant_gh,
            "top10": ranked, "first_relevant_rank": rank,
            "hit_at_5": in5, "hit_at_10": in10,
        })

    n = len(QUERIES)
    out = {
        "provider": provider,
        "n_queries": n,
        "recall_at_5": hit5 / n,
        "recall_at_10": hit10 / n,
        "mean_reciprocal_rank": rr_sum / n,
        "per_query": per_query,
    }
    print(json.dumps(out, indent=2))


if __name__ == "__main__":
    main()
