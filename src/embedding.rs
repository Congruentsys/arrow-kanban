// SPDX-License-Identifier: MIT
//! Embedding backends for semantic search (issue #104).
//!
//! Embeddings are populated at item write and stored resident on the
//! `embedding` column ([`crate::schema::EMBED_DIM`]) — never a sidecar
//! store. `KanbanStore` itself has no ML dependency: a backend is chosen and
//! invoked by the caller (CLI / server), which computes a `Vec<f32>` and
//! hands it to [`crate::crud::CreateItemInput::embedding`]. That keeps the
//! engine core generic, by design — domain/ML knowledge stays out of the
//! store itself.

use crate::schema::EMBED_DIM;

/// Errors from computing an embedding.
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("embedding backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, EmbedError>;

/// A source of embeddings for item text. Implementations must always return
/// vectors of length [`EMBED_DIM`] — callers (and [`crate::schema::embeddings_to_array`])
/// trust that invariant rather than re-validating it per row.
pub trait EmbeddingBackend: Send + Sync {
    /// Embed a batch of texts — batched so a real neural backend can
    /// amortize one model invocation across many rows.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Embed a single text — the common case (one item, one query).
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self
            .embed_batch(&[text])?
            .into_iter()
            .next()
            .unwrap_or_default())
    }

    /// A short name for provenance — recorded in eval output and the CLI's
    /// `--embedding-provider` help text.
    fn name(&self) -> &'static str;
}

/// Deterministic, offline, dependency-free embedding via the signed hashing
/// trick (Weinberger et al. 2009): whitespace-tokenize, hash each token into
/// one of [`EMBED_DIM`] buckets with a random sign, L2-normalize. No
/// network, no model download, no ONNX runtime — this is the built-in
/// default so a plain `cargo build`/`cargo test` (and any consumer who does
/// not opt into `--features fastembed-backend`) stays hermetic. It is a
/// real, if lexical-only, embedding rather than a stub: cosine similarity
/// over hashed bag-of-words vectors recovers word overlap, which is why it
/// is a legitimate fallback and not a placeholder.
pub struct HashEmbedBackend;

impl EmbeddingBackend for HashEmbedBackend {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| hash_embed(t)).collect())
    }

    fn name(&self) -> &'static str {
        "hash"
    }
}

fn hash_embed(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; EMBED_DIM as usize];
    for raw_token in text.split_whitespace() {
        let token = raw_token.to_lowercase();
        let h = fnv1a(token.as_bytes());
        // A second, independent hash decides the sign, so unrelated tokens
        // partially cancel instead of only ever adding — this is what keeps
        // a short title from being swamped by a long body purely on length.
        let sign_bit = fnv1a(format!("{token}#sign").as_bytes()) & 1;
        let sign = if sign_bit == 0 { 1.0 } else { -1.0 };
        let bucket = (h as usize) % v.len();
        v[bucket] += sign;
    }
    l2_normalize(&mut v);
    v
}

/// FNV-1a — not cryptographic, just a fast, well-distributed, dependency-free
/// hash for bucket assignment.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Real neural embeddings via fastembed-rs (ONNX Runtime) — the shipped
/// default backend (issue #104), active when built with
/// `--features fastembed-backend`. Not vendored: the model downloads from
/// the Hugging Face hub on first use and is cached under
/// `$HOME/.cache/huggingface` (`HF_HOME` overrides) — point that at a
/// pre-populated cache for a fully offline run; see README "Offline path".
#[cfg(feature = "fastembed-backend")]
pub struct FastEmbedBackend {
    model: std::sync::Mutex<fastembed::TextEmbedding>,
}

#[cfg(feature = "fastembed-backend")]
impl FastEmbedBackend {
    /// `AllMiniLML6V2Q` — a small (int8-quantized, ~25 MB download), 384-dim
    /// sentence-transformer. Chosen for CPU-only inference: comparing this
    /// backend against a GPU (candle) path needs a GPU to measure, so that
    /// bake-off is a separate, deferred follow-on — this ships the CPU-viable
    /// half of the trait's default. The whole point of an ONNX default is
    /// that it does not need a GPU to be useful.
    pub fn try_new() -> Result<Self> {
        let options = fastembed::TextInitOptions::new(fastembed::EmbeddingModel::AllMiniLML6V2Q);
        let model = fastembed::TextEmbedding::try_new(options)
            .map_err(|e| EmbedError::Backend(e.to_string()))?;
        Ok(Self {
            model: std::sync::Mutex::new(model),
        })
    }
}

