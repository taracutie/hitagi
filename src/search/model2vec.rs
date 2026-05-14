//! Model2Vec encoder.
//!
//! Loads tokenizer + safetensors + config from a local directory or from
//! Hugging Face Hub (cached under `$HF_HOME` ~ see `model_cache.rs`).
//! Encoding is a per-token weighted average of the embedding matrix rows;
//! optional weights/mapping tensors come from distillation.
//!
//! Also exposes `HashingEncoder` ~ a deterministic tokens-to-bag-of-hashed-
//! signs encoder. No model files needed; lower retrieval quality than a
//! real Model2Vec model but works completely offline. Used as the
//! `--hashing` opt-in and as the auto-fallback when `--offline` blocks the
//! download path.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ndarray::Array2;
use rayon::prelude::*;
use safetensors::tensor::Dtype;
use safetensors::SafeTensors;
use serde::Deserialize;
use tokenizers::models::ModelWrapper;
use tokenizers::normalizers::NormalizerWrapper;
use tokenizers::pre_tokenizers::PreTokenizerWrapper;
use tokenizers::{Model, Tokenizer};

use crate::error::{AppError, AppResult};

use super::fast_wp::{self, FastBertWordPiece, FastWriteInput};
use super::fuse::QueryEncoder;
use super::tokens::tokenize as bm25_tokenize;

pub const DEFAULT_MODEL_NAME: &str = "minishlab/potion-code-16M";
pub const DEFAULT_DIM: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelLoadPolicy {
    AllowDownload,
    NoDownload,
    Offline,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelOptions {
    pub model: String,
    pub policy: ModelLoadPolicy,
}

impl ModelOptions {
    pub fn new(model: Option<&str>, policy: ModelLoadPolicy) -> Self {
        Self {
            model: model
                .map(str::to_owned)
                .or_else(|| std::env::var("MIMI_MODEL").ok())
                .unwrap_or_else(|| DEFAULT_MODEL_NAME.to_owned()),
            policy,
        }
    }
}

pub trait Encoder: Send + Sync + QueryEncoder {
    fn dim(&self) -> usize;
    fn encode(&self, texts: &[String]) -> Array2<f32>;
}

#[derive(Clone, Debug)]
pub struct ModelStatus {
    pub tokenizer: Option<PathBuf>,
    pub safetensors: Option<PathBuf>,
    pub config: Option<PathBuf>,
}

impl ModelStatus {
    pub fn available(&self) -> bool {
        self.tokenizer.is_some() && self.safetensors.is_some() && self.config.is_some()
    }
}

pub fn model_status(model: Option<&str>) -> ModelStatus {
    let model = model
        .map(str::to_owned)
        .or_else(|| std::env::var("MIMI_MODEL").ok())
        .unwrap_or_else(|| DEFAULT_MODEL_NAME.to_owned());
    let path = Path::new(&model);
    if path.exists() {
        return ModelStatus {
            tokenizer: existing_file(path.join("tokenizer.json")),
            safetensors: existing_file(path.join("model.safetensors")),
            config: existing_file(path.join("config.json")),
        };
    }
    let cache = hf_hub::Cache::from_env().repo(hf_hub::Repo::model(model.clone()));
    ModelStatus {
        tokenizer: cache.get("tokenizer.json"),
        safetensors: cache.get("model.safetensors"),
        config: cache.get("config.json"),
    }
}

pub fn load_model(options: &ModelOptions) -> AppResult<Box<dyn Encoder>> {
    Ok(Box::new(Model2VecEncoder::from_options(options)?))
}

pub fn model_fingerprint(options: &ModelOptions) -> AppResult<String> {
    let (tokenizer_path, model_path, config_path) = model_files(options)?;
    let mut hasher = DefaultHasher::new();
    options.model.hash(&mut hasher);
    hash_file(&tokenizer_path, &mut hasher)?;
    hash_file(&model_path, &mut hasher)?;
    hash_file(&config_path, &mut hasher)?;
    Ok(format!("{:016x}", hasher.finish()))
}

/// Cheap stat-only signature of the three model files. Used as a fast-path
/// hint by `load_encoder_with_policy`: if the persisted dense row's
/// `model_files_meta` matches a fresh tuple, the multi-MB SHA pass in
/// `model_fingerprint` can be skipped and the cached fingerprint reused.
pub fn model_files_meta(options: &ModelOptions) -> AppResult<String> {
    let (tokenizer_path, model_path, config_path) = model_files(options)?;
    let parts = [&tokenizer_path, &model_path, &config_path]
        .into_iter()
        .map(|p| {
            let md = fs::metadata(p).map_err(|err| {
                AppError::internal(format!("stat model file {}: {err}", p.display()))
            })?;
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i128)
                .unwrap_or(0);
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            Ok::<_, AppError>(format!("{}:{}:{}", name, mtime, md.len()))
        })
        .collect::<AppResult<Vec<_>>>()?;
    Ok(parts.join("|"))
}

