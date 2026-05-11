//! SQLite round-trip for the persisted search index.
//!
//! Encodes the in-memory `Bm25Index`, chunk vector, file/language mappings,
//! and per-file signatures via bincode and stores them as BLOBs through
//! `cache.rs`. Dense vectors live in a sibling `dense.v10.bin` file as a
//! raw f32 little-endian matrix; the load path mmaps that and copies the
//! bytes into an `Array2<f32>` in one shot, skipping the multi-MB serde
//! pass that bincode used to dominate warm loads with. Mirror functions
//! decode on the way back. Sparse and dense are independent rows / files ~
//! a model swap rebuilds dense without touching sparse, and BM25-only mode
//! never writes the dense row at all.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use ndarray::Array2;
use serde::{Deserialize, Serialize};

use crate::bin_codec;
use crate::cache::{LanguageCountsBlob, ParseCache, SearchDenseRow, SearchSparseRow};
use crate::error::{AppError, AppResult};

use super::chunk_store::ChunkStore;
use super::dense::DenseIndex;
use super::sparse::Bm25Index;
use super::types::FileSignature;

/// Persisted dtype tag for `vectors_blob` in SQLite metadata. Always "f32"
/// today; the column is kept so a future dtype change can tag rows without
/// another schema bump. The actual bytes live in `dense.v10.bin`.
const DENSE_DTYPE_F32: &str = "f32";
/// On-disk magic for the raw-f32 dense matrix file. Bump if the byte layout
/// changes shape; size-prefixed fields ride the same tag.
const DENSE_FILE_MAGIC: u32 = 0xDE_5E_F032;
/// Fixed-width header on `dense.v10.bin`: magic + rows + cols + reserved word.
const DENSE_HEADER_BYTES: usize = 16;

/// All sparse-side state for one indexed repo. Held in memory after a load
/// or build; written/read as a single SQLite row.
#[derive(Clone, Debug)]
pub struct SparsePayload {
    pub bm25: Bm25Index,
    pub chunks: ChunkStore,
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
    /// Cheap stat-tuple of the model files used to build these vectors. Lets
    /// the next process skip rehashing if the files haven't changed.
    pub model_files_meta: String,
}

#[derive(Serialize, Deserialize)]
struct SignaturesBlob {
    signatures: Vec<FileSignature>,
}

#[derive(Serialize)]
struct SignaturesBlobRef<'a> {
    signatures: &'a [FileSignature],
}

#[derive(Serialize, Deserialize)]
struct MappingBlob {
    map: BTreeMap<String, Vec<usize>>,
}

#[derive(Serialize)]
struct MappingBlobRef<'a> {
    map: &'a BTreeMap<String, Vec<usize>>,
}

pub fn load_sparse(cache: &mut ParseCache) -> AppResult<Option<SparsePayload>> {
    let Some(row) = cache.lookup_search_sparse() else {
        return Ok(None);
    };
    // The four blobs decode independently and together cost ~30 ms on a
    // 2 k-file repo (mostly the BM25 parallel vectors and the chunk store
    // packed binary). Fan them out via `rayon::join` so the longest single
    // decode bounds the wall time instead of their sum.
    let (bm25_chunks, mappings_sigs) = rayon::join(
        || -> AppResult<(Bm25Index, ChunkStore)> {
            let (bm25, chunks) = rayon::join(
                || -> AppResult<Bm25Index> {
                    bin_codec::decode(&row.bm25_blob)
                        .map_err(|err| AppError::internal(format!("decode sparse bm25: {err}")))
                },
                || -> AppResult<ChunkStore> {
                    ChunkStore::decode_from_bytes(&row.chunks_blob)
                        .map_err(|err| AppError::internal(format!("decode sparse chunks: {err}")))
                },
            );
            Ok((bm25?, chunks?))
        },
        || -> AppResult<(MappingBlob, MappingBlob, SignaturesBlob)> {
            let file_mapping: MappingBlob = bin_codec::decode(&row.file_mapping_blob)
                .map_err(|err| AppError::internal(format!("decode sparse file mapping: {err}")))?;
            let language_mapping: MappingBlob = bin_codec::decode(&row.language_mapping_blob)
                .map_err(|err| {
                    AppError::internal(format!("decode sparse language mapping: {err}"))
                })?;
            let signatures: SignaturesBlob = bin_codec::decode(&row.signatures_blob)
                .map_err(|err| AppError::internal(format!("decode sparse signatures: {err}")))?;
            Ok((file_mapping, language_mapping, signatures))
        },
    );
    let (bm25, chunks) = bm25_chunks?;
    let (file_mapping, language_mapping, signatures) = mappings_sigs?;
    Ok(Some(SparsePayload {
        bm25,
        chunks,
        file_mapping: file_mapping.map,
        language_mapping: language_mapping.map,
        signatures: signatures.signatures,
        built_at_unix_secs: row.built_at_unix_secs,
    }))
}

