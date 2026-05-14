//! End-to-end search dispatch: walk → chunk → index → query.
//!
//! `ensure_sparse` is the cache hot-path. It walks the repo, takes a quick
//! file-signature snapshot, compares it to the persisted snapshot, and
//! rebuilds only when something changed. Rebuilds are parallel
//! (file-reading, chunking, and tokenization all fan out via rayon).
//!
//! Selectors at query time translate `--paths` (walk-scope), `--language`
//! (label filter), and `--exclude` into the chunk-id sets that BM25/Dense
//! use to skip unrelated chunks without rebuilding the index.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use globset::GlobSet;
use ndarray::{Array2, Axis};
use rayon::prelude::*;

use crate::cache::ParseCache;
use crate::error::{AppError, AppResult};
use crate::repo::RepoRoot;

use super::chunker::chunk_source;
use super::dense::DenseIndex;
use super::fuse::{search_bm25, QueryEncoder};
use super::model2vec::Encoder;
use super::persist::{
    load_dense, load_sparse, store_dense, store_sparse, DensePayload, SparsePayload,
};
use super::sparse::Bm25Index;
use super::types::{FileSignature, IndexedChunk, RankedHit, SearchMode, SearchStats};
use super::walker::{ensure_languages_resolved, walk_for_index, WalkOptions, WalkedFile};

/// Encoder batch size tuned for tokenizer throughput and stable memory usage.
const ENCODER_BATCH: usize = 1024;

/// Skip files larger than this when computing signatures / chunking.
/// Mirrors `walker::MAX_INDEX_FILE_BYTES`.
const PARALLEL_CHUNK_BATCH: usize = 32;

/// Result of `search_*` ~ ranked hits plus the stats the CLI needs to
/// render the response header (counts, elapsed, warnings).
#[derive(Debug, Clone)]
pub struct SearchOutput {
    pub hits: Vec<RankedHit>,
    pub stats: SearchStats,
    pub alpha: f32,
    pub indexed_files: usize,
    pub indexed_chunks: usize,
}

/// Compute the language selector: chunk ids that match any of `languages`.
/// `None` returns when the filter is empty (no filtering).
pub fn language_selector(payload: &SparsePayload, languages: &[String]) -> Option<Vec<usize>> {
    if languages.is_empty() {
        return None;
    }
    let mut ids: BTreeSet<usize> = BTreeSet::new();
    for lang in languages {
        if let Some(chunk_ids) = payload.language_mapping.get(lang.as_str()) {
            ids.extend(chunk_ids.iter().copied());
        }
    }
    Some(ids.into_iter().collect())
}

/// Compute a path-prefix selector: chunk ids whose file_path starts with
/// any of the requested prefixes. Used by positional `[PATHS]` scoping
/// without rebuilding the index.
pub fn path_selector(payload: &SparsePayload, prefixes: &[String]) -> Option<Vec<usize>> {
    if prefixes.is_empty() {
        return None;
    }
    let mut ids: BTreeSet<usize> = BTreeSet::new();
    for prefix in prefixes {
        let needle = prefix.trim_end_matches('/');
        for (path, chunk_ids) in &payload.file_mapping {
            if path == needle
                || path.starts_with(&format!("{needle}/"))
                || (needle.is_empty() && !path.is_empty())
            {
                ids.extend(chunk_ids.iter().copied());
            }
        }
    }
    Some(ids.into_iter().collect())
}

/// Compute an exclude selector: chunk ids whose file_path does not match any
/// exclude pattern. `None` returns when the filter is empty.
pub fn exclude_selector(
    payload: &SparsePayload,
    exclude_set: Option<&GlobSet>,
) -> Option<Vec<usize>> {
    let exclude_set = exclude_set?;
    let mut ids: BTreeSet<usize> = BTreeSet::new();
    for (path, chunk_ids) in &payload.file_mapping {
        if !exclude_set.is_match(path) {
            ids.extend(chunk_ids.iter().copied());
        }
    }
    Some(ids.into_iter().collect())
}

/// Intersect two selectors. Any-None returns the other; both-None → None.
pub fn intersect_selectors(a: Option<Vec<usize>>, b: Option<Vec<usize>>) -> Option<Vec<usize>> {
    match (a, b) {
        (None, None) => None,
        (Some(s), None) | (None, Some(s)) => Some(s),
        (Some(left), Some(right)) => {
            let lset: BTreeSet<usize> = left.into_iter().collect();
            let mut out: Vec<usize> = right.into_iter().filter(|id| lset.contains(id)).collect();
            out.sort_unstable();
            Some(out)
        }
    }
}