pub struct Model2VecEncoder {
    /// Fast WordPiece path for query encoding. Present on every warm load:
    /// either restored from the `fast_wp` cache or freshly built alongside
    /// the slow tokenizer after a cache miss. The query (single-text)
    /// encode path uses this exclusively ~ no allocation-heavy WordPiece
    /// build per warm search.
    fast: Option<FastBertWordPiece>,
    /// Slow `tokenizers::Tokenizer` for batch encoding (the index-build
    /// hot path, where the rayon-parallel `encode_batch_fast` is well
    /// optimized). Built lazily: on a fast-cache hit, we never call
    /// `Tokenizer::from_file` unless the caller actually invokes
    /// `encode`. Stored behind `OnceLock` so concurrent encoders share
    /// the same instance after a single resolution.
    slow: OnceLock<Tokenizer>,
    /// Path to the on-disk `tokenizer.json` for the lazy slow load. Kept
    /// even on fast-only paths so we can produce a Tokenizer when
    /// `encode_batch_fast` is finally needed.
    tokenizer_path: PathBuf,
    /// Token embeddings. Either an owned `Array2` (rebuild path) or a
    /// zero-copy view over the safetensors mmap. The warm path picks the
    /// mapped variant so the 60+ MB embeddings matrix is never memcpy'd
    /// out of the page cache ~ rows are read on demand by `encode_query`,
    /// which touches a handful of pages per call.
    embeddings: EmbeddingsMatrix,
    weights: Option<Vec<f32>>,
    token_mapping: Option<Vec<usize>>,
    normalize: bool,
    median_token_length: usize,
    unk_token_id: Option<u32>,
}

/// Storage for the model's embedding matrix. The owned variant is used
/// after a rebuild or for non-f32 dtypes; the mapped variant points an
/// `ArrayView2` at the safetensors `embeddings` tensor's bytes inside
/// the mmap'd model file. Both share the same f32 layout so callers
/// access rows via the same `ArrayView1` regardless of provenance.
#[derive(Debug)]
enum EmbeddingsMatrix {
    Owned(Array2<f32>),
    /// `safetensors_mmap[body_offset..body_offset + rows*cols*4]` is the
    /// row-major f32 LE body. The `Arc<Mmap>` keeps the mapping alive for
    /// as long as the encoder ~ dropping it would unmap the rows.
    Mapped {
        mmap: std::sync::Arc<memmap2::Mmap>,
        body_offset: usize,
        rows: usize,
        cols: usize,
    },
}

impl EmbeddingsMatrix {
    fn view(&self) -> ndarray::ArrayView2<'_, f32> {
        match self {
            EmbeddingsMatrix::Owned(arr) => arr.view(),
            EmbeddingsMatrix::Mapped {
                mmap,
                body_offset,
                rows,
                cols,
            } => {
                let len = *rows * *cols;
                // Safety: `Self::try_map` validated the byte range and
                // alignment (mmap is page-aligned, body_offset is a
                // multiple of 4). f32 has 4-byte alignment; LE bytes on
                // x86_64 / aarch64 LE platforms match the in-memory
                // layout the writer produced via `to_le_bytes`.
                let floats = unsafe {
                    std::slice::from_raw_parts(mmap.as_ptr().add(*body_offset) as *const f32, len)
                };
                ndarray::ArrayView2::from_shape((*rows, *cols), floats)
                    .expect("validated shape in try_map")
            }
        }
    }

    fn ncols(&self) -> usize {
        match self {
            EmbeddingsMatrix::Owned(arr) => arr.ncols(),
            EmbeddingsMatrix::Mapped { cols, .. } => *cols,
        }
    }

    /// Try to wrap the f32 LE bytes inside an mmap as a zero-copy view.
    /// Returns `None` if shape or alignment invariants aren't met; the
    /// caller falls back to the owned `Array2` constructed by
    /// `read_f32_matrix_parallel`.
    fn try_map(
        mmap: std::sync::Arc<memmap2::Mmap>,
        body_offset: usize,
        rows: usize,
        cols: usize,
    ) -> Option<Self> {
        let bytes = rows.checked_mul(cols).and_then(|n| n.checked_mul(4))?;
        if body_offset.checked_add(bytes)? > mmap.len() {
            return None;
        }
        if (mmap.as_ptr() as usize + body_offset) % std::mem::align_of::<f32>() != 0 {
            return None;
        }
        Some(EmbeddingsMatrix::Mapped {
            mmap,
            body_offset,
            rows,
            cols,
        })
    }
}

