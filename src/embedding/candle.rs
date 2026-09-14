// SPDX-License-Identifier: MIT
//! Pure-Rust neural embeddings via [candle](https://github.com/huggingface/candle),
//! active when built with `--features candle-backend`.
//!
//! Model: `sentence-transformers/all-MiniLM-L6-v2`, FP32 safetensors, 384-dim —
//! the same model the fastembed backend serves through ONNX Runtime, with the
//! same tokenization (512-token truncation, batch-longest padding), mean pooling
//! over the attention mask and L2 normalisation, so the two backends compute the
//! same function and differ only in runtime and device.
//!
//! **Device.** [`CandleDevice::Cpu`] always works. [`CandleDevice::Cuda`] needs a
//! binary built with candle's CUDA kernels — `cargo build --release --features
//! candle-backend,candle-transformers/cuda` (a CUDA toolchain with `nvcc` must be
//! installed) — and a visible GPU at runtime. A CUDA request that cannot be met is
//! an error. It never falls back to the CPU: a caller that asked for the GPU and
//! silently got the CPU would be measuring, or deploying, the wrong thing.
//!
//! **Offline path.** `config.json`, `tokenizer.json` and `model.safetensors` are
//! fetched with `hf-hub` on first use and cached under `$HF_HOME/hub` (default
//! `~/.cache/huggingface/hub`). Nothing is vendored into this repository. For a
//! fully offline host, populate that cache once on a networked machine and copy it
//! (or point `HF_HOME` at it); a cached file is read without touching the network.

use super::{EmbedError, EmbeddingBackend, Result};
use crate::schema::EMBED_DIM;
use candle_core::{Device, DeviceLocation, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use tokenizers::{PaddingStrategy, Tokenizer, TruncationParams};

/// Hugging Face repository the weights, config and tokenizer are read from.
pub const MODEL_REPO: &str = "sentence-transformers/all-MiniLM-L6-v2";

/// Token cap per text. 512 is the model's position-embedding limit and the cap
/// the fastembed backend applies to the same model, so both backends embed
/// exactly the same tokens. (The repository's own `tokenizer.json` truncates and
/// pads to a fixed 128, which is why [`configure_tokenizer`] overrides it.)
pub const MAX_TOKENS: usize = 512;

/// Where candle runs the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandleDevice {
    Cpu,
    /// A CUDA device by ordinal. Refused, never downgraded, when unavailable.
    Cuda(usize),
}

impl CandleDevice {
    /// Open the device. A CUDA request fails when this binary was built without
    /// CUDA kernels or when no such GPU is visible — with no CPU fallback.
    pub fn open(self) -> Result<Device> {
        match self {
            CandleDevice::Cpu => Ok(Device::Cpu),
            CandleDevice::Cuda(ordinal) => {
                if !candle_core::utils::cuda_is_available() {
                    return Err(EmbedError::Backend(
                        "candle-cuda requested, but this binary was built without CUDA support; \
                         rebuild with `--features candle-backend,candle-transformers/cuda` \
                         (no CPU fallback)"
                            .to_string(),
                    ));
                }
                let device = Device::new_cuda(ordinal).map_err(|e| {
                    backend_err(&format!("open CUDA device {ordinal} (no CPU fallback)"), e)
                })?;
                if !device.is_cuda() {
                    return Err(EmbedError::Backend(format!(
                        "CUDA device {ordinal} opened as {:?} (no CPU fallback)",
                        device.location()
                    )));
                }
                Ok(device)
            }
        }
    }
}

fn backend_err(context: &str, e: impl std::fmt::Display) -> EmbedError {
    EmbedError::Backend(format!("{context}: {e}"))
}

/// `sentence-transformers/all-MiniLM-L6-v2` on candle — see the module docs.
pub struct CandleBackend {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    name: &'static str,
}

