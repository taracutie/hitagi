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

use ndarray::Array2;
use rayon::prelude::*;
use safetensors::tensor::Dtype;
use safetensors::SafeTensors;
use serde::Deserialize;
use tokenizers::Tokenizer;

use crate::error::{AppError, AppResult};

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
                .or_else(|| std::env::var("HITAGI_MODEL").ok())
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
        .or_else(|| std::env::var("HITAGI_MODEL").ok())
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
    tokenizer: Tokenizer,
    embeddings: Array2<f32>,
    weights: Option<Vec<f32>>,
    token_mapping: Option<Vec<usize>>,
    normalize: bool,
    median_token_length: usize,
    unk_token_id: Option<u32>,
}

impl Model2VecEncoder {
    pub fn from_options(options: &ModelOptions) -> AppResult<Self> {
        let (tokenizer_path, model_path, config_path) = model_files(options)?;
        // Tokenizer parsing (~85 ms on a 1 MB tokenizer.json) is the long
        // pole. The embeddings table load (~55 ms paging 30 MB of f32 from
        // disk) and the side-channel JSON reads (config + unk_token) are
        // independent; we run all three on rayon workers alongside the
        // tokenizer so only the tokenizer parse remains on the wall, with
        // everything else folded under it. `unk_token_id`'s final hashmap
        // lookup is the only step that has to wait for both branches.
        // Returns `(Tokenizer, optional_precomputed_median, optional_unk_id)`
        // so the warm cache path can skip the second pass over the 60 k
        // vocab to derive those numbers when they were already stamped at
        // write time.
        let load_tokenizer = || -> AppResult<(Tokenizer, Option<usize>, Option<Option<u32>>)> {
            // Try the binary fast path first ~ rebuilding via
            // `WordPiece::builder() + BertNormalizer + BertPreTokenizer` from
            // a cached vocab + flag blob is ~15-25 ms vs ~75-85 ms for the
            // full JSON parse. The cache is invalidated on tokenizer.json
            // (mtime, size) change so a model swap re-parses the source.
            if let Some(cached) = super::tokenizer_cache::try_load(&tokenizer_path) {
                return Ok((
                    cached.tokenizer,
                    Some(cached.median_token_length),
                    Some(cached.unk_token_id),
                ));
            }
            let tk = Tokenizer::from_file(&tokenizer_path).map_err(|err| {
                AppError::internal(format!(
                    "failed to load tokenizer from {}: {err}",
                    tokenizer_path.display()
                ))
            })?;
            // Cache write happens after the caller computes median +
            // unk_token_id ~ we plumb them through so the next warm load
            // can skip recomputing.
            Ok((tk, None, None))
        };
        let load_tensors = || -> AppResult<(Array2<f32>, Option<Vec<f32>>, Option<Vec<usize>>)> {
            // mmap the safetensors file ~ on warm cache the bytes are
            // already in the OS page cache and the embedding matrix copy
            // hits memory bandwidth (not disk).
            let model_file = fs::File::open(&model_path).map_err(AppError::from)?;
            let mmap = unsafe { memmap2::Mmap::map(&model_file) }
                .map_err(|err| AppError::internal(format!("mmap safetensors: {err}")))?;
            let tensors = SafeTensors::deserialize(&mmap)
                .map_err(|err| AppError::internal(format!("failed to read safetensors: {err}")))?;
            // The 63 MB embeddings memcpy dominates the load. Split it into
            // strided chunks so the writes happen in parallel ~ on 8+ cores
            // we move from a single-thread 5-6 GB/s pipe to whatever the
            // memory bus can sustain across rayon workers (~2x on the WSL2
            // box this is tuned against). Weights + mapping are tiny and
            // ride on the calling thread alongside the parallel fanout.
            let embeddings_tensor = tensors
                .tensor("embeddings")
                .map_err(|err| AppError::internal(format!("missing embeddings tensor: {err}")))?;
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
            let embeddings = read_f32_matrix_parallel(data, rows, cols, embeddings_tensor.dtype())?;
            drop(tensors);
            drop(mmap);
            Ok((embeddings, weights, token_mapping))
        };
        // Sidecar: scan tokenizer.json for `unk_token` and load config.json.
        // Both are <20 ms in serial; running here hides them entirely behind
        // the tokenizer parse. Returns `(unk_token, normalize)`. Failures are
        // benign defaults so we never block the encoder on a missing field.
        let load_sidecar = || -> (Option<String>, bool) {
            let unk = read_unk_token_from_file(&tokenizer_path);
            let normalize = read_normalize_from_config(&config_path).unwrap_or(true);
            (unk, normalize)
        };
        // Two-way join keeps the existing thread budget; the sidecar runs on
        // the calling thread alongside the tensors branch so the tokenizer
        // parse stays alone on a worker (its own JSON parse is single-
        // threaded inside the tokenizers crate, so an extra worker wouldn't
        // help it).
        let tensors_then_sidecar = || -> AppResult<(
            (Array2<f32>, Option<Vec<f32>>, Option<Vec<usize>>),
            (Option<String>, bool),
        )> {
            let (tensors, sidecar) = rayon::join(load_tensors, load_sidecar);
            Ok((tensors?, sidecar))
        };
        let (tokenizer_result, tensors_sidecar_result) =
            rayon::join(load_tokenizer, tensors_then_sidecar);
        let (tokenizer, cached_median, cached_unk_id) = tokenizer_result?;
        let ((embeddings, weights, token_mapping), (unk_token, normalize)) =
            tensors_sidecar_result?;
        // Cache hit path skips re-iterating the 60 k vocab; only the slow
        // (JSON parse) path needs the derivations + the write-back.
        let median_token_length = match cached_median {
            Some(m) => m,
            None => median_token_byte_len(&tokenizer),
        };
        let unk_token = unk_token.unwrap_or_else(|| "[UNK]".to_string());
        let unk_token_id = match cached_unk_id {
            Some(id) => id,
            None => {
                let id = tokenizer.token_to_id(&unk_token);
                // We took the slow JSON path above; stamp the cache so the
                // next warm load takes the binary path.
                super::tokenizer_cache::write(&tokenizer_path, &tokenizer, median_token_length, id);
                id
            }
        };

        Ok(Self {
            tokenizer,
            embeddings,
            weights,
            token_mapping,
            normalize,
            median_token_length,
            unk_token_id,
        })
    }
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
        let dim = self.dim();
        let mut output = Array2::<f32>::zeros((texts.len(), dim));
        let truncated: Vec<String> = texts
            .iter()
            .map(|text| truncate_chars(text, 512 * self.median_token_length).to_owned())
            .collect();
        let encodings = self
            .tokenizer
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
            let mut count = 0usize;
            for id in token_ids {
                let token_idx = id as usize;
                let row = self
                    .token_mapping
                    .as_ref()
                    .and_then(|mapping| mapping.get(token_idx).copied())
                    .unwrap_or(token_idx);
                if row >= self.embeddings.nrows() {
                    continue;
                }
                let scale = self
                    .weights
                    .as_ref()
                    .and_then(|weights| weights.get(token_idx).copied())
                    .unwrap_or(1.0);
                let source = self.embeddings.row(row);
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
        if self.normalize {
            for row in output.rows_mut() {
                normalize_row(row);
            }
        }
        output
    }
}

impl QueryEncoder for Model2VecEncoder {
    fn encode_query(&self, query: &str) -> Array2<f32> {
        self.encode(&[query.to_owned()])
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
    let total = rows.checked_mul(cols).ok_or_else(|| {
        AppError::internal("embedding matrix dimensions overflow".to_string())
    })?;
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
    let dst_bytes: &mut [u8] = unsafe {
        std::slice::from_raw_parts_mut(buffer.as_mut_ptr() as *mut u8, body)
    };
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
