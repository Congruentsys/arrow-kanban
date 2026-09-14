// SPDX-License-Identifier: MIT
//! Embedding-backend bake-off: measures ONE backend ("arm") in ONE process and
//! writes one JSON result. Cold model load and peak RSS are per-process
//! quantities, so each repetition is its own process;
//! `scripts/embedding_bakeoff.py` drives every arm serially and aggregates.
//! See `docs/embedding-bakeoff.md` for the method and the recorded result.
//!
//! ```text
//! # CPU arms
//! cargo build --release --features fastembed-backend,candle-backend --example embedding_bakeoff
//! # CUDA arm (needs a CUDA toolchain with nvcc)
//! cargo build --release --features fastembed-backend,candle-backend,candle-transformers/cuda \
//!     --example embedding_bakeoff --target-dir target/cuda
//! target/release/examples/embedding_bakeoff --arm fastembed --corpus items.jsonl --out run.json
//! ```
//!
//! Arms: `fastembed` (ONNX Runtime, int8 `AllMiniLML6V2Q`), `fastembed-fp32`
//! (ONNX Runtime, FP32 `AllMiniLML6V2`), `candle` (FP32, CPU), `candle-cuda`
//! (FP32, CUDA). The corpus is JSON Lines, one `{"text": "..."}` per item.
//!
//! A CUDA arm writes nothing unless it proves it ran on the GPU:
//! - no other compute process is on the GPU when the run starts;
//! - the backend reports a `cuda:N` device, read back from candle, not echoed
//!   from the request;
//! - `nvidia-smi` utilisation sampled during the measured passes peaks at or
//!   above `--min-gpu-util`.
//!
//! Every refusal exits non-zero before the result file is written, so a CPU run
//! can never be recorded as a GPU run.

use arrow_kanban::embedding::{CandleBackend, CandleDevice, EmbeddingBackend, backend_by_name};
use arrow_kanban::schema::EMBED_DIM;
use clap::Parser;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const EXIT_USAGE: i32 = 2;
const EXIT_DEVICE: i32 = 3;
const EXIT_GPU_BUSY: i32 = 4;
const EXIT_GPU_IDLE: i32 = 5;
const EXIT_SAMPLER: i32 = 6;
const EXIT_LOAD: i32 = 7;
const EXIT_EMBED: i32 = 8;
const EXIT_IO: i32 = 9;

const GPU_QUERY: &str =
    "--query-gpu=timestamp,utilization.gpu,memory.used,clocks.current.graphics,power.draw";

#[derive(Parser)]
#[command(about = "Measure one embedding backend in one process and write a JSON result")]
struct Args {
    /// fastembed | fastembed-fp32 | candle | candle-cuda
    #[arg(long)]
    arm: String,
    /// JSON Lines corpus, one {"text": "..."} object per item
    #[arg(long)]
    corpus: PathBuf,
    /// Result file; written only when every check passes
    #[arg(long)]
    out: PathBuf,
    /// Batch sizes, each measured as one pass over the whole corpus
    #[arg(long, value_delimiter = ',', default_value = "1,16,64,256")]
    batch_sizes: Vec<usize>,
    /// Write the batch-1 pass's embeddings here (raw little-endian f32, rows of 384)
    #[arg(long)]
    dump_embeddings: Option<PathBuf>,
    /// nvidia-smi sampling interval for a CUDA arm
    #[arg(long, default_value_t = 500)]
    gpu_sample_ms: u64,
    /// A CUDA arm whose sampled utilisation never reaches this (percent) is refused
    #[arg(long, default_value_t = 10.0)]
    min_gpu_util: f64,
}

struct Refusal {
    code: i32,
    message: String,
}

fn refuse(code: i32, message: impl Into<String>) -> Refusal {
    Refusal {
        code,
        message: message.into(),
    }
}

fn main() {
    let args = Args::parse();
    if let Err(refusal) = run(&args) {
        eprintln!(
            "embedding_bakeoff: REFUSED ({}): {}",
            refusal.code, refusal.message
        );
        std::process::exit(refusal.code);
    }
}