fn query_selector(
    payload: &SparsePayload,
    languages: &[String],
    paths: &[String],
    exclude_set: Option<&GlobSet>,
) -> Option<Vec<usize>> {
    let language_sel = language_selector(payload, languages);
    let path_sel = path_selector(payload, paths);
    let exclude_sel = exclude_selector(payload, exclude_set);
    intersect_selectors(intersect_selectors(language_sel, path_sel), exclude_sel)
}

/// Walk the full repo, build a signatures snapshot, compare against the
/// persisted index, rebuild on mismatch. Returns the in-memory sparse
/// payload (chunks + BM25 + mappings + signatures + timestamp) ready for
/// `search_*` to use.
pub fn ensure_sparse(repo: &RepoRoot, cache: &mut ParseCache) -> AppResult<SparsePayload> {
    // Walk + signature snapshot and the sibling-file decodes inside
    // `load_sparse` are independent ~ the persisted blob doesn't depend
    // on whether the on-disk file set has changed. Fan them out via
    // `rayon::join` so the longer side bounds the wall instead of their
    // sum. The mismatch case still rebuilds; on a hit (the overwhelmingly
    // common case) we skip the wait that the sequential ordering used to
    // impose.
    let (walked_result, persisted_result) = rayon::join(
        || walk_for_index(repo, &WalkOptions::default(), None),
        || load_sparse(cache),
    );
    let walked = walked_result?;
    let signatures = make_signatures(&walked);

    if let Some(persisted) = persisted_result? {
        if signatures_match(&persisted.signatures, &signatures) {
            return Ok(persisted);
        }
    }

    progress(format!(
        "index: rebuilding sparse index for {} files",
        walked.len()
    ));
    let payload = build_sparse(walked, signatures)?;
    store_sparse(cache, &payload)?;
    progress(format!(
        "index: sparse index ready ({} files, {} chunks)",
        payload.file_mapping.len(),
        payload.chunks.len()
    ));
    Ok(payload)
}

/// Force a rebuild regardless of the persisted state. Used by
/// `mimi index build`.
pub fn rebuild_sparse(repo: &RepoRoot, cache: &mut ParseCache) -> AppResult<SparsePayload> {
    progress("index: walking repository");
    let walked = walk_for_index(repo, &WalkOptions::default(), None)?;
    progress(format!("index: found {} candidate files", walked.len()));
    let signatures = make_signatures(&walked);
    let payload = build_sparse(walked, signatures)?;
    store_sparse(cache, &payload)?;
    progress(format!(
        "index: sparse index ready ({} files, {} chunks)",
        payload.file_mapping.len(),
        payload.chunks.len()
    ));
    Ok(payload)
}

fn make_signatures(walked: &[WalkedFile]) -> Vec<FileSignature> {
    let mut sigs: Vec<FileSignature> = walked
        .iter()
        .filter_map(|w| {
            let mtime_ns = w.metadata.modified().ok().and_then(|t| {
                t.duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_nanos() as i128)
            })?;
            Some(FileSignature {
                rel_path: w.resolved.relative_path.clone(),
                len: w.metadata.len(),
                mtime_ns,
            })
        })
        .collect();
    sigs.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    sigs
}

fn signatures_match(a: &[FileSignature], b: &[FileSignature]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(l, r)| l.rel_path == r.rel_path && l.len == r.len && l.mtime_ns == r.mtime_ns)
}