impl Model2VecEncoder {
    pub fn from_options(options: &ModelOptions) -> AppResult<Self> {
        let (tokenizer_path, model_path, config_path) = model_files(options)?;
        // Two warm-path tokenizer formats sit on disk:
        //   1. `fast_wp.v1/<hash>.bin` ~ sorted-byte WordPiece vocab + flags,
        //      decoded via three `copy_from_slice` calls. Powers
        //      `FastBertWordPiece::dim_independent_encode` on warm queries.
        //   2. The full `Tokenizer` (no on-disk cache by design ~ the slow
        //      JSON parse is paid on cache (1) miss). Lazy-loaded later if
        //      `encode_batch_fast` is ever called (index rebuild path).
        //
        // The tokenizer parse + WordPiece HashMap build that the previous
        // `tokenizer_cache` paid (~40 ms warm) is gone from the search wall
        // entirely on cache (1) hits: `fast_wp::try_load` finishes in a
        // couple ms and the matching `BertNormalizer` flags ride alongside.
        //
        // We probe the fast cache synchronously up-front (the probe is a
        // sub-5ms mmap + flat-table memcpy on warm OS page cache). On a
        // hit, the `unk_token` + `config.json` sidecar reads disappear
        // entirely: both values come back stamped in the cache. On a
        // miss, the sidecar reads ride parallel with the slow tokenizer
        // parse (their old layout).
        let preloaded_fast = fast_wp::try_load(&tokenizer_path);
        let fast_cache_hit = preloaded_fast.is_some();
        // Capture for use inside parallel closures (the closures only run
        // when this branch is taken, so taking a copy here is fine).
        let need_sidecar = !fast_cache_hit;
        let need_slow_tokenizer = !fast_cache_hit;
        let load_tokenizer = || -> AppResult<Option<Tokenizer>> {
            if !need_slow_tokenizer {
                return Ok(None);
            }
            let tk = Tokenizer::from_file(&tokenizer_path).map_err(|err| {
                AppError::internal(format!(
                    "failed to load tokenizer from {}: {err}",
                    tokenizer_path.display()
                ))
            })?;
            Ok(Some(tk))
        };
        let load_tensors =
            || -> AppResult<(EmbeddingsMatrix, Option<Vec<f32>>, Option<Vec<usize>>)> {
                // mmap the safetensors file ~ on warm cache the bytes are
                // already in the OS page cache. For the f32 embedding matrix
                // we keep the mmap alive in the encoder and point an
                // `ArrayView2` at the body bytes directly, so the 60+ MB
                // memcpy `read_f32_matrix_parallel` used to do is gone from
                // the warm wall entirely. Non-f32 tensors fall back to the
                // owned-Vec path.
                let model_file = fs::File::open(&model_path).map_err(AppError::from)?;
                let mmap = unsafe { memmap2::Mmap::map(&model_file) }
                    .map_err(|err| AppError::internal(format!("mmap safetensors: {err}")))?;
                let tensors = SafeTensors::deserialize(&mmap).map_err(|err| {
                    AppError::internal(format!("failed to read safetensors: {err}"))
                })?;
                // The 63 MB embeddings memcpy dominates the load. Split it into
                // strided chunks so the writes happen in parallel ~ on 8+ cores
                // we move from a single-thread 5-6 GB/s pipe to whatever the
                // memory bus can sustain across rayon workers (~2x on the WSL2
                // box this is tuned against). Weights + mapping are tiny and
                // ride on the calling thread alongside the parallel fanout.
                let embeddings_tensor = tensors.tensor("embeddings").map_err(|err| {
                    AppError::internal(format!("missing embeddings tensor: {err}"))
                })?;
                let shape = embeddings_tensor.shape();
                let [rows, cols]: [usize; 2] = shape
                    .try_into()
                    .map_err(|_| AppError::internal("embeddings is not 2D".to_string()))?;
                let data = embeddings_tensor.data();
                let weights = tensors
                    .tensor("weights")
                    .ok()
                    .map(|t| read_f32_data(t.dtype(), t.data()))
                    .transpose()?;
                let token_mapping = tensors
                    .tensor("mapping")
                    .ok()
                    .map(|t| read_usize_data(t.dtype(), t.data()))
                    .transpose()?;
                let embeddings = if embeddings_tensor.dtype() == Dtype::F32 {
                    // Body offset is the byte distance from the mmap's start to
                    // the tensor's first byte. SafeTensors hands us a `&[u8]`
                    // borrowed from the mmap, so pointer subtraction recovers
                    // the absolute offset; alignment is enforced by `try_map`.
                    let body_offset = (data.as_ptr() as usize).wrapping_sub(mmap.as_ptr() as usize);
                    // Drop the SafeTensors header parser first so its borrow on
                    // `mmap` is released before we hand `mmap` to `Arc`.
                    drop(tensors);
                    let mmap_arc = std::sync::Arc::new(mmap);
                    EmbeddingsMatrix::try_map(mmap_arc, body_offset, rows, cols).ok_or_else(
                        || {
                            AppError::internal(
                                "embeddings mmap could not be aligned/sized for zero-copy view"
                                    .to_string(),
                            )
                        },
                    )?
                } else {
                    // Non-f32 path: keep the existing memcpy + dtype conversion.
                    let owned =
                        read_f32_matrix_parallel(data, rows, cols, embeddings_tensor.dtype())?;
                    drop(tensors);
                    drop(mmap);
                    EmbeddingsMatrix::Owned(owned)
                };
                Ok((embeddings, weights, token_mapping))
            };
        // Sidecar: scan tokenizer.json for `unk_token` and load config.json.
        // Both are <20 ms in serial; running here hides them entirely behind
        // the tokenizer parse. Returns `(unk_token, normalize)`. Failures are
        // benign defaults so we never block the encoder on a missing field.
        //
        // Fast-cache hits already carry `unk_token_id` and `config_normalize`
        // in the stamped blob, so the sidecar's only role becomes a slow-path
        // fallback. We skip it entirely when `try_load` succeeded ~ the
        // tokenizer.json + config.json reads were the longest single chunk on
        // the warm wall after the tokenizer build itself disappeared.
        let load_sidecar = || -> (Option<String>, Option<bool>) {
            // On a fast-cache hit we already carry both values; the closure
            // returns sentinels and skips the 20+ ms of filesystem reads.
            if !need_sidecar {
                return (None, None);
            }
            let unk = read_unk_token_from_file(&tokenizer_path);
            let normalize = Some(read_normalize_from_config(&config_path).unwrap_or(true));
            (unk, normalize)
        };
        // Two-way join keeps the existing thread budget; the sidecar runs on
        // the calling thread alongside the tensors branch so the tokenizer
        // parse stays alone on a worker (its own JSON parse is single-
        // threaded inside the tokenizers crate, so an extra worker wouldn't
        // help it).
        let tensors_then_sidecar = || -> AppResult<(
            (EmbeddingsMatrix, Option<Vec<f32>>, Option<Vec<usize>>),
            (Option<String>, Option<bool>),
        )> {
            let (tensors, sidecar) = rayon::join(load_tensors, load_sidecar);
            Ok((tensors?, sidecar))
        };
        let (tokenizer_result, tensors_sidecar_result) =
            rayon::join(load_tokenizer, tensors_then_sidecar);
        let slow_tokenizer_opt = tokenizer_result?;
        let ((embeddings, weights, token_mapping), (unk_token_sidecar, normalize_sidecar)) =
            tensors_sidecar_result?;
        // Two flow shapes:
        //   - Fast cache hit: `preloaded_fast` already has median + unk_id +
        //     config_normalize stamped; no slow Tokenizer ran on this path.
        //     The `slow` OnceLock stays empty until something calls `encode()`.
        //   - Slow cache miss: the freshly-parsed Tokenizer is the source of
        //     truth. We extract a `FastBertWordPiece` from it so this run
        //     gets the fast query path too, and persist the fast cache so
        //     next run skips the JSON parse entirely.
        let slow_cell = OnceLock::new();
        let (fast, median_token_length, unk_token_id, normalize) = if let Some(fast) =
            preloaded_fast
        {
            let median = fast.median_token_length;
            let unk_id = fast.unk_token_id;
            let norm = fast.config_normalize;
            (Some(fast), median, unk_id, norm)
        } else {
            let tokenizer = slow_tokenizer_opt
                .ok_or_else(|| AppError::internal("slow tokenizer missing on miss".to_string()))?;
            let unk_token = unk_token_sidecar.unwrap_or_else(|| "[UNK]".to_string());
            let median = median_token_byte_len(&tokenizer);
            let unk_id = tokenizer.token_to_id(&unk_token);
            let norm = normalize_sidecar.unwrap_or(true);
            let fast = build_fast_from_tokenizer(
                &tokenizer,
                &unk_token,
                median,
                unk_id,
                norm,
                &tokenizer_path,
            );
            let _ = slow_cell.set(tokenizer);
            (fast, median, unk_id, norm)
        };

        Ok(Self {
            fast,
            slow: slow_cell,
            tokenizer_path,
            embeddings,
            weights,
            token_mapping,
            normalize,
            median_token_length,
            unk_token_id,
        })
    }