#[cfg(feature = "fastembed-backend")]
impl EmbeddingBackend for FastEmbedBackend {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut model = self
            .model
            .lock()
            .map_err(|e| EmbedError::Backend(e.to_string()))?;
        model
            .embed(texts, None)
            .map_err(|e| EmbedError::Backend(e.to_string()))
    }

    fn name(&self) -> &'static str {
        "fastembed:AllMiniLML6V2Q"
    }
}

/// The backend `arrow-kanban` uses when none is explicitly selected —
/// fastembed-rs ONNX when compiled in (the shipped default, issue #104),
/// hash otherwise (the always-available, offline, hermetic fallback that
/// keeps a plain `cargo build`/`cargo test` free of any network or ONNX
/// dependency).
#[cfg(feature = "fastembed-backend")]
pub fn default_backend() -> Result<Box<dyn EmbeddingBackend>> {
    Ok(Box::new(FastEmbedBackend::try_new()?))
}

#[cfg(not(feature = "fastembed-backend"))]
pub fn default_backend() -> Result<Box<dyn EmbeddingBackend>> {
    Ok(Box::new(HashEmbedBackend))
}

/// Resolve a backend by CLI/config name (`--embedding-provider`).
/// `None`/`""`/`"default"` resolve to [`default_backend`].
pub fn backend_by_name(name: Option<&str>) -> Result<Box<dyn EmbeddingBackend>> {
    match name {
        None | Some("") | Some("default") => default_backend(),
        Some("hash") => Ok(Box::new(HashEmbedBackend)),
        #[cfg(feature = "fastembed-backend")]
        Some("fastembed") => Ok(Box::new(FastEmbedBackend::try_new()?)),
        #[cfg(not(feature = "fastembed-backend"))]
        Some("fastembed") => Err(EmbedError::Backend(
            "fastembed backend not compiled in — rebuild with `--features fastembed-backend`"
                .to_string(),
        )),
        Some(other) => Err(EmbedError::Backend(format!(
            "unknown embedding provider '{other}' (known: hash, fastembed)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_backend_dimension_matches_embed_dim() {
        let v = HashEmbedBackend
            .embed("kanban semantic query")
            .expect("embed");
        assert_eq!(v.len(), EMBED_DIM as usize);
    }

    #[test]
    fn hash_backend_is_deterministic() {
        let a = HashEmbedBackend.embed("fix the flaky test").expect("embed");
        let b = HashEmbedBackend.embed("fix the flaky test").expect("embed");
        assert_eq!(a, b);
    }

    #[test]
    fn hash_backend_is_l2_normalized() {
        let v = HashEmbedBackend
            .embed("normalize me please")
            .expect("embed");
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm was {norm}");
    }

    #[test]
    fn hash_backend_similar_text_scores_higher_than_unrelated() {
        let a = HashEmbedBackend
            .embed("fix the flaky login test")
            .expect("embed");
        let b = HashEmbedBackend
            .embed("fix the flaky login integration test")
            .expect("embed");
        let c = HashEmbedBackend
            .embed("upgrade the onnx runtime dependency")
            .expect("embed");
        let sim_ab: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        let sim_ac: f32 = a.iter().zip(&c).map(|(x, y)| x * y).sum();
        assert!(
            sim_ab > sim_ac,
            "related pair ({sim_ab}) should score above unrelated pair ({sim_ac})"
        );
    }

    #[test]
    fn empty_text_embeds_to_zero_vector_of_correct_dimension() {
        let v = HashEmbedBackend.embed("").expect("embed");
        assert_eq!(v.len(), EMBED_DIM as usize);
        assert!(v.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn embed_batch_matches_embed_called_per_text() {
        let texts = ["alpha bravo", "charlie delta", "echo foxtrot golf"];
        let batch = HashEmbedBackend.embed_batch(&texts).expect("batch");
        for (t, b) in texts.iter().zip(batch.iter()) {
            let single = HashEmbedBackend.embed(t).expect("single");
            assert_eq!(&single, b);
        }
    }

    #[test]
    fn backend_by_name_hash_returns_hash_backend() {
        let v = backend_by_name(Some("hash"))
            .expect("hash backend")
            .embed("x")
            .expect("embed");
        assert_eq!(v.len(), EMBED_DIM as usize);
    }

    #[test]
    fn backend_by_name_none_matches_default_backend() {
        let v = backend_by_name(None)
            .expect("default backend")
            .embed("x")
            .expect("embed");
        assert_eq!(v.len(), EMBED_DIM as usize);
    }

    #[test]
    fn backend_by_name_rejects_unknown_provider() {
        assert!(backend_by_name(Some("carrier-pigeon")).is_err());
    }
}