/// Build the sparse payload from a walked file list. Reads + chunks files
/// in parallel batches (rayon `par_chunks`); BM25 build happens once at
/// the end on the full chunk vector.
fn build_sparse(
    mut walked: Vec<WalkedFile>,
    signatures: Vec<FileSignature>,
) -> AppResult<SparsePayload> {
    // Resolve language labels now ~ the walk skips this step so warm
    // searches don't pay 2.5k extension lookups when the signatures match
    // and we never reach `build_sparse`.
    ensure_languages_resolved(&mut walked);
    ensure_language_parsers(&walked)?;

    progress(format!("index: chunking {} files", walked.len()));
    let chunked_results: Vec<AppResult<Vec<IndexedChunk>>> = walked
        .par_chunks(PARALLEL_CHUNK_BATCH)
        .flat_map(|batch| batch.iter().map(chunk_one).collect::<Vec<_>>())
        .collect();

    let mut chunked = Vec::with_capacity(chunked_results.len());
    for result in chunked_results {
        chunked.push(result?);
    }

    let mut chunks: Vec<IndexedChunk> = Vec::with_capacity(chunked.iter().map(Vec::len).sum());
    let mut file_mapping: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut language_mapping: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for chunk_set in chunked {
        for chunk in chunk_set {
            let chunk_id = chunks.len();
            file_mapping
                .entry(chunk.file_path.clone())
                .or_default()
                .push(chunk_id);
            let lang = chunk
                .language
                .clone()
                .unwrap_or_else(|| "plaintext".to_string());
            language_mapping.entry(lang).or_default().push(chunk_id);
            chunks.push(chunk);
        }
    }

    progress(format!("index: building BM25 over {} chunks", chunks.len()));
    let bm25 = Bm25Index::build_from_chunks(&chunks);
    let built_at_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    Ok(SparsePayload {
        bm25,
        chunks: super::chunk_store::ChunkStore::from_indexed(chunks),
        file_mapping,
        language_mapping,
        signatures,
        built_at_unix_secs,
    })
}

fn ensure_language_parsers(walked: &[WalkedFile]) -> AppResult<()> {
    let languages: BTreeSet<String> = walked
        .iter()
        .filter_map(|file| file.language.as_ref())
        .filter(|language| language.is_parseable())
        .map(|language| language.as_str().to_string())
        .collect();
    if languages.is_empty() {
        return Ok(());
    }

    let missing: Vec<String> = languages
        .iter()
        .filter(|language| !tree_sitter_language_pack::has_language(language.as_str()))
        .cloned()
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    progress(format!(
        "index: downloading tree-sitter parsers for {} (first run may take a while)",
        missing.join(", ")
    ));
    let refs: Vec<&str> = missing.iter().map(String::as_str).collect();
    let downloaded = tree_sitter_language_pack::download(&refs).map_err(|error| {
        AppError::parse(format!(
            "failed to download tree-sitter parsers for {}: {error}",
            missing.join(", ")
        ))
    })?;
    progress(format!(
        "index: tree-sitter parser cache ready ({downloaded} newly installed)"
    ));
    Ok(())
}

fn chunk_one(file: &WalkedFile) -> AppResult<Vec<IndexedChunk>> {
    let content = match std::fs::read_to_string(&file.resolved.full_path) {
        Ok(content) => content,
        Err(_) => return Ok(Vec::new()),
    };
    chunk_source(
        &content,
        &file.resolved.relative_path,
        file.language.as_ref(),
    )
}

/// BM25-only search dispatch. Resolves the selector intersection (language
/// + path prefixes), runs BM25, returns ranked hits + stats.
pub fn run_bm25(
    payload: &SparsePayload,
    query: &str,
    limit: usize,
    languages: &[String],
    paths: &[String],
    exclude_set: Option<&GlobSet>,
) -> SearchOutput {
    let selector = query_selector(payload, languages, paths, exclude_set);

    let started = Instant::now();
    let hits = search_bm25(
        query,
        &payload.bm25,
        &payload.chunks,
        limit,
        selector.as_deref(),
    );
    let elapsed_ms = started.elapsed().as_millis();

    SearchOutput {
        hits,
        stats: stats_from(payload, elapsed_ms),
        alpha: 0.0,
        indexed_files: payload.file_mapping.len(),
        indexed_chunks: payload.chunks.len(),
    }
}

/// Semantic-only search dispatch (Phase 3 surface; placed here so the
/// caller never has to know which fuse variant to call).
pub fn run_semantic<E: QueryEncoder + ?Sized>(
    payload: &SparsePayload,
    encoder: &E,
    dense: &super::dense::DenseIndex,
    query: &str,
    limit: usize,
    languages: &[String],
    paths: &[String],
    exclude_set: Option<&GlobSet>,
) -> AppResult<SearchOutput> {
    if dense.len() != payload.chunks.len() {
        return Err(AppError::internal(format!(
            "dense index length {} doesn't match chunk count {}",
            dense.len(),
            payload.chunks.len()
        )));
    }
    let selector = query_selector(payload, languages, paths, exclude_set);

    let started = Instant::now();
    let hits = super::fuse::search_semantic(
        query,
        encoder,
        dense,
        &payload.chunks,
        limit,
        selector.as_deref(),
    );
    let elapsed_ms = started.elapsed().as_millis();

    Ok(SearchOutput {
        hits,
        stats: stats_from(payload, elapsed_ms),
        alpha: 1.0,
        indexed_files: payload.file_mapping.len(),
        indexed_chunks: payload.chunks.len(),
    })
}