    /// Resolve (and cache) the heavy `Tokenizer` for batch encoding. Cheap
    /// when already loaded; pays the JSON-parse cost once on the first
    /// `encode_batch_fast` call after a fast-cache hit. Warm query encoding
    /// never reaches this branch.
    fn slow_tokenizer(&self) -> &Tokenizer {
        self.slow.get_or_init(|| {
            Tokenizer::from_file(&self.tokenizer_path)
                .expect("tokenizer.json load failed in lazy slow path")
        })
    }
}

/// Build a `FastBertWordPiece` from a freshly-parsed `Tokenizer`, and
/// persist the same data to the `fast_wp` cache so subsequent runs skip
/// the slow JSON parse. Returns `None` only when the tokenizer doesn't
/// match the BERT-WordPiece shape we accelerate; the caller falls back
/// to the slow encode path in that case.
fn build_fast_from_tokenizer(
    tokenizer: &Tokenizer,
    unk_token: &str,
    median_token_length: usize,
    unk_token_id: Option<u32>,
    config_normalize: bool,
    tokenizer_path: &Path,
) -> Option<FastBertWordPiece> {
    let (vocab, _wp_unk, continuing_subword_prefix, max_input_chars_per_word) =
        wordpiece_state(tokenizer)?;
    if !has_bert_pre_tokenizer(tokenizer) {
        return None;
    }
    let normalizer_flags = bert_normalizer_flags(tokenizer);
    let added_tokens = added_tokens_for_fast(tokenizer);
    let input = FastWriteInput {
        vocab,
        unk_token: unk_token.to_owned(),
        continuing_subword_prefix,
        max_input_chars_per_word,
        normalizer_flags,
        median_token_length,
        unk_token_id,
        added_tokens,
        config_normalize,
    };
    let fast = fast_wp::build_from_input(&input);
    // Persist for next time. Failures are silently ignored ~ the next warm
    // search will hit this same slow path again.
    fast_wp::write(tokenizer_path, &input);
    Some(fast)
}