pub fn store_sparse(cache: &mut ParseCache, payload: &SparsePayload) -> AppResult<()> {
    let bm25_blob = bin_codec::encode(&payload.bm25)
        .map_err(|err| AppError::internal(format!("encode sparse bm25: {err}")))?;
    // Borrowed wrappers so we don't clone the multi-MB mappings vectors
    // just to pass them into the bincode encoder. ChunkStore writes its
    // own packed binary format directly.
    let chunks_blob = payload.chunks.encode_to_bytes();
    let file_mapping_blob = bin_codec::encode(&MappingBlobRef {
        map: &payload.file_mapping,
    })
    .map_err(|err| AppError::internal(format!("encode sparse file mapping: {err}")))?;
    let language_mapping_blob = bin_codec::encode(&MappingBlobRef {
        map: &payload.language_mapping,
    })
    .map_err(|err| AppError::internal(format!("encode sparse language mapping: {err}")))?;
    let signatures_blob = bin_codec::encode(&SignaturesBlobRef {
        signatures: &payload.signatures,
    })
    .map_err(|err| AppError::internal(format!("encode sparse signatures: {err}")))?;
    let language_counts_blob = bin_codec::encode(&LanguageCountsBlob {
        counts: payload
            .language_mapping
            .iter()
            .map(|(lang, ids)| (lang.clone(), ids.len()))
            .collect(),
    })
    .map_err(|err| AppError::internal(format!("encode sparse language counts: {err}")))?;
    cache.store_search_sparse(&SearchSparseRow {
        bm25_blob,
        signatures_blob,
        chunks_blob,
        file_mapping_blob,
        language_mapping_blob,
        built_at_unix_secs: payload.built_at_unix_secs,
        indexed_files: payload.file_mapping.len(),
        indexed_chunks: payload.chunks.len(),
        language_counts_blob,
    });
    Ok(())
}

pub fn load_dense(cache: &mut ParseCache) -> AppResult<Option<DensePayload>> {
    let Some(row) = cache.lookup_search_dense() else {
        return Ok(None);
    };
    if row.vectors_dtype != DENSE_DTYPE_F32 && !row.vectors_dtype.is_empty() {
        return Err(AppError::internal(format!(
            "unknown dense vectors_dtype {:?}",
            row.vectors_dtype
        )));
    }
    let dense_path = cache.dense_blob_path();
    if !dense_path.exists() {
        // Metadata says we built dense, but the sibling file is gone.
        // Treat as a cache miss; the caller will rebuild and re-persist.
        return Ok(None);
    }
    let vectors = read_dense_vectors_file(&dense_path, Some(row.dim))?;
    if vectors.ncols() != row.dim {
        return Err(AppError::internal(format!(
            "dense row dim {} doesn't match vectors {}",
            row.dim,
            vectors.ncols()
        )));
    }
    Ok(Some(DensePayload {
        // Vectors were unit-normalized before persistence in `store_dense`,
        // so `from_normalized` skips the per-row L2 sweep that the generic
        // constructor would otherwise do.
        vectors: DenseIndex::from_normalized(vectors),
        encoder_kind: row.encoder_kind,
        model_id: row.model_id,
        model_fingerprint: row.model_fingerprint,
        built_at_unix_secs: row.built_at_unix_secs,
        model_files_meta: row.model_files_meta,
    }))
}

/// Standalone, ParseCache-free dense load. Used by the search command to
/// mmap + memcpy the dense matrix on a rayon worker alongside the sparse
/// and encoder loads ~ the SQLite metadata round-trip happens later on the
/// main thread, where we cross-check this matrix's shape against the
/// encoder's dim and the sparse payload's chunk count before adopting it.
/// Returns `None` for any benign miss (no cache dir, file absent, decode
/// error) so the slow path can fall through to `ensure_dense` / `build_dense`
/// as if no speculative load had happened.
pub fn load_dense_vectors_speculative(dense_path: &Path) -> Option<Array2<f32>> {
    if !dense_path.exists() {
        return None;
    }
    read_dense_vectors_file(dense_path, None).ok()
}

pub fn store_dense(
    cache: &mut ParseCache,
    payload: &DensePayload,
    raw_vectors: &Array2<f32>,
) -> AppResult<()> {
    let dense_path = cache.dense_blob_path();
    write_dense_vectors_file(&dense_path, raw_vectors)?;
    // The metadata row no longer carries the vector bytes; an empty
    // sentinel keeps the NOT NULL column happy without a schema change.
    cache.store_search_dense(&SearchDenseRow {
        encoder_kind: payload.encoder_kind.clone(),
        model_id: payload.model_id.clone(),
        model_fingerprint: payload.model_fingerprint.clone(),
        dim: payload.vectors.dim(),
        vectors_dtype: DENSE_DTYPE_F32.to_string(),
        vectors_blob: Vec::new(),
        built_at_unix_secs: payload.built_at_unix_secs,
        model_files_meta: payload.model_files_meta.clone(),
    });
    Ok(())
}