/// Hybrid search dispatch (Phase 3 surface).
pub fn run_hybrid<E: QueryEncoder + ?Sized>(
    payload: &SparsePayload,
    encoder: &E,
    dense: &super::dense::DenseIndex,
    query: &str,
    limit: usize,
    alpha: Option<f32>,
    languages: &[String],
    paths: &[String],
    exclude_set: Option<&GlobSet>,
) -> AppResult<SearchOutput> {
    if dense.len() != payload.chunks.len() {
        return Err(AppError::internal(format!(
            "dense index length {} doesn't match chunk count {}",
            dense.len(),
            payload.chunks.len()
        )));
    }
    let selector = query_selector(payload, languages, paths, exclude_set);

    let started = Instant::now();
    let (hits, alpha_used) = super::fuse::search_hybrid(
        query,
        encoder,
        dense,
        &payload.bm25,
        &payload.chunks,
        &payload.file_mapping,
        limit,
        alpha,
        selector.as_deref(),
    );
    let elapsed_ms = started.elapsed().as_millis();

    Ok(SearchOutput {
        hits,
        stats: stats_from(payload, elapsed_ms),
        alpha: alpha_used,
        indexed_files: payload.file_mapping.len(),
        indexed_chunks: payload.chunks.len(),
    })
}

fn stats_from(payload: &SparsePayload, elapsed_ms: u128) -> SearchStats {
    let _ = payload;
    SearchStats { elapsed_ms }
}

/// Build a dense index (or load the persisted one when fingerprint and
/// chunk count match). Caller supplies the encoder so `--hashing` and
/// `--model X` paths share the same orchestration.
pub fn ensure_dense(
    cache: &mut ParseCache,
    sparse: &SparsePayload,
    encoder: &dyn Encoder,
    encoder_kind: &str,
    model_id: &str,
    model_fingerprint: &str,
    model_files_meta: &str,
) -> AppResult<DensePayload> {
    ensure_dense_with_hint(
        cache,
        sparse,
        encoder,
        encoder_kind,
        model_id,
        model_fingerprint,
        model_files_meta,
        None,
    )
}

/// Same as `ensure_dense`, but lets the caller hand in a `MappedDense`
/// that was opened speculatively on another thread in parallel with the
/// sparse and encoder loads. We validate the hint against the cached
/// metadata: if encoder + fingerprint + chunk count + dim all line up, we
/// adopt the speculative mapping and skip the sequential `load_dense` IO
/// entirely. A failed validation (or no hint) falls through to the
/// classic path so the slow lane stays identical to before.
pub fn ensure_dense_with_hint(
    cache: &mut ParseCache,
    sparse: &SparsePayload,
    encoder: &dyn Encoder,
    encoder_kind: &str,
    model_id: &str,
    model_fingerprint: &str,
    model_files_meta: &str,
    hint: Option<super::dense::MappedDense>,
) -> AppResult<DensePayload> {
    if let Some(hint_mapped) = hint {
        // Speculative load only adopted when the cached metadata says the
        // same encoder + fingerprint + shape produced it. The metadata
        // lookup is a sub-millisecond SQLite read.
        if let Some(meta) = cache.lookup_search_dense_metadata() {
            let shape_ok =
                hint_mapped.rows() == sparse.chunks.len() && hint_mapped.cols() == encoder.dim();
            let meta_ok = meta.encoder_kind == encoder_kind
                && meta.model_fingerprint == model_fingerprint
                && meta.dim == encoder.dim();
            if shape_ok && meta_ok {
                return Ok(DensePayload {
                    vectors: super::dense::DenseIndex::from_mapped(hint_mapped),
                    encoder_kind: meta.encoder_kind,
                    model_id: meta.model_id,
                    model_fingerprint: meta.model_fingerprint,
                    built_at_unix_secs: meta.built_at_unix_secs,
                    model_files_meta: meta.model_files_meta,
                });
            }
        }
    }
    if let Some(persisted) = load_dense(cache)? {
        let fingerprint_ok = persisted.encoder_kind == encoder_kind
            && persisted.model_fingerprint == model_fingerprint
            && persisted.vectors.len() == sparse.chunks.len()
            && persisted.vectors.dim() == encoder.dim();
        if fingerprint_ok {
            return Ok(persisted);
        }
    }
    build_dense(
        cache,
        sparse,
        encoder,
        encoder_kind,
        model_id,
        model_fingerprint,
        model_files_meta,
    )
}