fn wordpiece_state(tokenizer: &Tokenizer) -> Option<(Vec<(String, u32)>, String, String, u32)> {
    let ModelWrapper::WordPiece(wp) = tokenizer.get_model() else {
        return None;
    };
    let vocab: Vec<(String, u32)> = wp.get_vocab().into_iter().collect();
    let unk = wp.unk_token.clone();
    let prefix = wp.continuing_subword_prefix.clone();
    let max_chars = wp.max_input_chars_per_word as u32;
    Some((vocab, unk, prefix, max_chars))
}

fn bert_normalizer_flags(tokenizer: &Tokenizer) -> (bool, bool, Option<bool>, bool) {
    let Some(NormalizerWrapper::BertNormalizer(b)) = tokenizer.get_normalizer() else {
        return (false, false, None, false);
    };
    (
        b.clean_text,
        b.handle_chinese_chars,
        b.strip_accents,
        b.lowercase,
    )
}

fn has_bert_pre_tokenizer(tokenizer: &Tokenizer) -> bool {
    matches!(
        tokenizer.get_pre_tokenizer(),
        Some(PreTokenizerWrapper::BertPreTokenizer(_))
    )
}

fn added_tokens_for_fast(tokenizer: &Tokenizer) -> Vec<(u32, String, bool)> {
    let added = tokenizer.get_added_vocabulary();
    let mut out: Vec<(u32, String, bool)> = added
        .get_added_tokens_decoder()
        .iter()
        .map(|(id, token)| (*id, token.content.clone(), token.normalized))
        .collect();
    // Deterministic ordering for cache stability.
    out.sort_by_key(|(id, _, _)| *id);
    out
}

/// Median UTF-8 byte length of vocab tokens, computed in O(n) via
/// `select_nth_unstable` instead of an O(n log n) sort. Used as a coarse
/// chars-to-bytes scale factor for pre-tokenizer text truncation; the exact
/// value isn't ranking-critical, only the gross magnitude.
fn median_token_byte_len(tokenizer: &Tokenizer) -> usize {
    let vocab = tokenizer.get_vocab(false);
    if vocab.is_empty() {
        return 1;
    }
    let mut lens: Vec<usize> = vocab.into_keys().map(|token| token.len()).collect();
    let mid = lens.len() / 2;
    let (_, &mut median, _) = lens.select_nth_unstable(mid);
    median
}