fn ms_since(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn read_corpus(path: &Path) -> Result<(Vec<String>, usize, String), Refusal> {
    let bytes = std::fs::read(path)
        .map_err(|e| refuse(EXIT_USAGE, format!("read corpus {}: {e}", path.display())))?;
    let sha256: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| refuse(EXIT_USAGE, format!("corpus is not UTF-8: {e}")))?;
    let mut texts = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .map_err(|e| refuse(EXIT_USAGE, format!("corpus line {}: {e}", index + 1)))?;
        let item = value.get("text").and_then(Value::as_str).ok_or_else(|| {
            refuse(
                EXIT_USAGE,
                format!("corpus line {}: no string \"text\" field", index + 1),
            )
        })?;
        texts.push(item.to_string());
    }
    if texts.is_empty() {
        return Err(refuse(EXIT_USAGE, "corpus has no items"));
    }
    Ok((texts, bytes.len(), sha256))
}

/// Build the arm's backend and report the device it actually landed on.
fn build_backend(arm: &str) -> Result<(Box<dyn EmbeddingBackend>, String), Refusal> {
    let load_err = |e: arrow_kanban::embedding::EmbedError| refuse(EXIT_LOAD, e.to_string());
    match arm {
        // fastembed runs on ONNX Runtime's default (CPU) execution provider.
        "fastembed" | "fastembed-fp32" => Ok((
            backend_by_name(Some(arm)).map_err(load_err)?,
            "cpu".to_string(),
        )),
        "candle" | "candle-cuda" => {
            let device = if arm == "candle" {
                CandleDevice::Cpu
            } else {
                CandleDevice::Cuda(0)
            };
            let backend = CandleBackend::try_new(device).map_err(load_err)?;
            let label = backend.device_label();
            Ok((Box::new(backend), label))
        }
        other => Err(refuse(
            EXIT_USAGE,
            format!("unknown arm '{other}' (fastembed, fastembed-fp32, candle, candle-cuda)"),
        )),
    }
}

fn proc_status_kb(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|line| line.starts_with(field))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
}

fn gpu_compute_apps() -> Result<Vec<String>, String> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,process_name,used_memory",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .map_err(|e| format!("nvidia-smi: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "nvidia-smi exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("No running"))
        .map(String::from)
        .collect())
}

#[derive(Clone, Serialize)]
struct GpuSample {
    phase: String,
    t_s: f64,
    util_pct: Option<f64>,
    mem_used_mib: Option<f64>,
    clock_mhz: Option<f64>,
    power_w: Option<f64>,
    raw: String,
}

/// `nvidia-smi -lms` running beside the measurement. Each line is tagged with
/// the phase that was current when it arrived (the tool line-flushes into a pipe).
struct GpuSampler {
    child: Child,
    reader: Option<JoinHandle<()>>,
    samples: Arc<Mutex<Vec<GpuSample>>>,
}