impl CandleBackend {
    /// Load the model onto `device`. The device is opened BEFORE anything is
    /// fetched, so an unmet CUDA request fails immediately and offline.
    pub fn try_new(device: CandleDevice) -> Result<Self> {
        let name = match device {
            CandleDevice::Cpu => "candle-cpu:all-MiniLM-L6-v2",
            CandleDevice::Cuda(_) => "candle-cuda:all-MiniLM-L6-v2",
        };
        let device = device.open()?;

        let api = hf_hub::api::sync::ApiBuilder::from_env()
            .build()
            .map_err(|e| backend_err("hf-hub init", e))?;
        let repo = api.model(MODEL_REPO.to_string());
        let fetch = |file: &str| {
            repo.get(file)
                .map_err(|e| backend_err(&format!("fetch {MODEL_REPO}/{file}"), e))
        };
        let config_path = fetch("config.json")?;
        let tokenizer_path = fetch("tokenizer.json")?;
        let weights_path = fetch("model.safetensors")?;

        let config_text = std::fs::read_to_string(&config_path)
            .map_err(|e| backend_err("read config.json", e))?;
        let config: Config =
            serde_json::from_str(&config_text).map_err(|e| backend_err("parse config.json", e))?;
        if config.hidden_size != EMBED_DIM as usize {
            return Err(EmbedError::Backend(format!(
                "{MODEL_REPO} has hidden size {}, expected {EMBED_DIM}",
                config.hidden_size
            )));
        }

        let mut tokenizer =
            Tokenizer::from_file(&tokenizer_path).map_err(|e| backend_err("load tokenizer", e))?;
        configure_tokenizer(&mut tokenizer)?;

        let weights =
            std::fs::read(&weights_path).map_err(|e| backend_err("read model.safetensors", e))?;
        let vb = VarBuilder::from_buffered_safetensors(weights, DTYPE, &device)
            .map_err(|e| backend_err("load safetensors", e))?;
        let model = BertModel::load(vb, &config).map_err(|e| backend_err("load BERT", e))?;

        Ok(Self {
            model,
            tokenizer,
            device,
            name,
        })
    }

    /// The device the model actually lives on, read back from candle rather
    /// than echoed from the request: `cpu`, `cuda:<ordinal>` or `metal:<ordinal>`.
    pub fn device_label(&self) -> String {
        match self.device.location() {
            DeviceLocation::Cpu => "cpu".to_string(),
            DeviceLocation::Cuda { gpu_id } => format!("cuda:{gpu_id}"),
            DeviceLocation::Metal { gpu_id } => format!("metal:{gpu_id}"),
        }
    }

    /// Whether the model is on a CUDA device.
    pub fn is_cuda(&self) -> bool {
        self.device.is_cuda()
    }

    fn embed_batch_inner(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| backend_err("tokenize", e))?;
        let batch = encodings.len();
        let seq_len = encodings[0].get_ids().len();
        let mut ids = Vec::with_capacity(batch * seq_len);
        let mut mask = Vec::with_capacity(batch * seq_len);
        for encoding in &encodings {
            ids.extend_from_slice(encoding.get_ids());
            mask.extend_from_slice(encoding.get_attention_mask());
        }
        let tensor_err = |e: candle_core::Error| backend_err("candle", e);
        let ids = Tensor::from_vec(ids, (batch, seq_len), &self.device).map_err(tensor_err)?;
        let mask = Tensor::from_vec(mask, (batch, seq_len), &self.device).map_err(tensor_err)?;
        let token_type_ids = ids.zeros_like().map_err(tensor_err)?;
        let hidden = self
            .model
            .forward(&ids, &token_type_ids, Some(&mask))
            .map_err(tensor_err)?;
        mean_pool_l2(&hidden, &mask)
            .and_then(|pooled| pooled.to_vec2::<f32>())
            .map_err(tensor_err)
    }
}

impl EmbeddingBackend for CandleBackend {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.embed_batch_inner(texts)
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

/// Replace the tokenizer file's padding and truncation with the ones the
/// fastembed backend uses for this model: pad each batch to its longest
/// sequence (keeping the file's pad token and id) and truncate at [`MAX_TOKENS`].
pub(crate) fn configure_tokenizer(tokenizer: &mut Tokenizer) -> Result<()> {
    let mut padding = tokenizer.get_padding().cloned().unwrap_or_default();
    padding.strategy = PaddingStrategy::BatchLongest;
    tokenizer.with_padding(Some(padding));
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: MAX_TOKENS,
            ..Default::default()
        }))
        .map_err(|e| backend_err("configure truncation", e))?;
    Ok(())
}