/// Pull the `unk_token` string out of `tokenizer.json` without paying a full
/// `serde_json::Value` parse. On a 1 MB tokenizer file the generic
/// `Value` round-trip was ~20 ms even after dropping the
/// `tokenizer.to_string()` step; deserializing into a tiny struct with
/// `Serde(deny_unknown_fields = false)` keeps the JSON parser running but
/// skips heap allocations for every irrelevant token in the vocab. We
/// only care about `model.unk_token`, so we model just that.
fn read_unk_token_from_file(path: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct ModelOnly {
        #[serde(default)]
        unk_token: Option<String>,
    }
    #[derive(Deserialize)]
    struct TokenizerHead {
        #[serde(default)]
        model: Option<ModelOnly>,
    }
    let bytes = fs::read(path).ok()?;
    let head: TokenizerHead = serde_json::from_slice(&bytes).ok()?;
    head.model.and_then(|m| m.unk_token)
}

/// Read the `normalize` flag out of `config.json` without parsing the whole
/// file into a `Value`. Mirrors `read_unk_token_from_file` ~ a typed struct
/// skips the per-field allocations the generic `Value` path would do. We
/// only need `normalize`; everything else stays unparsed.
fn read_normalize_from_config(path: &Path) -> Option<bool> {
    #[derive(Deserialize)]
    struct ConfigOnly {
        #[serde(default)]
        normalize: Option<bool>,
    }
    let bytes = fs::read(path).ok()?;
    let cfg: ConfigOnly = serde_json::from_slice(&bytes).ok()?;
    cfg.normalize
}

impl Encoder for Model2VecEncoder {
    fn dim(&self) -> usize {
        self.embeddings.ncols()
    }

    fn encode(&self, texts: &[String]) -> Array2<f32> {
        // Batch encoding is the index-build path; reach for the
        // (potentially lazy) slow `Tokenizer` whose `encode_batch_fast`
        // is rayon-parallel internally. Warm hybrid/semantic search never
        // touches this branch.
        let dim = self.dim();
        let mut output = Array2::<f32>::zeros((texts.len(), dim));
        let truncated: Vec<String> = texts
            .iter()
            .map(|text| truncate_chars(text, 512 * self.median_token_length).to_owned())
            .collect();
        let encodings = self
            .slow_tokenizer()
            .encode_batch_fast::<String>(truncated, false)
            .unwrap_or_default();

        for (row_idx, encoding) in encodings.into_iter().enumerate() {
            let mut token_ids: Vec<u32> = encoding.get_ids().to_vec();
            if let Some(unk) = self.unk_token_id {
                token_ids.retain(|id| *id != unk);
            }
            token_ids.truncate(512);
            if token_ids.is_empty() {
                continue;
            }
            self.fold_embeddings_into_row(&token_ids, &mut output, row_idx);
        }
        if self.normalize {
            for row in output.rows_mut() {
                normalize_row(row);
            }
        }
        output
    }
}

impl Model2VecEncoder {
    /// Pull each token's embedding (with optional `mapping` redirect and
    /// `weights` scaling) into `output[row_idx]`, then normalize by the
    /// number of contributing tokens. Shared by the slow batch path and
    /// the fast query path so the two can never drift on the math.
    fn fold_embeddings_into_row(
        &self,
        token_ids: &[u32],
        output: &mut Array2<f32>,
        row_idx: usize,
    ) {
        let embeddings = self.embeddings.view();
        let dim = embeddings.ncols();
        let nrows = embeddings.nrows();
        let mut count = 0usize;
        for &id in token_ids {
            let token_idx = id as usize;
            let row = self
                .token_mapping
                .as_ref()
                .and_then(|mapping| mapping.get(token_idx).copied())
                .unwrap_or(token_idx);
            if row >= nrows {
                continue;
            }
            let scale = self
                .weights
                .as_ref()
                .and_then(|weights| weights.get(token_idx).copied())
                .unwrap_or(1.0);
            let source = embeddings.row(row);
            for col in 0..dim {
                output[(row_idx, col)] += source[col] * scale;
            }
            count += 1;
        }
        if count > 0 {
            for col in 0..dim {
                output[(row_idx, col)] /= count as f32;
            }
        }
    }
}

