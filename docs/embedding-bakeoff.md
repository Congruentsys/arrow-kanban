# Embedding-backend bake-off

Which embedding backend should `arrow-kanban` ship as its default, and does a GPU path earn its
dependency? This page records how the question was measured and how to reproduce it.

## Arms

All four arms serve the same model, `all-MiniLM-L6-v2` (384 dimensions). They also share the same
text processing: 512-token truncation, padding to the longest text in the batch, mean pooling over
the attention mask, and L2 normalisation. They differ only in precision, runtime and device, so any
pair of arms isolates one of those factors.

| arm | provider name | runtime | precision | device |
|---|---|---|---|---|
| A | `fastembed` | ONNX Runtime (fastembed-rs) | int8, dynamic quantisation (`AllMiniLML6V2Q`) | CPU |
| B | `fastembed-fp32` | ONNX Runtime (fastembed-rs) | FP32 (`AllMiniLML6V2`) | CPU |
| C | `candle-cuda` | candle | FP32 (`sentence-transformers/all-MiniLM-L6-v2` safetensors) | CUDA |
| D | `candle` | candle | FP32 (same weights as C) | CPU |

- A vs B isolates quantisation.
- B vs D isolates the runtime on the CPU.
- D vs C isolates the device.
- A parity check confirms the arms really compute the same function: the mean and minimum cosine
  between their outputs on the same texts.

## What is measured, per arm

Each repetition is a fresh process, because model load time and peak memory are per-process
quantities. Arms run one at a time, and each arm runs at least three times.

- **Cold model load**, with the model already cached. Download time is never included: measured runs
  point `HF_ENDPOINT` at an unreachable address, so an attempted download fails the run instead of
  inflating the number.
- **First-call latency**, the first embedding after load.
- **Throughput** in items/s at batch sizes 1, 16, 64 and 256, each one pass over the corpus after one
  unmeasured warm-up batch.
- **Per-item latency at batch 1**: mean, p50, p95 and p99. `create` embeds one item per write, so this
  is the write-path number.
- **Peak process RSS** (`VmHWM`).
- **For the CUDA arm:** `nvidia-smi` utilisation, memory, graphics clock and power, sampled every
  500 ms during the measured passes, plus the process's GPU memory where the driver reports it.
- **Retrieval quality**: recall@5, recall@10 and MRR through the existing harness
  (`scripts/eval_populate_board.py` + `scripts/eval_recall.py`) and its 12 hand-labelled queries over
  this repository's public GitHub issues.

## A GPU arm must prove it ran on the GPU

The candle backend never falls back to the CPU. `--embedding-provider candle-cuda` on a build without
CUDA kernels, or on a host with no visible GPU, is an error.

Before the harness writes a result for the CUDA arm, three checks must pass:

- no other compute process was on the GPU when the run started;
- the backend reported a `cuda:N` device, read back from candle rather than echoed from the request;
- sampled GPU utilisation during the measured passes reached at least 10%.

Any failed check exits non-zero with no result file, so a CPU run can never be recorded as a GPU run.

The recall step also checks that every item on the evaluation board actually received an embedding.
`create` only warns when a backend fails, and a silently skipped embedding would otherwise degrade
recall without any error.

## Reproduce

The corpus for throughput and latency is your own board's item text, one JSON object per line:
`{"text": "<title>\n\n<body>"}`, which is exactly what `create` embeds. Keep it outside the repository;
board content is instance data. The quality step uses a frozen snapshot of this repository's issues:

```bash
gh issue list --repo Congruentsys/arrow-kanban --state all --limit 300 \
    --json number,title,body > /tmp/issues.json
```

Build both binaries. The CUDA build needs a CUDA toolchain with `nvcc`. On aarch64 Linux, candle's
matrix kernels need the `fp16` target feature at compile time; give both builds the same flags so the
arms stay comparable.

```bash
export RUSTFLAGS="-C target-feature=+fp16"      # aarch64 Linux only
cargo build --release --features fastembed-backend,candle-backend \
    --example embedding_bakeoff --bin arrow-kanban
cargo build --release --features fastembed-backend,candle-backend,candle-transformers/cuda \
    --example embedding_bakeoff --bin arrow-kanban --target-dir target/cuda
```

CUDA is enabled through candle's own feature rather than a feature of this crate. CI builds with
`--all-features` on runners that have no CUDA toolchain, and candle compiles its CUDA kernels with
`nvcc` at build time.

Pre-cache the models with network access, then run every arm with the endpoint unreachable:

```bash
export HF_HOME=/tmp/bakeoff-models
for arm in fastembed fastembed-fp32 candle candle-cuda; do     # unmeasured, downloads once
    dir=target/release; [ "$arm" = candle-cuda ] && dir=target/cuda/release
    "$dir/examples/embedding_bakeoff" --arm "$arm" --corpus items.jsonl \
        --out /tmp/precache-$arm.json --batch-sizes 1
done
HF_ENDPOINT=http://127.0.0.1:9 python3 scripts/embedding_bakeoff.py \
    --corpus items.jsonl --issues-json /tmp/issues.json --out-dir /tmp/bakeoff --runs 3
```

The driver waits until no `cargo`/`rustc` process is running before every run, and until the GPU has
no compute process before every CUDA run. It writes `bakeoff-summary.json`, which holds every raw run,
the medians and min-max spread, the parity matrix, the recall results and the sha256 of each binary
and of the corpus. It also prints a result table.