/// Force a dense rebuild. Used by `mimi index build --mode hybrid`.
pub fn rebuild_dense(
    cache: &mut ParseCache,
    sparse: &SparsePayload,
    encoder: &dyn Encoder,
    encoder_kind: &str,
    model_id: &str,
    model_fingerprint: &str,
    model_files_meta: &str,
) -> AppResult<DensePayload> {
    build_dense(
        cache,
        sparse,
        encoder,
        encoder_kind,
        model_id,
        model_fingerprint,
        model_files_meta,
    )
}

fn build_dense(
    cache: &mut ParseCache,
    sparse: &SparsePayload,
    encoder: &dyn Encoder,
    encoder_kind: &str,
    model_id: &str,
    model_fingerprint: &str,
    model_files_meta: &str,
) -> AppResult<DensePayload> {
    let dim = encoder.dim();
    if sparse.chunks.is_empty() {
        let vectors = Array2::<f32>::zeros((0, dim));
        let raw = vectors.clone();
        let payload = DensePayload {
            vectors: DenseIndex::new(vectors),
            encoder_kind: encoder_kind.to_string(),
            model_id: model_id.to_string(),
            model_fingerprint: model_fingerprint.to_string(),
            built_at_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            model_files_meta: model_files_meta.to_string(),
        };
        store_dense(cache, &payload, &raw)?;
        return Ok(payload);
    }

    progress(format!(
        "index: embedding {} chunks with {encoder_kind}",
        sparse.chunks.len()
    ));
    let total = sparse.chunks.len();
    let mut all_rows = Array2::<f32>::zeros((total, dim));
    let mut row_offset = 0usize;
    while row_offset < total {
        let batch_end = (row_offset + ENCODER_BATCH).min(total);
        let batch_len = batch_end - row_offset;
        if row_offset > 0 {
            progress(format!("index: embedded {row_offset}/{total} chunks"));
        }
        let texts: Vec<String> = (row_offset..batch_end)
            .map(|id| sparse.chunks.content(id).to_owned())
            .collect();
        let encoded = encoder.encode(&texts);
        if encoded.shape() != [batch_len, dim] {
            return Err(AppError::internal(format!(
                "encoder returned {:?} for batch of {} (expected dim {})",
                encoded.shape(),
                batch_len,
                dim
            )));
        }
        let mut slice = all_rows.slice_mut(ndarray::s![row_offset..batch_end, ..]);
        slice.assign(&encoded);
        row_offset = batch_end;
    }
    progress(format!(
        "index: embedded {}/{} chunks",
        row_offset,
        sparse.chunks.len()
    ));
    // L2-normalize each row in place. `DenseIndex::from_normalized` then
    // skips a redundant per-row sweep ~ same vectors get persisted, and
    // the load path doesn't re-normalize either.
    for mut row in all_rows.axis_iter_mut(Axis(0)) {
        let norm = row.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 1e-8 {
            row.mapv_inplace(|v| v / norm);
        }
    }
    let raw = all_rows.clone();
    let payload = DensePayload {
        vectors: DenseIndex::from_normalized(all_rows),
        encoder_kind: encoder_kind.to_string(),
        model_id: model_id.to_string(),
        model_fingerprint: model_fingerprint.to_string(),
        built_at_unix_secs: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        model_files_meta: model_files_meta.to_string(),
    };
    store_dense(cache, &payload, &raw)?;
    progress("index: dense index ready");
    Ok(payload)
}

/// Used by `find-related`: locate the chunk that contains a given
/// `path:line` and return its id.
pub fn find_chunk_at_line(payload: &SparsePayload, path: &str, line: usize) -> Option<usize> {
    let chunk_ids = payload.file_mapping.get(path)?;
    chunk_ids
        .iter()
        .copied()
        .find(|&id| line >= payload.chunks.start_line(id) && line <= payload.chunks.end_line(id))
        .or_else(|| chunk_ids.iter().copied().next())
}

/// Mark `_` to discard search return values that callers don't need.
#[doc(hidden)]
pub fn _suppress_unused() {
    let _ = SearchMode::Hybrid;
}

fn progress(message: impl AsRef<str>) {
    eprintln!("mimi: {}", message.as_ref());
}