impl QueryEncoder for Model2VecEncoder {
    fn encode_query(&self, query: &str) -> Array2<f32> {
        // Fast path: a sorted-byte WordPiece + the official BertNormalizer
        // produce an identical token-id stream to `Tokenizer::encode_batch_fast`
        // without paying the per-warm-call HashMap rebuild. Falls through
        // to the slow batched path only for tokenizers we couldn't recognize
        // as BertNormalizer+BertPreTokenizer+WordPiece at load time.
        let Some(fast) = self.fast.as_ref() else {
            return self.encode(&[query.to_owned()]);
        };
        let dim = self.dim();
        let mut output = Array2::<f32>::zeros((1, dim));
        let truncated = truncate_chars(query, 512 * self.median_token_length);
        let mut token_ids: Vec<u32> = Vec::new();
        fast.dim_independent_encode(truncated, &mut token_ids);
        if let Some(unk) = self.unk_token_id {
            token_ids.retain(|id| *id != unk);
        }
        if token_ids.len() > 512 {
            token_ids.truncate(512);
        }
        if !token_ids.is_empty() {
            self.fold_embeddings_into_row(&token_ids, &mut output, 0);
        }
        if self.normalize {
            normalize_row(output.row_mut(0));
        }
        output
    }
}

pub struct HashingEncoder {
    dim: usize,
}

impl HashingEncoder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

impl Encoder for HashingEncoder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn encode(&self, texts: &[String]) -> Array2<f32> {
        let mut matrix = Array2::<f32>::zeros((texts.len(), self.dim));
        for (row, text) in texts.iter().enumerate() {
            for tok in bm25_tokenize(text) {
                let mut hasher = DefaultHasher::new();
                tok.hash(&mut hasher);
                let h = hasher.finish();
                let idx = (h as usize) % self.dim;
                let sign = if (h >> 63) == 0 { 1.0 } else { -1.0 };
                matrix[(row, idx)] += sign;
            }
            normalize_row(matrix.row_mut(row));
        }
        matrix
    }
}

impl QueryEncoder for HashingEncoder {
    fn encode_query(&self, query: &str) -> Array2<f32> {
        self.encode(&[query.to_owned()])
    }
}

fn model_files(options: &ModelOptions) -> AppResult<(PathBuf, PathBuf, PathBuf)> {
    let path = Path::new(&options.model);
    if path.exists() {
        return Ok((
            path.join("tokenizer.json"),
            path.join("model.safetensors"),
            path.join("config.json"),
        ));
    }
    if options.policy != ModelLoadPolicy::AllowDownload {
        let status = model_status(Some(&options.model));
        if let (Some(tokenizer), Some(model), Some(config)) =
            (status.tokenizer, status.safetensors, status.config)
        {
            return Ok((tokenizer, model, config));
        }
        return Err(AppError::not_found(format!(
            "model {:?} is not available locally; pass --hashing or omit --offline/--no-download",
            options.model
        )));
    }
    let api = hf_hub::api::sync::ApiBuilder::from_env()
        .build()
        .map_err(|err| AppError::internal(format!("hf-hub init failed: {err}")))?;
    let repo = api.model(options.model.clone());
    let tokenizer = repo
        .get("tokenizer.json")
        .map_err(|err| AppError::internal(format!("download tokenizer.json: {err}")))?;
    let model = repo
        .get("model.safetensors")
        .map_err(|err| AppError::internal(format!("download model.safetensors: {err}")))?;
    let config = repo
        .get("config.json")
        .map_err(|err| AppError::internal(format!("download config.json: {err}")))?;
    Ok((tokenizer, model, config))
}

fn existing_file(path: PathBuf) -> Option<PathBuf> {
    path.exists().then_some(path)
}

fn hash_file(path: &Path, hasher: &mut DefaultHasher) -> AppResult<()> {
    let bytes = fs::read(path)
        .map_err(|err| AppError::internal(format!("read model file {}: {err}", path.display())))?;
    path.file_name().hash(hasher);
    bytes.hash(hasher);
    Ok(())
}

