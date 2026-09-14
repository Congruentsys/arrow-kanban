#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Embedding-backend bake-off driver: runs every arm of
examples/embedding_bakeoff.rs serially, repeats each arm, checks that the arms
compute the same function, measures retrieval quality through the existing
recall harness, and writes one summary JSON. See docs/embedding-bakeoff.md.

Build both binaries first (the CUDA build needs a CUDA toolchain with nvcc):

  cargo build --release --features fastembed-backend,candle-backend \\
      --example embedding_bakeoff --bin arrow-kanban
  cargo build --release --features fastembed-backend,candle-backend,candle-transformers/cuda \\
      --example embedding_bakeoff --bin arrow-kanban --target-dir target/cuda

Then:

  python3 scripts/embedding_bakeoff.py --corpus items.jsonl --out-dir /tmp/bakeoff \\
      --issues-json issues.json [--arms fastembed,fastembed-fp32,candle,candle-cuda] [--runs 3]

The corpus is JSON Lines, one {"text": "..."} per item: use your own board's
item text (title, a blank line, then the body, which is what `create` embeds).
--issues-json is a frozen `gh issue list --json number,title,body` snapshot for
scripts/eval_recall.py; without it the recall step is skipped.

Arms run one at a time. Before each run the driver waits until no cargo/rustc
process is running, and before a CUDA run until no compute process is on the
GPU: a measurement taken beside a build or another GPU job is not a measurement.
"""

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
CUDA_ARMS = {"candle-cuda"}
BATCH_SIZES = [1, 16, 64, 256]


def utc_now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def build_process_count():
    out = subprocess.run(["ps", "-eo", "comm"], capture_output=True, text=True).stdout
    return sum(1 for line in out.splitlines() if line.strip() in ("cargo", "rustc"))


def gpu_compute_apps():
    result = subprocess.run(
        ["nvidia-smi", "--query-compute-apps=pid,process_name,used_memory",
         "--format=csv,noheader,nounits"],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"nvidia-smi failed: {result.stderr.strip()}")
    return [line.strip() for line in result.stdout.splitlines()
            if line.strip() and not line.startswith("No running")]


def preflight(needs_gpu, timeout_s):
    """Wait for a quiet host; return what was observed. Raises on timeout."""
    start = time.time()
    while True:
        builds = build_process_count()
        apps = gpu_compute_apps() if needs_gpu else []
        if builds == 0 and not apps:
            return {"checked_at": utc_now(), "build_processes": builds,
                    "gpu_compute_apps": apps if needs_gpu else None,
                    "waited_s": round(time.time() - start, 1)}
        if time.time() - start > timeout_s:
            raise RuntimeError(
                f"host not quiet after {timeout_s}s: {builds} cargo/rustc, GPU apps {apps}")
        time.sleep(15)


def gpu_event_reason_counters():
    """Cumulative clock-event-reason counters (microseconds), e.g. SW power capping.

    An instantaneous `clocks_event_reasons` sample cannot show a cap that only
    applies under load; the cumulative counters, read before and after a GPU run,
    can. Returns {} when the driver does not report them.
    """
    result = subprocess.run(["nvidia-smi", "-q", "-d", "PERFORMANCE"],
                            capture_output=True, text=True)
    if result.returncode != 0:
        return {}
    counters, in_section = {}, False
    for line in result.stdout.splitlines():
        if "Clocks Event Reasons Counters" in line:
            in_section = True
            continue
        if in_section:
            match = re.match(r"\s+([A-Za-z][A-Za-z ]+?)\s+:\s+(\d+)\s*us", line)
            if match:
                counters[match.group(1).strip()] = int(match.group(2))
            elif line.strip() and not line.startswith(" " * 8):
                break
    return counters


def binaries_for(arm, args):
    build = args.gpu_build_dir if arm in CUDA_ARMS else args.cpu_build_dir
    return (os.path.join(build, "examples", "embedding_bakeoff"),
            os.path.join(build, "arrow-kanban"))


def host_info():
    info = {"uname": " ".join(platform.uname()), "cpu_count": os.cpu_count()}
    try:
        gpu = subprocess.run(
            ["nvidia-smi", "--query-gpu=name,driver_version,compute_cap",
             "--format=csv,noheader"], capture_output=True, text=True)
        if gpu.returncode == 0:
            info["gpu"] = gpu.stdout.strip()
    except FileNotFoundError:
        info["gpu"] = None
    return info


def model_cache_listing():
    """Size and mtime of every file under HF_HOME, to show no download happened mid-run."""
    root = os.environ.get("HF_HOME")
    if not root or not os.path.isdir(root):
        return None
    listing = {}
    for dirpath, _, files in os.walk(root):
        for name in files:
            path = os.path.join(dirpath, name)
            if os.path.islink(path):
                continue
            stat = os.stat(path)
            listing[os.path.relpath(path, root)] = [stat.st_size, int(stat.st_mtime)]
    return listing


def run_arm(arm, index, args):
    bench, _ = binaries_for(arm, args)
    out = os.path.join(args.out_dir, f"run-{arm}-{index}.json")
    log = os.path.join(args.out_dir, f"run-{arm}-{index}.log")
    dump = os.path.join(args.out_dir, f"emb-{arm}.f32")
    for stale in (out, log):
        if os.path.exists(stale):
            os.remove(stale)
    record = {"arm": arm, "run": index,
              "preflight": preflight(arm in CUDA_ARMS, args.idle_timeout_s)}
    cmd = [bench, "--arm", arm, "--corpus", args.corpus, "--out", out,
           "--min-gpu-util", str(args.min_gpu_util)]
    if index == 1:
        cmd += ["--dump-embeddings", dump]
    record["command"] = cmd
    if arm in CUDA_ARMS:
        record["gpu_event_reason_counters_before_us"] = gpu_event_reason_counters()
    with open(log, "w") as f:
        rc = subprocess.run(cmd, stdout=f, stderr=subprocess.STDOUT).returncode
    if arm in CUDA_ARMS:
        after = gpu_event_reason_counters()
        before = record["gpu_event_reason_counters_before_us"]
        record["gpu_event_reason_counters_after_us"] = after
        record["gpu_event_reason_counters_delta_us"] = {
            key: after[key] - before[key] for key in after if key in before}
    record["rc"] = rc
    record["log"] = log
    if rc == 0 and os.path.exists(out):
        with open(out) as f:
            record["result"] = json.load(f)
    else:
        with open(log) as f:
            record["failure"] = f.read()[-2000:]
    print(f"  {arm} run {index}: rc={rc}", flush=True)
    return record


def parity(arms, args):
    import numpy as np

    loaded = {}
    for arm in arms:
        path = os.path.join(args.out_dir, f"emb-{arm}.f32")
        if os.path.exists(path):
            loaded[arm] = np.fromfile(path, dtype="<f4").reshape(-1, 384)
    pairs = {}
    names = sorted(loaded)
    for i, a in enumerate(names):
        for b in names[i + 1:]:
            x, y = loaded[a], loaded[b]
            if x.shape != y.shape:
                pairs[f"{a}|{b}"] = {"error": f"shape {x.shape} vs {y.shape}"}
                continue
            cos = (x * y).sum(1) / (np.linalg.norm(x, axis=1) * np.linalg.norm(y, axis=1))
            pairs[f"{a}|{b}"] = {"n": int(cos.size), "mean": float(cos.mean()),
                                 "min": float(cos.min()),
                                 "p01": float(np.percentile(cos, 1))}
    return pairs


def recall(provider, args):
    cli = binaries_for(provider, args)[1]
    board = os.path.join(args.out_dir, f"recall-board-{provider}")
    if os.path.isdir(board):
        shutil.rmtree(board)
    env = dict(os.environ, ARROW_KANBAN_BIN=cli, EVAL_ISSUES_JSON=args.issues_json)
    with open(args.issues_json) as f:
        expected = len(json.load(f))
    record = {"provider": provider,
              "preflight": preflight(provider in CUDA_ARMS, args.idle_timeout_s)}
    populate = subprocess.run(
        [sys.executable, os.path.join(REPO, "scripts", "eval_populate_board.py"), provider, board],
        capture_output=True, text=True, env=env)
    created = re.search(r"Created (\d+) items", populate.stdout)
    record["created"] = int(created.group(1)) if created else 0
    record["populate_stderr_tail"] = populate.stderr[-2000:]
    # `create` stores an item WITHOUT an embedding when the backend fails (it only
    # warns), which would silently degrade recall. `embed` reports how many items
    # still lack one: anything but 0 invalidates this provider's recall.
    backfill = subprocess.run(
        [cli, "--root", board, "embed", "--embedding-provider", provider],
        capture_output=True, text=True, env=env)
    missing = re.search(r"Embedding (\d+) item", backfill.stdout)
    record["items_missing_embedding"] = int(missing.group(1)) if missing else None
    if record["created"] != expected or record["items_missing_embedding"] != 0:
        record["valid"] = False
        record["backfill_output"] = (backfill.stdout + backfill.stderr)[-2000:]
        return record
    evaluated = subprocess.run(
        [sys.executable, os.path.join(REPO, "scripts", "eval_recall.py"), provider, board],
        capture_output=True, text=True, env=env)
    record["rc"] = evaluated.returncode
    record["valid"] = evaluated.returncode == 0
    if evaluated.returncode == 0:
        record["result"] = json.loads(evaluated.stdout)
    else:
        record["stderr_tail"] = evaluated.stderr[-2000:]
    return record


def spread(values):
    values = [v for v in values if v is not None]
    if not values:
        return None
    return {"median": statistics.median(values), "min": min(values), "max": max(values),
            "values": values}


def aggregate(records):
    ok = [r["result"] for r in records if r.get("rc") == 0 and "result" in r]
    if not ok:
        return {"successful_runs": 0}

    def batch(result, size):
        return next((b["items_per_s"] for b in result["batches"] if b["batch_size"] == size), None)

    out = {
        "successful_runs": len(ok),
        "device": sorted({r["device"] for r in ok}),
        "load_ms": spread([r["load_ms"] for r in ok]),
        "first_call_ms": spread([r["first_call_ms"] for r in ok]),
        "items_per_s": {str(size): spread([batch(r, size) for r in ok]) for size in BATCH_SIZES},
        "batch1_latency_ms": {key: spread([r["batch1_latency_ms"][key] for r in ok])
                              for key in ("mean", "p50", "p95", "p99")},
        "peak_rss_kb": spread([r["rss_kb"]["peak"] for r in ok]),
        "write_cost_ms": spread([r["load_ms"] + r["batch1_latency_ms"]["p50"] for r in ok]),
    }
    gpu = [r["gpu"]["measured_passes"] for r in ok if r.get("gpu")]
    if gpu:
        def metric(name, stat):
            return spread([g[name].get(stat) for g in gpu if isinstance(g.get(name), dict)])
        out["gpu_measured_passes"] = {
            "utilization_gpu_pct": {"min": metric("utilization_gpu_pct", "min"),
                                    "p50": metric("utilization_gpu_pct", "p50"),
                                    "max": metric("utilization_gpu_pct", "max")},
            "clocks_current_graphics_mhz": {"min": metric("clocks_current_graphics_mhz", "min"),
                                            "p50": metric("clocks_current_graphics_mhz", "p50"),
                                            "max": metric("clocks_current_graphics_mhz", "max")},
            "power_draw_w": {"min": metric("power_draw_w", "min"),
                             "p50": metric("power_draw_w", "p50"),
                             "max": metric("power_draw_w", "max")},
            "memory_used_mib": {"p50": metric("memory_used_mib", "p50")},
            "own_process_used_memory_mib": [r["gpu"].get("own_process_used_memory_mib")
                                            for r in ok if r.get("gpu")],
        }
    return out


def fmt(spread_value, digits=1):
    if not spread_value:
        return "n/a"
    return (f"{spread_value['median']:.{digits}f} "
            f"({spread_value['min']:.{digits}f}-{spread_value['max']:.{digits}f})")


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--corpus", required=True)
    parser.add_argument("--out-dir", required=True)
    parser.add_argument("--arms", default="fastembed,fastembed-fp32,candle,candle-cuda")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--issues-json")
    parser.add_argument("--cpu-build-dir", default=os.path.join(REPO, "target", "release"))
    parser.add_argument("--gpu-build-dir",
                        default=os.path.join(REPO, "target", "cuda", "release"))
    parser.add_argument("--min-gpu-util", type=float, default=10.0)
    parser.add_argument("--idle-timeout-s", type=int, default=1800)
    args = parser.parse_args()
    args.corpus = os.path.abspath(args.corpus)
    arms = [a.strip() for a in args.arms.split(",") if a.strip()]
    os.makedirs(args.out_dir, exist_ok=True)

    binaries = {}
    for arm in arms:
        for path in binaries_for(arm, args):
            if not os.path.exists(path):
                sys.exit(f"missing binary {path}; build it first (see --help)")
            binaries[path] = sha256_file(path)

    summary = {
        "schema": "arrow-kanban/embedding-bakeoff-summary/1",
        "started_at": utc_now(),
        "host": host_info(),
        "binaries_sha256": binaries,
        "corpus": {"items": sum(1 for line in open(args.corpus) if line.strip()),
                   "bytes": os.path.getsize(args.corpus), "sha256": sha256_file(args.corpus)},
        "arms": arms,
        "runs_per_arm": args.runs,
        "model_cache_before": model_cache_listing(),
    }

    records = {arm: [] for arm in arms}
    for arm in arms:
        print(f"arm {arm}", flush=True)
        for index in range(1, args.runs + 1):
            records[arm].append(run_arm(arm, index, args))

    summary["model_cache_after_throughput"] = model_cache_listing()
    summary["model_cache_unchanged"] = (
        summary["model_cache_before"] == summary["model_cache_after_throughput"])
    summary["runs"] = records
    summary["aggregate"] = {arm: aggregate(records[arm]) for arm in arms}
    summary["parity"] = parity(arms, args)

    if args.issues_json:
        summary["recall_corpus"] = {"issues_json_sha256": sha256_file(args.issues_json)}
        summary["recall"] = {}
        for provider in ["hash"] + arms:
            print(f"recall {provider}", flush=True)
            summary["recall"][provider] = recall(provider, args)

    summary["finished_at"] = utc_now()
    out_path = os.path.join(args.out_dir, "bakeoff-summary.json")
    with open(out_path, "w") as f:
        json.dump(summary, f, indent=2)

    print(f"\n| arm | device | load ms | batch-1 p50 ms | batch-1 p99 ms | items/s b1 | b16 | "
          f"b64 | b256 | peak RSS MiB | recall@10 | MRR |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for arm in arms:
        agg = summary["aggregate"][arm]
        if not agg.get("successful_runs"):
            print(f"| {arm} | FAILED | | | | | | | | | | |")
            continue
        rec = summary.get("recall", {}).get(arm, {}).get("result", {})
        rss = agg["peak_rss_kb"]
        rss_mib = {k: rss[k] / 1024 for k in ("median", "min", "max")} if rss else None
        print(f"| {arm} | {','.join(agg['device'])} | {fmt(agg['load_ms'], 0)} | "
              f"{fmt(agg['batch1_latency_ms']['p50'], 2)} | {fmt(agg['batch1_latency_ms']['p99'], 2)} | "
              + " | ".join(fmt(agg["items_per_s"][str(s)], 0) for s in BATCH_SIZES)
              + f" | {fmt(rss_mib, 0)} | {rec.get('recall_at_10', 'n/a')} | "
              f"{rec.get('mean_reciprocal_rank', 'n/a')} |")
    print(f"\nsummary: {out_path}")


if __name__ == "__main__":
    main()
