//! SQLite round-trip for the persisted search index.
//!
//! Encodes the in-memory `Bm25Index`, dense `Array2<f32>`, chunk vector,
//! file/language mappings, and per-file signatures via bincode and stores
//! them as BLOBs through `cache.rs`. Mirror functions decode on the way
//! back. Sparse and dense are independent rows ~ a model swap rebuilds
//! dense without touching sparse, and BM25-only mode never writes the
//! dense row at all.

use std::collections::BTreeMap;

use ndarray::Array2;
use serde::{Deserialize, Serialize};

use crate::bin_codec;
use crate::cache::{ParseCache, SearchDenseRow, SearchSparseRow};
use crate::error::{AppError, AppResult};

use super::dense::DenseIndex;
use super::sparse::Bm25Index;
use super::types::{FileSignature, IndexedChunk};

/// All sparse-side state for one indexed repo. Held in memory after a load
/// or build; written/read as a single SQLite row.
#[derive(Clone, Debug)]
pub struct SparsePayload {
    pub bm25: Bm25Index,
    pub chunks: Vec<IndexedChunk>,
    pub file_mapping: BTreeMap<String, Vec<usize>>,
    pub language_mapping: BTreeMap<String, Vec<usize>>,
    pub signatures: Vec<FileSignature>,
    pub built_at_unix_secs: i64,
}

#[derive(Clone, Debug)]
pub struct DensePayload {
    pub vectors: DenseIndex,
    pub encoder_kind: String,
    pub model_id: String,
    pub model_fingerprint: String,
    pub built_at_unix_secs: i64,
}

#[derive(Serialize, Deserialize)]
struct ChunksBlob {
    chunks: Vec<IndexedChunk>,
}

#[derive(Serialize, Deserialize)]
struct SignaturesBlob {
    signatures: Vec<FileSignature>,
}

#[derive(Serialize, Deserialize)]
struct MappingBlob {
    map: BTreeMap<String, Vec<usize>>,
}

pub fn load_sparse(cache: &mut ParseCache) -> AppResult<Option<SparsePayload>> {
    let Some(row) = cache.lookup_search_sparse() else {
        return Ok(None);
    };
    let bm25: Bm25Index = bin_codec::decode(&row.bm25_blob)
        .map_err(|err| AppError::internal(format!("decode sparse bm25: {err}")))?;
    let chunks: ChunksBlob = bin_codec::decode(&row.chunks_blob)
        .map_err(|err| AppError::internal(format!("decode sparse chunks: {err}")))?;
    let file_mapping: MappingBlob = bin_codec::decode(&row.file_mapping_blob)
        .map_err(|err| AppError::internal(format!("decode sparse file mapping: {err}")))?;
    let language_mapping: MappingBlob = bin_codec::decode(&row.language_mapping_blob)
        .map_err(|err| AppError::internal(format!("decode sparse language mapping: {err}")))?;
    let signatures: SignaturesBlob = bin_codec::decode(&row.signatures_blob)
        .map_err(|err| AppError::internal(format!("decode sparse signatures: {err}")))?;
    Ok(Some(SparsePayload {
        bm25,
        chunks: chunks.chunks,
        file_mapping: file_mapping.map,
        language_mapping: language_mapping.map,
        signatures: signatures.signatures,
        built_at_unix_secs: row.built_at_unix_secs,
    }))
}

pub fn store_sparse(cache: &mut ParseCache, payload: &SparsePayload) -> AppResult<()> {
    let bm25_blob = bin_codec::encode(&payload.bm25)
        .map_err(|err| AppError::internal(format!("encode sparse bm25: {err}")))?;
    let chunks_blob = bin_codec::encode(&ChunksBlob {
        chunks: payload.chunks.clone(),
    })
    .map_err(|err| AppError::internal(format!("encode sparse chunks: {err}")))?;
    let file_mapping_blob = bin_codec::encode(&MappingBlob {
        map: payload.file_mapping.clone(),
    })
    .map_err(|err| AppError::internal(format!("encode sparse file mapping: {err}")))?;
    let language_mapping_blob = bin_codec::encode(&MappingBlob {
        map: payload.language_mapping.clone(),
    })
    .map_err(|err| AppError::internal(format!("encode sparse language mapping: {err}")))?;
    let signatures_blob = bin_codec::encode(&SignaturesBlob {
        signatures: payload.signatures.clone(),
    })
    .map_err(|err| AppError::internal(format!("encode sparse signatures: {err}")))?;
    cache.store_search_sparse(&SearchSparseRow {
        bm25_blob,
        signatures_blob,
        chunks_blob,
        file_mapping_blob,
        language_mapping_blob,
        built_at_unix_secs: payload.built_at_unix_secs,
    });
    Ok(())
}

pub fn load_dense(cache: &mut ParseCache) -> AppResult<Option<DensePayload>> {
    let Some(row) = cache.lookup_search_dense() else {
        return Ok(None);
    };
    let vectors: Array2<f32> = bin_codec::decode(&row.vectors_blob)
        .map_err(|err| AppError::internal(format!("decode dense vectors: {err}")))?;
    if vectors.ncols() != row.dim {
        return Err(AppError::internal(format!(
            "dense row dim {} doesn't match vectors {}",
            row.dim,
            vectors.ncols()
        )));
    }
    Ok(Some(DensePayload {
        vectors: DenseIndex::new(vectors),
        encoder_kind: row.encoder_kind,
        model_id: row.model_id,
        model_fingerprint: row.model_fingerprint,
        built_at_unix_secs: row.built_at_unix_secs,
    }))
}

pub fn store_dense(
    cache: &mut ParseCache,
    payload: &DensePayload,
    raw_vectors: &Array2<f32>,
) -> AppResult<()> {
    let vectors_blob = bin_codec::encode(raw_vectors)
        .map_err(|err| AppError::internal(format!("encode dense vectors: {err}")))?;
    cache.store_search_dense(&SearchDenseRow {
        encoder_kind: payload.encoder_kind.clone(),
        model_id: payload.model_id.clone(),
        model_fingerprint: payload.model_fingerprint.clone(),
        dim: payload.vectors.dim(),
        vectors_blob,
        built_at_unix_secs: payload.built_at_unix_secs,
    });
    Ok(())
}
