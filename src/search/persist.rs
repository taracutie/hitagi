//! SQLite round-trip for the persisted search index.
//!
//! Encodes the in-memory `Bm25Index`, chunk vector, file/language mappings,
//! and per-file signatures via bincode and stores them as BLOBs through
//! `cache.rs`. Dense vectors live in a sibling `dense.v10.bin` file as a
//! raw f32 little-endian matrix; on warm loads we mmap that file and point
//! an `ArrayView2` directly at the OS page cache via `MappedDense` ~ the
//! 33 MB memcpy + extra page-fault pass the previous `read_dense_vectors_file`
//! did on every warm search is gone, replaced by a single mmap setup. Same
//! ranking math runs against the same bytes, just without copying them
//! into a fresh `Vec` first. Sparse and dense are independent rows / files
//! ~ a model swap rebuilds dense without touching sparse, and BM25-only
//! mode never writes the dense row at all.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use ndarray::Array2;
use serde::{Deserialize, Serialize};

use crate::bin_codec;
use crate::cache::{LanguageCountsBlob, ParseCache, SearchDenseRow, SearchSparseRow};
use crate::error::{AppError, AppResult};

use super::chunk_store::ChunkStore;
use super::dense::{DenseIndex, MappedDense};
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
static DENSE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// All sparse-side state for one indexed repo. Held in memory after a load
/// or build; written/read as a single SQLite row.
#[derive(Debug)]
pub struct SparsePayload {
    pub bm25: Bm25Index,
    pub chunks: ChunkStore,
    pub file_mapping: BTreeMap<String, Vec<usize>>,
    pub language_mapping: BTreeMap<String, Vec<usize>>,
    pub signatures: Vec<FileSignature>,
    pub built_at_unix_secs: i64,
}