impl GpuSampler {
    fn start(interval_ms: u64, phase: Arc<Mutex<String>>, t0: Instant) -> Result<Self, String> {
        let mut child = Command::new("nvidia-smi")
            .args([
                GPU_QUERY,
                "--format=csv,noheader,nounits",
                "-lms",
                &interval_ms.to_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("start nvidia-smi sampler: {e}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "nvidia-smi sampler has no stdout".to_string())?;
        let samples = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&samples);
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let current = phase.lock().map(|p| p.clone()).unwrap_or_default();
                let fields: Vec<&str> = line.split(',').map(str::trim).collect();
                let number = |i: usize| fields.get(i).and_then(|f| f.parse::<f64>().ok());
                let sample = GpuSample {
                    phase: current,
                    t_s: t0.elapsed().as_secs_f64(),
                    util_pct: number(1),
                    mem_used_mib: number(2),
                    clock_mhz: number(3),
                    power_w: number(4),
                    raw: line,
                };
                if let Ok(mut all) = sink.lock() {
                    all.push(sample);
                }
            }
        });
        Ok(Self {
            child,
            reader: Some(reader),
            samples,
        })
    }

    fn wait_for_first_sample(&self, timeout: Duration) -> Result<(), String> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.samples.lock().map(|s| !s.is_empty()).unwrap_or(false) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(format!(
            "nvidia-smi produced no sample within {} s",
            timeout.as_secs()
        ))
    }

    fn stop(mut self) -> Vec<GpuSample> {
        self.halt();
        self.samples
            .lock()
            .map(|mut s| std::mem::take(&mut *s))
            .unwrap_or_default()
    }

    fn halt(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for GpuSampler {
    fn drop(&mut self) {
        self.halt();
    }
}

fn set_phase(phase: &Arc<Mutex<String>>, name: &str) {
    if let Ok(mut current) = phase.lock() {
        *current = name.to_string();
    }
}

/// Nearest-rank percentile over an ascending slice.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil() as usize;
    sorted[rank.clamp(1, n) - 1]
}

fn distribution(values: &[f64]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    json!({
        "n": sorted.len(),
        "mean": sorted.iter().sum::<f64>() / sorted.len() as f64,
        "min": sorted[0],
        "p50": percentile(&sorted, 50.0),
        "p95": percentile(&sorted, 95.0),
        "p99": percentile(&sorted, 99.0),
        "max": sorted[sorted.len() - 1],
    })
}

fn gpu_metric_summary(samples: &[&GpuSample], pick: fn(&GpuSample) -> Option<f64>) -> Value {
    let values: Vec<f64> = samples.iter().filter_map(|s| pick(s)).collect();
    let unavailable = samples.len() - values.len();
    let mut summary = distribution(&values);
    if let Value::Object(map) = &mut summary {
        map.insert("unavailable_samples".into(), json!(unavailable));
        return summary;
    }
    json!({ "n": 0, "unavailable_samples": unavailable })
}

fn gpu_summary(samples: &[&GpuSample]) -> Value {
    json!({
        "samples": samples.len(),
        "utilization_gpu_pct": gpu_metric_summary(samples, |s| s.util_pct),
        "memory_used_mib": gpu_metric_summary(samples, |s| s.mem_used_mib),
        "clocks_current_graphics_mhz": gpu_metric_summary(samples, |s| s.clock_mhz),
        "power_draw_w": gpu_metric_summary(samples, |s| s.power_w),
    })
}

fn check_embeddings(
    rows: &[Vec<f32>],
    expected: usize,
    norms: &mut (f32, f32),
) -> Result<(), Refusal> {
    if rows.len() != expected {
        return Err(refuse(
            EXIT_EMBED,
            format!("expected {expected} embeddings, got {}", rows.len()),
        ));
    }
    for (index, row) in rows.iter().enumerate() {
        if row.len() != EMBED_DIM as usize || row.iter().any(|x| !x.is_finite()) {
            return Err(refuse(
                EXIT_EMBED,
                format!(
                    "embedding {index} has length {} or a non-finite value",
                    row.len()
                ),
            ));
        }
        let norm = row.iter().map(|x| x * x).sum::<f32>().sqrt();
        norms.0 = norms.0.min(norm);
        norms.1 = norms.1.max(norm);
    }
    Ok(())
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), Refusal> {
    let tmp = path.with_extension("partial");
    std::fs::write(&tmp, bytes)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| refuse(EXIT_IO, format!("write {}: {e}", path.display())))
}

