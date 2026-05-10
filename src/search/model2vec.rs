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
use safetensors::tensor::Dtype;
use safetensors::SafeTensors;
use serde_json::Value;
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
        let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|err| {
            AppError::internal(format!(
                "failed to load tokenizer from {}: {err}",
                tokenizer_path.display()
            ))
        })?;
        let mut lens: Vec<usize> = tokenizer
            .get_vocab(false)
            .keys()
            .map(|token| token.len())
            .collect();
        lens.sort_unstable();
        let median_token_length = lens.get(lens.len() / 2).copied().unwrap_or(1);

        let config: Value =
            serde_json::from_reader(fs::File::open(&config_path).map_err(AppError::from)?)
                .map_err(|err| AppError::internal(format!("failed to read model config: {err}")))?;
        let normalize = config
            .get("normalize")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let tokenizer_json: Value =
            serde_json::from_str(&tokenizer.to_string(false).map_err(|err| {
                AppError::internal(format!("failed to serialize tokenizer json: {err}"))
            })?)
            .map_err(|err| AppError::internal(format!("failed to parse tokenizer json: {err}")))?;
        let unk_token = tokenizer_json
            .get("model")
            .and_then(|model| model.get("unk_token"))
            .and_then(Value::as_str)
            .unwrap_or("[UNK]");
        let unk_token_id = tokenizer.token_to_id(unk_token);

        let bytes = fs::read(&model_path).map_err(AppError::from)?;
        let tensors = SafeTensors::deserialize(&bytes)
            .map_err(|err| AppError::internal(format!("failed to read safetensors: {err}")))?;
        let embeddings = read_matrix(&tensors, "embeddings")?;
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

fn read_matrix(tensors: &SafeTensors<'_>, name: &str) -> AppResult<Array2<f32>> {
    let tensor = tensors
        .tensor(name)
        .map_err(|err| AppError::internal(format!("missing tensor {name}: {err}")))?;
    let shape = tensor.shape();
    let [rows, cols]: [usize; 2] = shape
        .try_into()
        .map_err(|_| AppError::internal(format!("tensor {name} is not 2D")))?;
    let values = read_f32_data(tensor.dtype(), tensor.data())?;
    Array2::from_shape_vec((rows, cols), values)
        .map_err(|err| AppError::internal(format!("reshape tensor {name}: {err}")))
}

fn read_f32_data(dtype: Dtype, raw: &[u8]) -> AppResult<Vec<f32>> {
    match dtype {
        Dtype::F32 => Ok(raw
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect()),
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