#[derive(Debug)]
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
    // Open both sibling mmaps before the SQLite query so the kernel can
    // start paging them in alongside the small-BLOB SELECT instead of
    // serializing the multi-MB reads after the lookup. `Mmap` setup is
    // a couple of syscalls; the data costs nothing until first touch.
    let chunks_mmap = cache.open_chunks_mmap();
    let bm25_mmap = cache.open_bm25_mmap();
    let Some(row) = cache.lookup_search_sparse() else {
        return Ok(None);
    };
    let Some(chunks_mmap) = chunks_mmap else {
        // Sparse SQLite row exists but the sibling chunks.v13.bin is gone.
        // Treat as a cache miss; the caller will rebuild and re-persist.
        return Ok(None);
    };
    let Some(bm25_mmap) = bm25_mmap else {
        return Ok(None);
    };
    // Three independent decode branches. `ChunkStore::decode_from_mmap`
    // references the multi-MB content body straight into the OS-managed
    // pages and only materializes the small records/paths tables, so the
    // 40+ MB `to_vec` that `decode_from_bytes` used to pay for is gone.
    // BM25's flat format decodes via one `copy_from_slice` per `Vec`
    // straight out of the mmap. The longest single decode bounds the wall.
    let (bm25_chunks, mappings_sigs) = rayon::join(
        || -> AppResult<(Bm25Index, ChunkStore)> {
            let (bm25, chunks) = rayon::join(
                || -> AppResult<Bm25Index> {
                    Bm25Index::decode_from_bytes(&bm25_mmap[..])
                        .map_err(|err| AppError::internal(format!("decode sparse bm25: {err}")))
                },
                || -> AppResult<ChunkStore> {
                    ChunkStore::decode_from_mmap(chunks_mmap)
                        .map_err(|err| AppError::internal(format!("decode sparse chunks: {err}")))
                },
            );
            Ok((bm25?, chunks?))
        },
        || -> AppResult<(MappingBlob, MappingBlob, SignaturesBlob)> {
            // file_mapping, language_mapping, and signatures decode
            // independently. The file_mapping blob is the biggest (one
            // entry per indexed file × varint chunk-id list); splitting
            // it from the other two on a worker pays for the rayon::join
            // overhead even on a small repo.
            let (file_mapping_r, lang_sigs) = rayon::join(
                || -> AppResult<MappingBlob> {
                    bin_codec::decode(&row.file_mapping_blob).map_err(|err| {
                        AppError::internal(format!("decode sparse file mapping: {err}"))
                    })
                },
                || -> AppResult<(MappingBlob, SignaturesBlob)> {
                    let language_mapping: MappingBlob =
                        bin_codec::decode(&row.language_mapping_blob).map_err(|err| {
                            AppError::internal(format!("decode sparse language mapping: {err}"))
                        })?;
                    let signatures: SignaturesBlob = bin_codec::decode(&row.signatures_blob)
                        .map_err(|err| {
                            AppError::internal(format!("decode sparse signatures: {err}"))
                        })?;
                    Ok((language_mapping, signatures))
                },
            );
            let file_mapping = file_mapping_r?;
            let (language_mapping, signatures) = lang_sigs?;
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
    let bm25_blob = payload.bm25.encode_to_bytes();
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
    let mapped = mmap_dense_vectors(&dense_path, Some(row.dim))?;
    if mapped.cols() != row.dim {
        return Err(AppError::internal(format!(
            "dense row dim {} doesn't match vectors {}",
            row.dim,
            mapped.cols()
        )));
    }
    Ok(Some(DensePayload {
        // Persisted vectors are already unit-normalized; we point an
        // `ArrayView2` directly at the mapped bytes without any extra copy
        // or per-row sweep.
        vectors: DenseIndex::from_mapped(mapped),
        encoder_kind: row.encoder_kind,
        model_id: row.model_id,
        model_fingerprint: row.model_fingerprint,
        built_at_unix_secs: row.built_at_unix_secs,
        model_files_meta: row.model_files_meta,
    }))
}

/// Standalone, ParseCache-free dense load. Used by the search command to
/// open the mmap-backed dense matrix on a rayon worker alongside the sparse
/// and encoder loads ~ the SQLite metadata round-trip happens later on the
/// main thread, where we cross-check this matrix's shape against the
/// encoder's dim and the sparse payload's chunk count before adopting it.
/// Returns `None` for any benign miss (no cache dir, file absent, decode
/// error) so the slow path can fall through to `ensure_dense` / `build_dense`
/// as if no speculative load had happened.
///
/// Pages are pre-faulted on the same rayon worker that opened the mapping,
/// so the downstream matvec (which runs on the calling thread inside
/// `run_hybrid`) hits warm page-table entries rather than paying per-page
/// fault latency mid-dot-product.
pub fn load_dense_vectors_speculative(dense_path: &Path) -> Option<MappedDense> {
    if !dense_path.exists() {
        return None;
    }
    let mapped = mmap_dense_vectors(dense_path, None).ok()?;
    mapped.prefault_pages();
    Some(mapped)
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

/// mmap the dense vectors file and return a zero-copy view over the
/// f32 LE body. `expected_dim` lets us reject a stale file where SQLite
/// metadata and the blob disagreed (e.g. a partial write). The body is
/// not memcpy'd ~ subsequent matvec touches the same page-cache pages
/// directly, which used to pay the per-page fault cost twice (once for
/// the warm memcpy, again for the matvec). The mapping is wrapped in
/// `Arc` so the `DenseIndex` clone path stays cheap.
fn mmap_dense_vectors(path: &Path, expected_dim: Option<usize>) -> AppResult<MappedDense> {
    let file = fs::File::open(path).map_err(|err| {
        AppError::internal(format!("open dense vectors file {}: {err}", path.display()))
    })?;
    let mmap = unsafe { memmap2::Mmap::map(&file) }
        .map_err(|err| AppError::internal(format!("mmap dense vectors: {err}")))?;
    // Prefetch hint to the kernel: the subsequent matvec touches every
    // f32 in the matrix exactly once, sequentially. `MADV_WILLNEED` lets
    // the kernel issue read-ahead pages in the background while the rest
    // of `ensure_sparse` is still walking the repo; on warm-cache hits
    // this turns the matvec's per-page-fault cost (which was ~2 ms of
    // hot-path stall) into background work parallel with the sparse load.
    // Failures are silently ignored ~ `madvise` is best-effort.
    let _ = mmap.advise(memmap2::Advice::WillNeed);
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
    let mmap = Arc::new(mmap);
    MappedDense::new(mmap, DENSE_HEADER_BYTES, rows, cols).ok_or_else(|| {
        AppError::internal(format!(
            "dense vectors mapping rejected: rows={rows} cols={cols}"
        ))
    })
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
    let tmp = dense_tmp_path(path);
    fs::write(&tmp, bytes).map_err(|err| {
        AppError::internal(format!("write dense vectors tmp {}: {err}", tmp.display()))
    })?;
    fs::rename(&tmp, path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        AppError::internal(format!(
            "rename dense vectors {} -> {}: {err}",
            tmp.display(),
            path.display()
        ))
    })?;
    Ok(())
}

fn dense_tmp_path(path: &Path) -> PathBuf {
    let id = DENSE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("bin.tmp.{}.{}", std::process::id(), id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::{Arc, Barrier};

    fn temp_dir(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "mimi-dense-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn dense_tmp_paths_are_unique_for_same_target() {
        let target = std::env::temp_dir().join("mimi-cache/dense.v13.bin");
        let paths: HashSet<PathBuf> = (0..32).map(|_| dense_tmp_path(&target)).collect();
        assert_eq!(paths.len(), 32);
        assert!(paths.iter().all(|path| path.parent() == target.parent()));
    }

    #[test]
    fn concurrent_dense_writes_do_not_share_temp_file() {
        let dir = temp_dir("concurrent-write");
        let target = dir.join("dense.v13.bin");
        let vectors = Arc::new(Array2::from_shape_vec((2, 2), vec![1.0, 2.0, 3.0, 4.0]).unwrap());
        let barrier = Arc::new(Barrier::new(12));
        let mut handles = Vec::new();

        for _ in 0..12 {
            let target = target.clone();
            let vectors = Arc::clone(&vectors);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                write_dense_vectors_file(&target, &vectors)
            }));
        }

        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        let mapped = mmap_dense_vectors(&target, Some(2)).unwrap();
        assert_eq!(mapped.rows(), 2);
        assert_eq!(mapped.cols(), 2);
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(".tmp."))
            })
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");

        fs::remove_dir_all(dir).ok();
    }
}