fn run(args: &Args) -> Result<(), Refusal> {
    let started_at = chrono::Utc::now().to_rfc3339();
    if args.batch_sizes.is_empty() || args.batch_sizes.contains(&0) {
        return Err(refuse(EXIT_USAGE, "batch sizes must be non-empty and > 0"));
    }
    let (texts, corpus_bytes, corpus_sha256) = read_corpus(&args.corpus)?;
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let n = refs.len();
    let wants_cuda = args.arm == "candle-cuda";

    let t0 = Instant::now();
    let phase = Arc::new(Mutex::new("pre_load".to_string()));
    let mut compute_apps_before = Vec::new();
    let sampler = if wants_cuda {
        compute_apps_before = gpu_compute_apps().map_err(|e| refuse(EXIT_SAMPLER, e))?;
        if !compute_apps_before.is_empty() {
            return Err(refuse(
                EXIT_GPU_BUSY,
                format!("GPU is not idle; compute processes: {compute_apps_before:?}"),
            ));
        }
        let sampler = GpuSampler::start(args.gpu_sample_ms, Arc::clone(&phase), t0)
            .map_err(|e| refuse(EXIT_SAMPLER, e))?;
        sampler
            .wait_for_first_sample(Duration::from_secs(10))
            .map_err(|e| refuse(EXIT_SAMPLER, e))?;
        Some(sampler)
    } else {
        None
    };

    let rss_before_load = proc_status_kb("VmRSS:");
    set_phase(&phase, "load");
    let load_start = Instant::now();
    let (backend, device) = build_backend(&args.arm)?;
    let load_ms = ms_since(load_start);
    let rss_after_load = proc_status_kb("VmRSS:");

    let device_ok = if wants_cuda {
        device.starts_with("cuda:")
    } else {
        device == "cpu"
    };
    if !device_ok {
        return Err(refuse(
            EXIT_DEVICE,
            format!("arm '{}' loaded on device '{device}'", args.arm),
        ));
    }

    let embed_err = |e: arrow_kanban::embedding::EmbedError| refuse(EXIT_EMBED, e.to_string());
    let mut norms = (f32::MAX, f32::MIN);

    set_phase(&phase, "first_call");
    let first_start = Instant::now();
    let first = backend.embed(refs[0]).map_err(embed_err)?;
    let first_call_ms = ms_since(first_start);
    check_embeddings(&[first], 1, &mut norms)?;

    let mut batches = Vec::new();
    let mut batch1_latencies_ms = Vec::new();
    let mut dumped: Option<Vec<Vec<f32>>> = None;
    let dump_batch = if args.batch_sizes.contains(&1) {
        1
    } else {
        args.batch_sizes[0]
    };
    for &batch_size in &args.batch_sizes {
        set_phase(&phase, &format!("warmup_{batch_size}"));
        let warmup_start = Instant::now();
        let warm = backend
            .embed_batch(&refs[..batch_size.min(n)])
            .map_err(embed_err)?;
        let warmup_ms = ms_since(warmup_start);
        check_embeddings(&warm, batch_size.min(n), &mut norms)?;

        set_phase(&phase, &format!("batch_{batch_size}"));
        let mut rows = Vec::with_capacity(n);
        let pass_start = Instant::now();
        for chunk in refs.chunks(batch_size) {
            let call_start = Instant::now();
            let out = backend.embed_batch(chunk).map_err(embed_err)?;
            if batch_size == 1 {
                batch1_latencies_ms.push(ms_since(call_start));
            }
            rows.extend(out);
        }
        let seconds = pass_start.elapsed().as_secs_f64();
        set_phase(&phase, "between_passes");
        check_embeddings(&rows, n, &mut norms)?;
        batches.push(json!({
            "batch_size": batch_size,
            "items": n,
            "seconds": seconds,
            "items_per_s": n as f64 / seconds,
            "warmup_ms": warmup_ms,
        }));
        if batch_size == dump_batch && dumped.is_none() {
            dumped = Some(rows);
        }
    }
    set_phase(&phase, "post");

    let compute_apps_at_end = if wants_cuda {
        Some(gpu_compute_apps().map_err(|e| refuse(EXIT_SAMPLER, e))?)
    } else {
        None
    };
    let rss_peak = proc_status_kb("VmHWM:");

    let gpu = match sampler {
        None => Value::Null,
        Some(sampler) => {
            let samples = sampler.stop();
            let measured: Vec<&GpuSample> = samples
                .iter()
                .filter(|s| s.phase.starts_with("batch_"))
                .collect();
            let max_util = measured
                .iter()
                .filter_map(|s| s.util_pct)
                .fold(None, |acc: Option<f64>, u| {
                    Some(acc.map_or(u, |a| a.max(u)))
                });
            match max_util {
                None => {
                    return Err(refuse(
                        EXIT_GPU_IDLE,
                        "no GPU utilisation sample landed in the measured passes",
                    ));
                }
                Some(peak) if peak < args.min_gpu_util => {
                    return Err(refuse(
                        EXIT_GPU_IDLE,
                        format!(
                            "GPU utilisation peaked at {peak}% during the measured passes \
                             (floor {}%): this was not a GPU run",
                            args.min_gpu_util
                        ),
                    ));
                }
                Some(_) => {}
            }
            let own_pid = std::process::id().to_string();
            let apps_at_end = compute_apps_at_end.unwrap_or_default();
            let own_used_memory = apps_at_end
                .iter()
                .find(|row| row.split(',').next().map(str::trim) == Some(own_pid.as_str()))
                .and_then(|row| row.rsplit(',').next())
                .map(|field| field.trim().to_string());
            let mut phases: Vec<String> = samples.iter().map(|s| s.phase.clone()).collect();
            phases.dedup();
            let per_phase: serde_json::Map<String, Value> = phases
                .iter()
                .map(|name| {
                    let in_phase: Vec<&GpuSample> =
                        samples.iter().filter(|s| &s.phase == name).collect();
                    (name.clone(), gpu_summary(&in_phase))
                })
                .collect();
            json!({
                "sample_interval_ms": args.gpu_sample_ms,
                "query": GPU_QUERY,
                "min_gpu_util_floor_pct": args.min_gpu_util,
                "compute_apps_before_load": compute_apps_before,
                "compute_apps_at_end": apps_at_end,
                "own_process_used_memory_mib": own_used_memory,
                "measured_passes": gpu_summary(&measured),
                "per_phase": per_phase,
                "samples": samples,
            })
        }
    };

    let dump = match (&args.dump_embeddings, dumped) {
        (Some(path), Some(rows)) => {
            let mut bytes = Vec::with_capacity(rows.len() * EMBED_DIM as usize * 4);
            for value in rows.iter().flatten() {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            write_atomically(path, &bytes)?;
            json!({
                "path": path.display().to_string(),
                "format": "f32 little-endian, row-major",
                "rows": rows.len(),
                "dim": EMBED_DIM,
                "from_batch_size": dump_batch,
            })
        }
        _ => Value::Null,
    };

    let result = json!({
        "schema": "arrow-kanban/embedding-bakeoff-run/1",
        "started_at": started_at,
        "arm": args.arm,
        "backend_name": backend.name(),
        "device": device,
        "cuda_compiled": candle_core::utils::cuda_is_available(),
        "host": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "available_parallelism": std::thread::available_parallelism().map(|p| p.get()).ok(),
        },
        "corpus": { "items": n, "bytes": corpus_bytes, "sha256": corpus_sha256 },
        "load_ms": load_ms,
        "first_call_ms": first_call_ms,
        "batches": batches,
        "batch1_latency_ms": distribution(&batch1_latencies_ms),
        "rss_kb": { "before_load": rss_before_load, "after_load": rss_after_load, "peak": rss_peak },
        "embedding_l2_norm": { "min": norms.0, "max": norms.1 },
        "gpu": gpu,
        "embeddings_dump": dump,
    });
    let text = serde_json::to_string_pretty(&result)
        .map_err(|e| refuse(EXIT_IO, format!("serialise result: {e}")))?;
    write_atomically(&args.out, text.as_bytes())?;
    println!(
        "{} on {device}: load {load_ms:.0} ms, first call {first_call_ms:.1} ms, result {}",
        args.arm,
        args.out.display()
    );
    Ok(())
}