/// Parallel f32 matrix read. The embedding matrix is multi-MB and the
/// per-row write pattern is independent, so striping the copy across
/// rayon workers lets the memory bus carry parallel writes instead of
/// bottlenecking on a single thread. Falls back to the sequential
/// `read_f32_data` path for non-f32 dtypes (the parallel-vs-serial cost
/// crossover is well above what f64/f16 inputs hit in practice).
fn read_f32_matrix_parallel(
    raw: &[u8],
    rows: usize,
    cols: usize,
    dtype: Dtype,
) -> AppResult<Array2<f32>> {
    if dtype != Dtype::F32 {
        let values = read_f32_data(dtype, raw)?;
        return Array2::from_shape_vec((rows, cols), values)
            .map_err(|err| AppError::internal(format!("reshape embeddings: {err}")));
    }
    let total = rows
        .checked_mul(cols)
        .ok_or_else(|| AppError::internal("embedding matrix dimensions overflow".to_string()))?;
    let body = total
        .checked_mul(4)
        .ok_or_else(|| AppError::internal("embedding matrix bytes overflow".to_string()))?;
    if body > raw.len() {
        return Err(AppError::internal(format!(
            "embeddings tensor truncated: header asks for {body} bytes, have {}",
            raw.len()
        )));
    }
    let mut buffer: Vec<f32> = Vec::with_capacity(total);
    // Safety: `total` matches `cols * rows`; the parallel copy below
    // initializes every element before we hand the Vec out.
    unsafe { buffer.set_len(total) };
    let dst_bytes: &mut [u8] =
        unsafe { std::slice::from_raw_parts_mut(buffer.as_mut_ptr() as *mut u8, body) };
    // 1 MiB stripes keep cache pressure local on each worker while still
    // letting rayon issue several outstanding writes in parallel. Empirically
    // this hits the WSL2 memory bus's parallel-write sweet spot; coarser
    // stripes leave workers idle on small models, finer stripes thrash L2.
    const STRIPE: usize = 1024 * 1024;
    dst_bytes
        .par_chunks_mut(STRIPE)
        .enumerate()
        .for_each(|(idx, dst_chunk)| {
            let offset = idx * STRIPE;
            let len = dst_chunk.len();
            dst_chunk.copy_from_slice(&raw[offset..offset + len]);
        });
    if !cfg!(target_endian = "little") {
        for v in buffer.iter_mut() {
            *v = f32::from_bits(v.to_bits().swap_bytes());
        }
    }
    Array2::from_shape_vec((rows, cols), buffer)
        .map_err(|err| AppError::internal(format!("reshape embeddings: {err}")))
}

fn read_f32_data(dtype: Dtype, raw: &[u8]) -> AppResult<Vec<f32>> {
    match dtype {
        // Safetensors stores f32 as LE bytes. On the platforms we ship to
        // (x86_64 / aarch64 LE) this collapses to a single memcpy via
        // `copy_from_slice`; the previous `chunks_exact(4).map(...).collect()`
        // loop didn't auto-vectorize and cost ~35 ms on a 32 MB embedding
        // matrix.
        Dtype::F32 => {
            let len = raw.len() / 4;
            let mut out: Vec<f32> = Vec::with_capacity(len);
            unsafe { out.set_len(len) };
            let dst: &mut [u8] =
                unsafe { std::slice::from_raw_parts_mut(out.as_mut_ptr() as *mut u8, len * 4) };
            dst.copy_from_slice(&raw[..len * 4]);
            if !cfg!(target_endian = "little") {
                for v in out.iter_mut() {
                    *v = f32::from_bits(v.to_bits().swap_bytes());
                }
            }
            Ok(out)
        }
        Dtype::F64 => Ok(raw
            .chunks_exact(8)
            .map(|b| f64::from_le_bytes(b.try_into().unwrap()) as f32)
            .collect()),
        Dtype::F16 => Ok(raw.chunks_exact(2).map(half_from_le_bytes).collect()),
        other => Err(AppError::internal(format!(
            "unsupported float tensor dtype: {other:?}"
        ))),
    }
}

fn read_usize_data(dtype: Dtype, raw: &[u8]) -> AppResult<Vec<usize>> {
    match dtype {
        Dtype::I64 => Ok(raw
            .chunks_exact(8)
            .map(|b| i64::from_le_bytes(b.try_into().unwrap()) as usize)
            .collect()),
        Dtype::I32 => Ok(raw
            .chunks_exact(4)
            .map(|b| i32::from_le_bytes(b.try_into().unwrap()) as usize)
            .collect()),
        Dtype::U64 => Ok(raw
            .chunks_exact(8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()) as usize)
            .collect()),
        Dtype::U32 => Ok(raw
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
            .collect()),
        other => Err(AppError::internal(format!(
            "unsupported mapping tensor dtype: {other:?}"
        ))),
    }
}

fn half_from_le_bytes(bytes: &[u8]) -> f32 {
    let bits = u16::from_le_bytes([bytes[0], bytes[1]]);
    half::f16::from_bits(bits).to_f32()
}

fn truncate_chars(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &text[..byte_idx],
        None => text,
    }
}

fn normalize_row(mut row: ndarray::ArrayViewMut1<'_, f32>) {
    let norm = row.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for v in row.iter_mut() {
            *v /= norm;
        }
    }
}
