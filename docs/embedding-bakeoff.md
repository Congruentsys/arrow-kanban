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

## Result

**Where this was measured**

| | |
|---|---|
| Machine | NVIDIA GB10 Grace-Blackwell, aarch64 |
| CPU | 20 cores: 10x Cortex-X925 + 10x Cortex-A725 |
| Memory | 128 GB unified |
| Software | driver 580.173.02, CUDA 13.0, rustc 1.97.1 |
| Code | commit `452f7dd` |

**Corpus.** 1024 real board items with median text length 2,207 characters. Most items therefore reach the 512-token cap.

Each cell below is the median of the arm's three runs, with the min-max spread in parentheses.

| arm | device | runs | cold load ms | first call ms | batch-1 p50 ms | batch-1 p95 ms | batch-1 p99 ms | items/s b1 | items/s b16 | items/s b64 | items/s b256 | after-load RSS MiB | peak RSS MiB | recall@5 | recall@10 | MRR |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| A fastembed int8 (CPU) | cpu | 3 | 147 (144-158) | 44.3 (43.9-47.0) | 126.96 (125.72-129.27) | 161.43 (160.04-161.87) | 171.96 (171.09-180.09) | 9.1 (9.1-9.3) | 12.2 (12.0-12.3) | 13.4 (13.3-13.6) | 13.7 (13.7-13.8) | 81 (80-82) | 19035 (19032-19038) | 1.0000 | 1.0000 | 0.9583 |
| B fastembed FP32 (CPU) | cpu | 3 | 259 (257-261) | 90.2 (82.9-90.6) | 227.85 (206.12-232.51) | 264.58 (261.42-265.27) | 274.82 (271.29-278.19) | 5.3 (5.2-5.5) | 5.9 (5.9-6.0) | 6.2 (6.1-6.3) | 9.2 (9.2-9.3) | 155 (155-157) | 18548 (18544-18556) | 1.0000 | 1.0000 | 0.9583 |
| C candle FP32 (CUDA) | cuda:0 | 3 | 610 (606-615) | 436.8 (433.1-442.9) | 28.17 (28.07-28.32) | 48.49 (45.87-51.28) | 82.11 (70.44-99.71) | 37.6 (36.2-38.7) | 39.1 (39.1-39.6) | 41.7 (41.6-41.9) | 43.2 (43.1-43.2) | 320 (320-320) | 685 (682-687) | 1.0000 | 1.0000 | 0.9583 |
| D candle FP32 (CPU) | cpu | 3 | 160 (153-163) | 253.5 (234.5-393.5) | 1504.71 (1496.99-1528.03) | 2463.45 (2417.44-2484.90) | 2785.86 (2681.66-2794.18) | 0.7 (0.7-0.8) | 0.6 (0.6-0.6) | 0.7 (0.7-0.7) | 0.7 (0.7-0.7) | 111 (111-111) | 17190 (17189-17190) | 1.0000 | 1.0000 | 0.9583 |
| hash (control) | cpu | - | - | - | - | - | - | - | - | - | - | - | - | 0.8333 | 0.9167 | 0.5764 |

**Output parity.** Over the same 1024 texts, the three FP32 arms (B, C, D) agree to six decimals: mean and minimum cosine are both 1.000000. Against them, the int8 arm (A) has mean cosine 0.992294 and minimum 0.984270.

**Retrieval quality.** All four arms score identically on the evaluation set: recall@5 1.0, recall@10 1.0, MRR 0.9583. The `hash` control reproduces its published figures exactly, which confirms the evaluation corpus is unchanged.

**The CUDA runs really ran on the GPU.** All three reported device `cuda:0`, with GPU utilisation at 96% (both max and median) across roughly 200 samples per run.

## Decision

The decision rule was fixed before the first timed run and applied unchanged.

### The default stays `fastembed` (int8, ONNX Runtime)

- A GPU backend cannot be the default of a CLI that must work without one, so the candidates were the three CPU arms. All three pass the quality bar.
- The deciding metric is the cost of one write in a fresh CLI process: cold load plus batch-1 p50.

  | backend | cost of one write |
  |---|---|
  | fastembed int8 | 274 ms |
  | fastembed FP32 | 488 ms |
  | candle CPU | 1,665 ms |

- To replace the default, a challenger had to be at least 20% cheaper with no overlap between run spreads. Neither was.

### candle CUDA ships as an opt-in path

**Pass criteria.** The rule required at least 3x the best CPU backend's throughput at both batch 64 and batch 256, with no quality loss.

**Measured:**

| batch | candle CUDA | fastembed int8 | speed-up |
|---|---|---|---|
| 64 | 41.7 items/s | 13.4 items/s | 3.11x |
| 256 | 43.2 items/s | 13.7 items/s | 3.15x |

The margin over 3x is small. As a check, pairing candle CUDA's slowest run with fastembed's fastest run still gives 3.06x at batch 64 and 3.13x at batch 256.

**Recommended use:** bulk (re-)embedding, e.g. `arrow-kanban embed --all --embedding-provider candle-cuda`.

**Not recommended:** single writes. candle CUDA's per-item latency is low (28 ms batch-1 p50), but a fresh process pays about 610 ms of CUDA and model load first, so fastembed remains cheaper there.

### candle on CPU is not recommended

At default thread settings on this machine, candle on CPU is about 12x slower per item than fastembed int8 (1.50 s against 127 ms batch-1 p50). The thread-count note below explains part of the gap. It stays available only because it is the same `candle-backend` that provides the CUDA path.

## What these numbers do and do not show

- **Thread count (candle CPU vs fastembed).** Configured parallelism is matched: both libraries default to 20 threads on all 20 cores. Realized parallelism is not. On this workload and this big.LITTLE CPU, candle keeps about 4 cores busy, while ONNX Runtime keeps about 7-11. The candle CPU figures reflect that lower scaling at default settings. They are not evidence that candle's kernels are slower per core, and no comparison at matched realized parallelism was run.
- **The GPU was running slow.** Under load, this unit's GPU graphics clock stayed at 617 MHz at 96% utilisation, drawing about 11 W. A sibling unit of the same model runs at about 2,470 MHz under load. This can only have held candle CUDA back, so the measured 3.1x likely understates what the same GPU does when it is not clock-limited.
- **Memory.** The CPU arms' 17-19 GB peak RSS, and candle CUDA's roughly 20 GB of GPU memory, come from the batch-256 pass over 512-token texts. After load, a process holds 81-320 MiB (see the table), and a one-item `create` never approaches the peak. For bulk embedding of long texts, a smaller batch bounds memory.
- **Small quality set.** There are 12 queries, so one rank swap moves MRR by about 0.04. All four backends tied.
- **One run was replaced.** A 15-second host page-cache eviction overlapped one fastembed FP32 run. Under a rule set before that run's numbers were read, it was excluded from the aggregates and replaced by an extra run.
- **Mixing backends on one board.** The three FP32 backends produced identical vectors, so a board embedded with any one of them can be queried with another. The int8 `fastembed` differs slightly (minimum cosine 0.984 against FP32). After switching between int8 and FP32, re-embed with `arrow-kanban embed --all`.

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