/// Mean-pool `hidden` (`[batch, seq, dim]`) over the positions `mask`
/// (`[batch, seq]`, 1 = real token) marks, then L2-normalise each row.
/// Padding positions contribute nothing, whatever values they hold.
pub(crate) fn mean_pool_l2(hidden: &Tensor, mask: &Tensor) -> candle_core::Result<Tensor> {
    let mask = mask.to_dtype(hidden.dtype())?.unsqueeze(2)?;
    let summed = hidden.broadcast_mul(&mask)?.sum(1)?;
    let counts = mask.sum(1)?.maximum(1f32)?;
    let mean = summed.broadcast_div(&counts)?;
    let norm = mean.sqr()?.sum_keepdim(1)?.sqrt()?.maximum(1e-12f32)?;
    mean.broadcast_div(&norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_pool_ignores_padding_and_l2_normalizes() {
        // Row 0's last position is padding holding a huge value that would
        // dominate the mean if the mask were ignored.
        let hidden = Tensor::new(
            &[
                [[1f32, 0.], [3., 0.], [1000., 1000.]],
                [[0., 2.], [0., 4.], [0., 6.]],
            ],
            &Device::Cpu,
        )
        .unwrap();
        let mask = Tensor::new(&[[1u32, 1, 0], [1, 1, 1]], &Device::Cpu).unwrap();
        let pooled: Vec<Vec<f32>> = mean_pool_l2(&hidden, &mask).unwrap().to_vec2().unwrap();
        // Row 0: mean of (1,0) and (3,0) is (2,0), normalised to (1,0).
        assert!(
            (pooled[0][0] - 1.0).abs() < 1e-6 && pooled[0][1].abs() < 1e-6,
            "{:?}",
            pooled[0]
        );
        // Row 1: mean (0,4), normalised to (0,1).
        assert!(
            pooled[1][0].abs() < 1e-6 && (pooled[1][1] - 1.0).abs() < 1e-6,
            "{:?}",
            pooled[1]
        );
    }

    // The model repository's tokenizer.json ships fixed-128 padding and 128-token
    // truncation. Left in place, every text would be cut at 128 tokens and every
    // batch padded to 128, so candle would embed different tokens than the
    // fastembed backend does for the same model.
    const FIXED_128_TOKENIZER: &str = r#"{
        "version": "1.0",
        "truncation": {"direction": "Right", "max_length": 128, "strategy": "LongestFirst", "stride": 0},
        "padding": {"strategy": {"Fixed": 128}, "direction": "Right", "pad_to_multiple_of": null,
                    "pad_id": 0, "pad_type_id": 0, "pad_token": "[PAD]"},
        "added_tokens": [],
        "normalizer": null,
        "pre_tokenizer": {"type": "Whitespace"},
        "post_processor": null,
        "decoder": null,
        "model": {"type": "WordLevel", "vocab": {"[PAD]": 0, "[UNK]": 1, "a": 2}, "unk_token": "[UNK]"}
    }"#;

    #[test]
    fn tokenizer_is_reconfigured_to_batch_longest_padding_and_512_truncation() {
        let mut tokenizer = Tokenizer::from_bytes(FIXED_128_TOKENIZER).unwrap();
        configure_tokenizer(&mut tokenizer).unwrap();

        let short = tokenizer.encode_batch(vec!["a", "a a a"], true).unwrap();
        assert_eq!(short[0].get_ids().len(), 3, "padded to the batch's longest");
        assert_eq!(short[0].get_attention_mask(), &[1, 0, 0]);

        let long_text = vec!["a"; 600].join(" ");
        let long = tokenizer
            .encode_batch(vec!["a a", long_text.as_str()], true)
            .unwrap();
        assert_eq!(long[1].get_ids().len(), MAX_TOKENS, "truncated at 512");
        assert_eq!(long[0].get_ids().len(), MAX_TOKENS, "padded to 512");
        assert_eq!(long[0].get_ids()[2], 0, "padding keeps the file's pad id");
    }

    #[test]
    fn cpu_device_opens_as_cpu() {
        assert!(CandleDevice::Cpu.open().unwrap().is_cpu());
    }
}