/// mmap `dense.v10.bin` and copy the raw f32 LE bytes into a fresh `Array2`.
/// `expected_dim` lets us reject a stale file where SQLite metadata and the
/// blob disagreed (e.g. a partial write). On a typical 32 MB matrix the
/// `copy_from_slice` is a single memcpy, roughly 10x faster than the bincode
/// pass it replaces.
fn read_dense_vectors_file(path: &Path, expected_dim: Option<usize>) -> AppResult<Array2<f32>> {
    let file = fs::File::open(path).map_err(|err| {
        AppError::internal(format!(
            "open dense vectors file {}: {err}",
            path.display()
        ))
    })?;
    let mmap = unsafe { memmap2::Mmap::map(&file) }
        .map_err(|err| AppError::internal(format!("mmap dense vectors: {err}")))?;
    let bytes: &[u8] = &mmap;
    if bytes.len() < DENSE_HEADER_BYTES {
        return Err(AppError::internal(format!(
            "dense vectors file too small ({} bytes)",
            bytes.len()
        )));
    }
    let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    if magic != DENSE_FILE_MAGIC {
        return Err(AppError::internal(format!(
            "dense vectors file magic mismatch: got {magic:#x}",
        )));
    }
    let rows = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let cols = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    if let Some(expected) = expected_dim {
        if cols != expected {
            return Err(AppError::internal(format!(
                "dense vectors file dim {cols} doesn't match metadata {expected}"
            )));
        }
    }
    let expected_body = rows
        .checked_mul(cols)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| AppError::internal("dense vectors size overflow".to_string()))?;
    if bytes.len() < DENSE_HEADER_BYTES + expected_body {
        return Err(AppError::internal(format!(
            "dense vectors file truncated: header asks for {} bytes, have {}",
            DENSE_HEADER_BYTES + expected_body,
            bytes.len()
        )));
    }
    let mut buffer: Vec<f32> = Vec::with_capacity(rows * cols);
    // Safety: the Vec has capacity rows*cols; we initialize every element via
    // copy_from_slice before observing the values.
    unsafe { buffer.set_len(rows * cols) };
    let dst: &mut [u8] = unsafe {
        std::slice::from_raw_parts_mut(buffer.as_mut_ptr() as *mut u8, expected_body)
    };
    dst.copy_from_slice(&bytes[DENSE_HEADER_BYTES..DENSE_HEADER_BYTES + expected_body]);
    Array2::from_shape_vec((rows, cols), buffer)
        .map_err(|err| AppError::internal(format!("reshape dense vectors: {err}")))
}

/// Write `vectors` as a raw f32 LE matrix with a 16-byte header. Writes go
/// through a temp file + rename so an interrupted store can't leave a
/// half-written matrix that the next reader would interpret as the
/// expected shape.
fn write_dense_vectors_file(path: &Path, vectors: &Array2<f32>) -> AppResult<()> {
    let (rows, cols) = vectors.dim();
    let body_bytes = rows
        .checked_mul(cols)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| AppError::internal("dense vectors size overflow".to_string()))?;
    let mut out: Vec<u8> = Vec::with_capacity(DENSE_HEADER_BYTES + body_bytes);
    out.extend_from_slice(&DENSE_FILE_MAGIC.to_le_bytes());
    out.extend_from_slice(&(rows as u32).to_le_bytes());
    out.extend_from_slice(&(cols as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    // ndarray::Array2 holds its f32 cells in row-major C order by default
    // (which is what `from_shape_vec` reconstructs), so a single slice cast
    // matches the layout the loader expects.
    let slice = vectors
        .as_slice()
        .ok_or_else(|| AppError::internal("dense vectors not contiguous".to_string()))?;
    let body_view: &[u8] =
        unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u8, body_bytes) };
    out.extend_from_slice(body_view);
    atomic_write_dense(path, &out)
}

fn atomic_write_dense(path: &Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|err| {
                AppError::internal(format!(
                    "create dense vectors parent {}: {err}",
                    parent.display()
                ))
            })?;
        }
    }
    let tmp = path.with_extension("bin.tmp");
    fs::write(&tmp, bytes).map_err(|err| {
        AppError::internal(format!("write dense vectors tmp {}: {err}", tmp.display()))
    })?;
    fs::rename(&tmp, path).map_err(|err| {
        AppError::internal(format!("rename dense vectors {}: {err}", path.display()))
    })?;
    Ok(())
}
