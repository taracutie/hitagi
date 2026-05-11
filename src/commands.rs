use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write;
use std::fs::Metadata;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use rayon::prelude::*;

use crate::{
    cache::ParseCache,
    error::{AppError, AppResult},
    framework, git,
    lang::{count_lines, Language, LineStats},
    models::{
        CacheClearResponse, CacheLangCount, CachePathResponse, CacheStatusResponse,
        DiffFileResponse, DiffFileSummary, DiffHunk, DiffMultiFileResponse, DiffOverviewResponse,
        DiffPathsResponse, DiffSummaryFile, DiffSummaryGroup, DiffSummaryResponse, FilesGroup,
        FilesResponse, FindGroup, FindMatch, FindMatches, FindRelatedResponse, FindResponse,
        IndexBuildResponse, IndexCleanResponse, IndexStatusResponse, LangSummary, LangsResponse,
        LocFileResult, LocFilesResponse, LocSymbolResult, LocSymbolsResponse, NextInfoResponse,
        NextLayoutsResponse, NextRoutesResponse, NextServerActionsResponse, OutlineResponse,
        OutputSymbol, OutputSymbolDetail, ReadFileResponse, ReadSummaryResponse, SearchHit,
        SearchResponse, SymbolDetail, SymbolInfo, SymbolResponse,
    },
    parser::{parse_source, ParsedFile},
    queries::{
        resolve_symbol, snippet_for_symbol_signature, symbol_detail, symbols_for_line_range,
    },
    repo::{self, RepoRoot, ResolvedPath},
    search,
};

pub const MAX_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// "Drop" `value` by leaking it. Used at the tail of one-shot CLI
/// commands that own multi-MB caches/indexes (`ChunkStore` content mmap,
/// dense `Array2<f32>`, BM25 postings, embedding matrix,
/// `tokenizers::Tokenizer`, ...) ~ sequential `drop` otherwise sits
/// between the search result and the text output the caller is
/// waiting on, adding 20-30 ms of latency per command for free() work
/// that the OS will redo on `exit(2)` anyway.
///
/// This is only safe because the binary is a short-lived CLI: `main` is
/// about to return and the OS will reclaim all memory and unmap all
/// mappings. Library consumers that need clean teardown should call into
/// `commands::*` and `Drop` the response themselves; only the CLI
/// wrapper exercises this path.
fn defer_drop<T: 'static>(value: T) {
    std::mem::forget(value);
}

/// When `outline` is called without --depth, --kind, or --bytes and the file
/// has more symbols than this, auto-collapse to --depth 1 and emit a note. A
/// 2,500-symbol Prisma schema produces a ~240 KB response without this; the
/// caller almost always wants orientation first, then a targeted drill.
const OUTLINE_AUTO_SUMMARY_THRESHOLD: usize = 500;

#[derive(Debug, Default, Clone)]
pub struct OutlineOptions {
    pub bytes: bool,
    pub kinds: Vec<String>,
    /// Max nesting depth: 1 = top-level only, 2 = top + 1 nested, etc. None = no limit.
    pub depth: Option<usize>,
}

#[derive(Debug, Default, Clone)]
pub struct SymbolOptions {
    pub bytes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchModeArg {
    Hybrid,
    Bm25,
    Semantic,
}

impl SearchModeArg {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hybrid => "hybrid",
            Self::Bm25 => "bm25",
            Self::Semantic => "semantic",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub paths: Vec<String>,
    pub excludes: Vec<String>,
    pub limit: usize,
    pub mode: SearchModeArg,
    pub languages: Vec<String>,
    pub alpha: Option<f32>,
    pub snippet: bool,
    pub hashing: bool,
    pub no_download: bool,
    pub offline: bool,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FindRelatedOptions {
    pub limit: usize,
    pub hashing: bool,
    pub no_download: bool,
    pub offline: bool,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct IndexBuildOptions {
    pub mode: SearchModeArg,
    pub hashing: bool,
    pub no_download: bool,
    pub offline: bool,
    pub model: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct ReadOptions {
    pub lines: Option<(usize, usize)>,
    pub summary: bool,
}

#[derive(Debug, Clone)]
pub struct FindOptions {
    pub paths: Vec<String>,
    pub excludes: Vec<String>,
    pub kinds: Vec<String>,
    pub limit: usize,
    pub bytes: bool,
    pub snippet: bool,
    pub terse: bool,
    /// Cap matches per file at N. 0 = no cap. Suppressed matches are recorded
    /// in `FindResponse::more_in_file` (or per-group when grouped).
    pub per_file: usize,
}

#[derive(Debug, Clone)]
pub struct FilesOptions {
    pub globs: Vec<String>,
    pub excludes: Vec<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocSort {
    CodeDesc,
    CodeAsc,
    Path,
}

impl LocSort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CodeDesc => "code-desc",
            Self::CodeAsc => "code-asc",
            Self::Path => "path",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LocSymbolsOptions {
    pub paths: Vec<String>,
    pub excludes: Vec<String>,
    pub kinds: Vec<String>,
    pub languages: Vec<String>,
    pub min_lines: Option<usize>,
    pub max_lines: Option<usize>,
    pub limit: usize,
    pub sort: LocSort,
    pub bytes: bool,
    pub snippet: bool,
}

#[derive(Debug, Clone)]
pub struct LocFilesOptions {
    pub globs: Vec<String>,
    pub excludes: Vec<String>,
    pub languages: Vec<String>,
    pub min_lines: Option<usize>,
    pub max_lines: Option<usize>,
    pub limit: usize,
    pub sort: LocSort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffScope {
    All,
    Staged,
    Unstaged,
    Untracked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffBodyMode {
    Full,
    ChangedLines,
    AddedOnly,
    None,
}

impl Default for DiffBodyMode {
    fn default() -> Self {
        Self::Full
    }
}

#[derive(Debug, Clone)]
pub struct DiffOptions {
    pub scope: DiffScope,
    /// Validated by the CLI layer before this struct is built.
    pub against: String,
    pub excludes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DiffFileOptions {
    pub symbol: Option<String>,
    pub raw: bool,
    pub body: DiffBodyMode,
    pub snippet: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DiffSummaryOptions {
    pub symbols: bool,
    pub commit: bool,
    pub group_by_state: bool,
}

struct LoadedSource {
    language: Option<Language>,
    content: String,
}

struct ParseCacheUpdate {
    relative_path: String,
    metadata: Metadata,
    language: Language,
    line_stats: Option<LineStats>,
    symbols: Vec<SymbolInfo>,
}

struct PreparedFindFile {
    resolved: ResolvedPath,
    metadata: Metadata,
    language: Language,
    cached_symbols: Option<Vec<SymbolInfo>>,
    needs_content: bool,
}

enum FindWorkItem {
    Skip {
        relative_path: String,
    },
    Error {
        relative_path: String,
        error: AppError,
    },
    Load(PreparedFindFile),
}

struct FindFileResult {
    relative_path: String,
    language: Language,
    symbols: Vec<SymbolInfo>,
    content: Option<String>,
    cache_update: Option<ParseCacheUpdate>,
}

enum FindWorkResult {
    Skipped {
        relative_path: String,
    },
    Error {
        relative_path: String,
        error: AppError,
    },
    Ready(FindFileResult),
}

pub fn outline(repo: &RepoRoot, path: &str, opts: OutlineOptions) -> AppResult<OutlineResponse> {
    let resolved = repo.resolve_file(path)?;
    let mut cache = ParseCache::open(repo.root());

    let stat = stat_file(&resolved)?;
    let language = stat.language.filter(|l| l.is_parseable()).ok_or_else(|| {
        AppError::unsupported(format!("no parser for {}", resolved.relative_path))
    })?;

    let symbols = cached_or_parsed(&mut cache, &resolved, &stat.metadata, &language, None)?;
    cache.save(false);

    let total_symbols = symbols.len();
    let mut kind_counts: BTreeMap<String, usize> = BTreeMap::new();
    for s in &symbols {
        *kind_counts.entry(s.kind.clone()).or_insert(0) += 1;
    }
    let all_kinds: BTreeSet<String> = kind_counts.keys().cloned().collect();

    // Soft cap: if the caller didn't constrain the response and the file is
    // huge, collapse to top-level shapes. The caller can always re-issue with
    // explicit --depth N or --kind to drill in.
    let caller_constrained = opts.depth.is_some() || !opts.kinds.is_empty() || opts.bytes;
    let auto_summarized = !caller_constrained && total_symbols > OUTLINE_AUTO_SUMMARY_THRESHOLD;
    let effective_depth = if auto_summarized { Some(1) } else { opts.depth };

    let symbols: Vec<OutputSymbol> = symbols
        .into_iter()
        .filter(|s| within_depth(&s.qualname, effective_depth))
        .filter(|s| matches_kinds(&opts.kinds, &s.kind))
        .map(|s| to_output_symbol(s, opts.bytes))
        .collect();

    let available_kinds = if !opts.kinds.is_empty() && symbols.is_empty() {
        Some(all_kinds.into_iter().collect())
    } else {
        None
    };

    let note = if auto_summarized {
        Some(format!(
            "file has {total_symbols} symbols; auto-applied --depth 1 to keep response compact. \
             Re-run with --depth N or --kind K1,K2 to drill in."
        ))
    } else {
        None
    };

    Ok(OutlineResponse {
        language: language.as_str().to_string(),
        total_symbols,
        kind_counts,
        symbols,
        available_kinds,
        auto_summarized,
        note,
    })
}

pub fn symbol(
    repo: &RepoRoot,
    path: &str,
    qualname: &str,
    opts: SymbolOptions,
) -> AppResult<SymbolResponse> {
    let qualname = qualname.trim();
    if qualname.is_empty() {
        return Err(AppError::bad_request("qualname must not be empty"));
    }

    let resolved = repo.resolve_file(path)?;
    let mut cache = ParseCache::open(repo.root());

    let stat = stat_file(&resolved)?;
    let language = stat.language.filter(|l| l.is_parseable()).ok_or_else(|| {
        AppError::unsupported(format!("no parser for {}", resolved.relative_path))
    })?;
    let content = read_after_stat(&resolved)?;
    let symbols = cached_or_parsed(
        &mut cache,
        &resolved,
        &stat.metadata,
        &language,
        Some(&content),
    )?;
    cache.save(false);

    let parsed = ParsedFile { symbols };
    let detail = symbol_detail(&parsed, &content, qualname, MAX_RESPONSE_BYTES)?;

    Ok(SymbolResponse {
        language: language.as_str().to_string(),
        symbol: to_output_symbol_detail(detail, opts.bytes),
    })
}

pub fn search(repo: &RepoRoot, query: &str, opts: SearchOptions) -> AppResult<SearchResponse> {
    let query = query.trim();
    if query.is_empty() {
        return Err(AppError::bad_request("query must not be empty"));
    }
    if opts.limit == 0 {
        return Err(AppError::bad_request("--limit must be at least 1"));
    }

    let mut warnings: Vec<String> = Vec::new();
    let exclude_set = build_exclude_set(&opts.excludes)?;
    let mode_label = opts.mode.as_str().to_string();

    let output = match opts.mode {
        SearchModeArg::Bm25 => {
            let mut cache = ParseCache::open(repo.root());
            let sparse = search::engine::ensure_sparse(repo, &mut cache)?;
            if sparse.chunks.is_empty() {
                warnings.push("repo has no indexable files".to_string());
            }
            let r = search::engine::run_bm25(
                &sparse,
                query,
                opts.limit,
                &opts.languages,
                &opts.paths,
                exclude_set.as_ref(),
            );
            // Punt the heavy drops (BM25 index + mmap-backed ChunkStore +
            // SQLite connection) to a detached worker so the CLI can
            // render the response in parallel. The process exits shortly
            // after; the OS reclaims everything either way.
            defer_drop((sparse, cache));
            r
        }
        SearchModeArg::Hybrid | SearchModeArg::Semantic => {
            // Three independent IO chunks dominate the warm-search wall:
            //   sparse-index load    (walk + SQLite + BM25/chunk decode)
            //   encoder load         (fast WordPiece + safetensors mmap)
            //   dense-matrix mapping (zero-copy mmap-view, prefaulted)
            // None depend on the others' results, so we fan them out via
            // nested `rayon::join`s. The dense matrix is just bytes ~ we
            // open the mmap speculatively on a third worker and adopt it
            // in `ensure_dense_with_hint` once the cached fingerprint check
            // confirms the bytes match this encoder. A mismatch (rare:
            // model swap / corruption) drops the hint and falls back to
            // `load_dense → build_dense`, leaving the slow lane unchanged.
            let prepared = prepare_encoder_load(
                opts.hashing,
                opts.no_download,
                opts.offline,
                opts.model.as_deref(),
            )?;
            let repo_ref = repo;
            let dense_path =
                ParseCache::cache_dir_for(repo_ref.root()).map(|d| d.join("dense.v13.bin"));
            let (sparse_result, (encoder_result, dense_hint)) = rayon::join(
                || -> AppResult<(ParseCache, search::persist::SparsePayload)> {
                    let mut cache_inner = ParseCache::open(repo_ref.root());
                    let sparse = search::engine::ensure_sparse(repo_ref, &mut cache_inner)?;
                    Ok((cache_inner, sparse))
                },
                || {
                    rayon::join(
                        || load_encoder_files(&prepared),
                        || -> Option<search::dense::MappedDense> {
                            dense_path
                                .as_ref()
                                .and_then(|p| search::persist::load_dense_vectors_speculative(p))
                        },
                    )
                },
            );
            let (mut cache, sparse) = sparse_result?;
            if sparse.chunks.is_empty() {
                warnings.push("repo has no indexable files".to_string());
            }
            let mut encoder_load = encoder_result?;
            if let Some(w) = encoder_load.warning.take() {
                warnings.push(w);
            }
            let (fingerprint, files_meta) =
                reconcile_dense_fingerprint(&mut cache, &prepared, &encoder_load);
            let dense = search::engine::ensure_dense_with_hint(
                &mut cache,
                &sparse,
                encoder_load.encoder.as_ref(),
                &encoder_load.kind,
                &encoder_load.model_id,
                &fingerprint,
                &files_meta,
                dense_hint,
            )?;
            let r = match opts.mode {
                SearchModeArg::Semantic => search::engine::run_semantic(
                    &sparse,
                    encoder_load.encoder.as_ref(),
                    &dense.vectors,
                    query,
                    opts.limit,
                    &opts.languages,
                    &opts.paths,
                    exclude_set.as_ref(),
                )?,
                SearchModeArg::Hybrid => search::engine::run_hybrid(
                    &sparse,
                    encoder_load.encoder.as_ref(),
                    &dense.vectors,
                    query,
                    opts.limit,
                    opts.alpha,
                    &opts.languages,
                    &opts.paths,
                    exclude_set.as_ref(),
                )?,
                SearchModeArg::Bm25 => unreachable!("bm25 handled above"),
            };
            // Heavy locals (sparse BM25 + chunks mmap, dense 60 MB matrix,
            // encoder embedding table + tokenizer, ParseCache SQLite handle)
            // would otherwise add ~15-25 ms of sequential Drop work right
            // before this function returns the SearchResponse to the CLI.
            // Punt them to a detached worker so the caller can render the
            // response in parallel.
            defer_drop((sparse, dense, encoder_load, cache, prepared));
            r
        }
    };

    let mut results: Vec<SearchHit> = output
        .hits
        .into_iter()
        .map(|hit| build_hit(hit, opts.snippet))
        .collect();

    if !opts.paths.is_empty() {
        results.retain(|hit| hit_under_any_prefix(&hit.path, &opts.paths));
    }

    Ok(SearchResponse {
        query: query.to_string(),
        mode: mode_label,
        alpha: output.alpha,
        limit: opts.limit,
        languages: opts.languages,
        paths: opts.paths,
        elapsed_ms: output.stats.elapsed_ms,
        indexed_files: output.indexed_files,
        indexed_chunks: output.indexed_chunks,
        warnings,
        results,
    })
}

pub fn find_related(
    repo: &RepoRoot,
    path: &str,
    line: usize,
    opts: FindRelatedOptions,
) -> AppResult<FindRelatedResponse> {
    if line == 0 {
        return Err(AppError::bad_request("line must be 1-indexed"));
    }
    if opts.limit == 0 {
        return Err(AppError::bad_request("--limit must be at least 1"));
    }

    // Resolve the path against the repo's file list so suffix-matching works
    // (`--repo /repo find-related src/foo.rs` works even when invoked from
    // a subdir).
    let resolved = repo.resolve_file(path)?;

    let mut warnings: Vec<String> = Vec::new();
    // Three-way fan-out mirrors `search()`: the sparse-index load, encoder
    // load, and the raw dense-matrix mmap+memcpy are independent IO chunks.
    // Speculatively load the dense file alongside the others; the cached
    // metadata check decides whether to adopt it after we already have a
    // ParseCache + encoder in hand. The slow path (mismatch) drops the hint
    // and falls back to the classic `load_dense → build_dense` lane.
    let prepared = prepare_encoder_load(
        opts.hashing,
        opts.no_download,
        opts.offline,
        opts.model.as_deref(),
    )?;
    let repo_ref = repo;
    let dense_path = ParseCache::cache_dir_for(repo_ref.root()).map(|d| d.join("dense.v13.bin"));
    let (sparse_result, (encoder_result, dense_hint)) = rayon::join(
        || -> AppResult<(ParseCache, search::persist::SparsePayload)> {
            let mut cache_inner = ParseCache::open(repo_ref.root());
            let sparse = search::engine::ensure_sparse(repo_ref, &mut cache_inner)?;
            Ok((cache_inner, sparse))
        },
        || {
            rayon::join(
                || load_encoder_files(&prepared),
                || -> Option<search::dense::MappedDense> {
                    let path = dense_path.as_ref()?;
                    search::persist::load_dense_vectors_speculative(path)
                },
            )
        },
    );
    let (mut cache, sparse) = sparse_result?;
    let mut encoder_load = encoder_result?;
    if let Some(w) = encoder_load.warning.take() {
        warnings.push(w);
    }

    let chunk_id = search::engine::find_chunk_at_line(&sparse, &resolved.relative_path, line)
        .ok_or_else(|| {
            AppError::not_found(format!(
                "no indexed chunk for {}:{line}; index may need a rebuild",
                resolved.relative_path
            ))
        })?;
    let source_chunk = sparse.chunks.to_indexed(chunk_id);

    let (fingerprint, files_meta) =
        reconcile_dense_fingerprint(&mut cache, &prepared, &encoder_load);
    let dense = search::engine::ensure_dense_with_hint(
        &mut cache,
        &sparse,
        encoder_load.encoder.as_ref(),
        &encoder_load.kind,
        &encoder_load.model_id,
        &fingerprint,
        &files_meta,
        dense_hint,
    )?;

    let started = std::time::Instant::now();
    // Encode the source chunk's content as the query. Selector excludes the
    // source chunk itself so we never return it.
    let mut allowed: Vec<usize> = (0..sparse.chunks.len()).collect();
    allowed.retain(|&id| id != chunk_id);
    let mut hits = search::fuse::search_semantic(
        &source_chunk.content,
        encoder_load.encoder.as_ref(),
        &dense.vectors,
        &sparse.chunks,
        opts.limit,
        Some(&allowed),
    );
    let elapsed_ms = started.elapsed().as_millis();

    // Drop any remaining matches for the source path itself unless it has
    // multiple chunks (then keep neighboring chunks).
    hits.retain(|h| {
        h.chunk.file_path != source_chunk.file_path || h.chunk.start_line != source_chunk.start_line
    });

    let results: Vec<SearchHit> = hits.into_iter().map(|h| build_hit(h, false)).collect();
    let source_hit = SearchHit {
        path: source_chunk.file_path,
        lines: [source_chunk.start_line, source_chunk.end_line],
        language: source_chunk.language,
        score: 1.0,
        source: "source".to_string(),
        snippet: None,
    };

    let indexed_files = sparse.file_mapping.len();
    let indexed_chunks = sparse.chunks.len();
    // Skip sequential drop of the multi-MB sparse / dense / encoder
    // payloads; same rationale as the `search` command. The CLI exits
    // shortly and the OS reclaims everything.
    defer_drop((sparse, dense, encoder_load, cache, prepared));

    Ok(FindRelatedResponse {
        path: resolved.relative_path,
        line,
        limit: opts.limit,
        elapsed_ms,
        indexed_files,
        indexed_chunks,
        source_chunk: source_hit,
        warnings,
        results,
    })
}

/// CLI-derived encoder configuration, computed once up-front so the
/// thread-bound `load_encoder_files` can do all the IO without touching
/// either the policy logic or the cache.
struct EncoderRequest {
    hashing: bool,
    offline: bool,
    no_download: bool,
    /// `None` when `--hashing` is passed (we skip the model entirely).
    options: Option<search::model2vec::ModelOptions>,
}

/// File-IO-only result of loading the encoder. The fingerprint is *not*
/// resolved here ~ that step needs `ParseCache` and is done by
/// `reconcile_dense_fingerprint` after the parallel join.
struct EncoderLoad {
    encoder: Box<dyn search::model2vec::Encoder>,
    kind: String,
    model_id: String,
    warning: Option<String>,
    /// Cheap `(mtime, len)` tuple of the model files. Set to `None` when the
    /// encoder is the hashing fallback (no files to stat).
    current_files_meta: Option<String>,
    /// Empty unless `current_files_meta` was unset and we still need a
    /// fingerprint; carries the model options used to load.
    fingerprint_options: Option<search::model2vec::ModelOptions>,
}

fn prepare_encoder_load(
    hashing: bool,
    no_download: bool,
    offline: bool,
    model: Option<&str>,
) -> AppResult<EncoderRequest> {
    let options = if hashing {
        None
    } else {
        let policy = if offline {
            search::model2vec::ModelLoadPolicy::Offline
        } else if no_download {
            search::model2vec::ModelLoadPolicy::NoDownload
        } else {
            search::model2vec::ModelLoadPolicy::AllowDownload
        };
        Some(search::model2vec::ModelOptions::new(model, policy))
    };
    Ok(EncoderRequest {
        hashing,
        offline,
        no_download,
        options,
    })
}

/// Cache-free portion of encoder loading: open the model files, build the
/// encoder, and stat the files for `model_files_meta`. Runs on a worker
/// thread alongside `ensure_sparse`.
fn load_encoder_files(req: &EncoderRequest) -> AppResult<EncoderLoad> {
    if req.hashing {
        let encoder = search::model2vec::HashingEncoder::new(search::model2vec::DEFAULT_DIM);
        return Ok(EncoderLoad {
            encoder: Box::new(encoder),
            kind: "hashing".to_string(),
            model_id: format!("hashing-{}", search::model2vec::DEFAULT_DIM),
            warning: None,
            current_files_meta: Some(String::new()),
            fingerprint_options: None,
        });
    }
    let options = req
        .options
        .clone()
        .expect("non-hashing request always carries ModelOptions");
    search::model_cache::ensure_hf_home();
    let model_status = search::model2vec::model_status(Some(&options.model));
    if matches!(
        options.policy,
        search::model2vec::ModelLoadPolicy::AllowDownload
    ) && !model_status.available()
    {
        eprintln!(
            "hitagi: index: downloading embedding model {} (first run may take a while)",
            options.model
        );
    } else {
        eprintln!("hitagi: index: loading embedding model {}", options.model);
    }
    match search::model2vec::load_model(&options) {
        Ok(encoder) => {
            let current = search::model2vec::model_files_meta(&options).ok();
            Ok(EncoderLoad {
                encoder,
                kind: "model2vec".to_string(),
                model_id: options.model.clone(),
                warning: None,
                current_files_meta: current,
                fingerprint_options: Some(options),
            })
        }
        Err(err) if req.offline || req.no_download => {
            let encoder = search::model2vec::HashingEncoder::new(search::model2vec::DEFAULT_DIM);
            Ok(EncoderLoad {
                encoder: Box::new(encoder),
                kind: "hashing".to_string(),
                model_id: format!("hashing-{}", search::model2vec::DEFAULT_DIM),
                warning: Some(format!(
                    "model unavailable ({err}); falling back to hashing encoder"
                )),
                current_files_meta: Some(String::new()),
                fingerprint_options: None,
            })
        }
        Err(err) => Err(err),
    }
}

/// Resolve the dense fingerprint + files_meta tuple. Uses the cached row's
/// fast-path when the on-disk files haven't changed; otherwise rehashes the
/// model files. Always returns a usable `(fingerprint, files_meta)` pair
/// (hashing encoder, missing model, etc. fall back to deterministic strings).
fn reconcile_dense_fingerprint(
    cache: &mut ParseCache,
    req: &EncoderRequest,
    load: &EncoderLoad,
) -> (String, String) {
    if req.hashing {
        let id = format!("hashing-{}", search::model2vec::DEFAULT_DIM);
        return (id, String::new());
    }
    let Some(options) = load.fingerprint_options.as_ref() else {
        // Hashing-fallback path: same shape as the explicit --hashing branch.
        return (load.model_id.clone(), String::new());
    };
    let cached = cache.lookup_search_dense_metadata();
    let current = load.current_files_meta.clone();
    match (&cached, &current) {
        (Some(row), Some(meta))
            if row.encoder_kind == "model2vec"
                && row.model_id == options.model
                && !row.model_files_meta.is_empty()
                && row.model_files_meta == *meta =>
        {
            (row.model_fingerprint.clone(), meta.clone())
        }
        _ => {
            let fp = search::model2vec::model_fingerprint(options)
                .unwrap_or_else(|_| options.model.clone());
            (fp, current.unwrap_or_default())
        }
    }
}

/// Resolve the encoder requested by the CLI flags, applying the
/// `--offline` / `--no-download` / `--hashing` policy. Returns the encoder
/// + tagging metadata needed to fingerprint the persisted dense row.
///
/// Falls back to the hashing encoder when `--offline` / `--no-download` is
/// set and the Model2Vec model isn't locally cached, emitting a warning so
/// the user knows quality is degraded.
fn load_encoder_with_policy(
    cache: &mut ParseCache,
    hashing: bool,
    no_download: bool,
    offline: bool,
    model: Option<&str>,
) -> AppResult<(
    Box<dyn search::model2vec::Encoder>,
    String,
    String,
    String,
    String,
    Option<String>,
)> {
    if hashing {
        let encoder = search::model2vec::HashingEncoder::new(search::model2vec::DEFAULT_DIM);
        return Ok((
            Box::new(encoder),
            "hashing".to_string(),
            format!("hashing-{}", search::model2vec::DEFAULT_DIM),
            format!("hashing-{}", search::model2vec::DEFAULT_DIM),
            String::new(),
            None,
        ));
    }

    let policy = if offline {
        search::model2vec::ModelLoadPolicy::Offline
    } else if no_download {
        search::model2vec::ModelLoadPolicy::NoDownload
    } else {
        search::model2vec::ModelLoadPolicy::AllowDownload
    };
    let options = search::model2vec::ModelOptions::new(model, policy);
    search::model_cache::ensure_hf_home();
    let model_status = search::model2vec::model_status(model);
    if matches!(policy, search::model2vec::ModelLoadPolicy::AllowDownload)
        && !model_status.available()
    {
        eprintln!(
            "hitagi: index: downloading embedding model {} (first run may take a while)",
            options.model
        );
    } else {
        eprintln!("hitagi: index: loading embedding model {}", options.model);
    }

    match search::model2vec::load_model(&options) {
        Ok(encoder) => {
            // Fast path: if the cached dense row is for the same model and
            // its file-metadata signature still matches what's on disk,
            // reuse the cached fingerprint instead of re-hashing 30+ MB
            // of model weights.
            let cached = cache.lookup_search_dense_metadata();
            let current_meta = search::model2vec::model_files_meta(&options).ok();
            let (fingerprint, files_meta) = match (&cached, &current_meta) {
                (Some(row), Some(meta))
                    if row.encoder_kind == "model2vec"
                        && row.model_id == options.model
                        && !row.model_files_meta.is_empty()
                        && row.model_files_meta == *meta =>
                {
                    (row.model_fingerprint.clone(), meta.clone())
                }
                _ => {
                    let fp = search::model2vec::model_fingerprint(&options)
                        .unwrap_or_else(|_| options.model.clone());
                    (fp, current_meta.unwrap_or_default())
                }
            };
            Ok((
                encoder,
                "model2vec".to_string(),
                options.model.clone(),
                fingerprint,
                files_meta,
                None,
            ))
        }
        Err(err) if offline || no_download => {
            // Fall back to the hashing encoder so the agent can still get
            // some semantic-ish ranking instead of a hard error.
            let encoder = search::model2vec::HashingEncoder::new(search::model2vec::DEFAULT_DIM);
            Ok((
                Box::new(encoder),
                "hashing".to_string(),
                format!("hashing-{}", search::model2vec::DEFAULT_DIM),
                format!("hashing-{}", search::model2vec::DEFAULT_DIM),
                String::new(),
                Some(format!(
                    "model unavailable ({err}); falling back to hashing encoder"
                )),
            ))
        }
        Err(err) => Err(err),
    }
}

pub fn index_status(repo: &RepoRoot) -> IndexStatusResponse {
    let inspection = ParseCache::inspect_search_index(repo.root());
    let parse_inspection = ParseCache::inspect(repo.root());
    let cache_file = parse_inspection
        .cache_file
        .as_ref()
        .map(|p| p.display().to_string());

    let indexed_files = inspection.sparse_indexed_files.unwrap_or(0);
    let indexed_chunks = inspection.sparse_indexed_chunks.unwrap_or(0);
    let languages = inspection.sparse_language_counts.clone();

    IndexStatusResponse {
        cache_file,
        sparse_present: inspection.sparse_present,
        dense_present: inspection.dense_present,
        indexed_files,
        indexed_chunks,
        languages,
        model_id: inspection.dense_model_id,
        encoder_kind: inspection.dense_encoder_kind,
        model_fingerprint: inspection.dense_model_fingerprint,
        dim: inspection.dense_dim,
        sparse_built_at_unix_secs: inspection.sparse_built_at_unix_secs,
        dense_built_at_unix_secs: inspection.dense_built_at_unix_secs,
        sparse_size_bytes: inspection
            .sparse_present
            .then_some(inspection.sparse_chunks_bytes),
        dense_size_bytes: inspection
            .dense_present
            .then_some(inspection.dense_vectors_bytes),
    }
}

pub fn index_build(repo: &RepoRoot, opts: IndexBuildOptions) -> AppResult<IndexBuildResponse> {
    let mut cache = ParseCache::open(repo.root());
    let started = std::time::Instant::now();
    let sparse = search::engine::rebuild_sparse(repo, &mut cache)?;
    let mut warnings: Vec<String> = Vec::new();

    if matches!(opts.mode, SearchModeArg::Hybrid | SearchModeArg::Semantic) {
        let (encoder, encoder_kind, model_id, fingerprint, files_meta, encoder_warning) =
            load_encoder_with_policy(
                &mut cache,
                opts.hashing,
                opts.no_download,
                opts.offline,
                opts.model.as_deref(),
            )?;
        if let Some(w) = encoder_warning {
            warnings.push(w);
        }
        let _ = search::engine::rebuild_dense(
            &mut cache,
            &sparse,
            encoder.as_ref(),
            &encoder_kind,
            &model_id,
            &fingerprint,
            &files_meta,
        )?;
    }

    let elapsed_ms = started.elapsed().as_millis();
    let languages: BTreeMap<String, usize> = sparse
        .language_mapping
        .iter()
        .map(|(k, v)| (k.clone(), v.len()))
        .collect();

    Ok(IndexBuildResponse {
        mode: opts.mode.as_str().to_string(),
        indexed_files: sparse.file_mapping.len(),
        indexed_chunks: sparse.chunks.len(),
        languages,
        elapsed_ms,
        warnings,
    })
}

pub fn index_clean(repo: &RepoRoot) -> AppResult<IndexCleanResponse> {
    let mut cache = ParseCache::open(repo.root());
    let cleared = cache
        .clear_search_index()
        .map_err(|e| AppError::internal(format!("clear search index: {e}")))?;
    let parse_inspection = ParseCache::inspect(repo.root());
    let cache_file = parse_inspection
        .cache_file
        .as_ref()
        .map(|p| p.display().to_string());
    Ok(IndexCleanResponse {
        cleared,
        cache_file,
    })
}

fn build_hit(hit: search::types::RankedHit, want_snippet: bool) -> SearchHit {
    let snippet = want_snippet.then(|| first_nonblank_line(&hit.chunk.content));
    SearchHit {
        path: hit.chunk.file_path,
        lines: [hit.chunk.start_line, hit.chunk.end_line],
        language: hit.chunk.language,
        score: hit.score,
        source: hit.source.as_str().to_string(),
        snippet,
    }
}

fn first_nonblank_line(content: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            // Cap to 100 chars so a one-line giant import statement doesn't
            // blow up the response.
            if trimmed.chars().count() > 100 {
                return trimmed.chars().take(100).collect::<String>() + "…";
            }
            return trimmed.to_string();
        }
    }
    String::new()
}

fn hit_under_any_prefix(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|p| {
        let needle = p.trim_end_matches('/');
        path == needle || path.starts_with(&format!("{needle}/"))
    })
}

pub fn read_file(repo: &RepoRoot, path: &str, opts: ReadOptions) -> AppResult<ReadFileResponse> {
    let resolved = repo.resolve_file(path)?;
    let loaded = load_source(&resolved)?;

    let language = loaded
        .language
        .map(|l| l.as_str().to_string())
        .unwrap_or_else(|| "plaintext".to_string());

    match opts.lines {
        Some((start, end)) => {
            if start == 0 || end == 0 {
                return Err(AppError::bad_request(
                    "--lines values are 1-indexed (got 0)",
                ));
            }
            if start > end {
                return Err(AppError::bad_request("--lines start must be <= end"));
            }

            let total = loaded.content.lines().count();
            if start > total {
                return Err(AppError::bad_request(format!(
                    "--lines start ({start}) is past end of file (file has {total} line{})",
                    if total == 1 { "" } else { "s" }
                )));
            }

            let clamped_end = end.min(total);

            let sliced: String = loaded
                .content
                .lines()
                .skip(start - 1)
                .take(clamped_end - start + 1)
                .collect::<Vec<&str>>()
                .join("\n");

            if sliced.len() > MAX_RESPONSE_BYTES {
                return Err(AppError::too_large(
                    "sliced content exceeds configured response limit",
                ));
            }

            Ok(ReadFileResponse {
                language,
                content: sliced,
                lines: Some([start, clamped_end]),
                total_lines: Some(total),
            })
        }
        None => {
            if loaded.content.len() > MAX_RESPONSE_BYTES {
                return Err(AppError::too_large(
                    "content exceeds configured response limit (use --lines S-E to slice)",
                ));
            }
            Ok(ReadFileResponse {
                language,
                content: loaded.content,
                lines: None,
                total_lines: None,
            })
        }
    }
}

pub fn read_summary(repo: &RepoRoot, path: &str) -> AppResult<ReadSummaryResponse> {
    let resolved = repo.resolve_file(path)?;
    let stat = stat_file(&resolved)?;
    let content = read_after_stat(&resolved)?;
    let detected = stat.language.clone().unwrap_or(Language::Plaintext);
    let line_stats = count_lines(content.as_bytes(), &detected);
    let language = detected.as_str().to_string();

    let mut total_symbols = 0;
    let mut kind_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut symbols = Vec::new();
    let mut note = None;

    if let Some(parseable) = stat.language.as_ref().filter(|l| l.is_parseable()) {
        let mut cache = ParseCache::open(repo.root());
        let parsed = cached_or_parsed(
            &mut cache,
            &resolved,
            &stat.metadata,
            parseable,
            Some(&content),
        )?;
        cache.save(false);

        total_symbols = parsed.len();
        for symbol in &parsed {
            *kind_counts.entry(symbol.kind.clone()).or_insert(0) += 1;
        }
        let auto_summarized = total_symbols > OUTLINE_AUTO_SUMMARY_THRESHOLD;
        let depth = if auto_summarized { Some(1) } else { None };
        symbols = parsed
            .into_iter()
            .filter(|s| within_depth(&s.qualname, depth))
            .map(|s| to_output_symbol(s, false))
            .collect();
        if auto_summarized {
            note = Some(format!(
                "file has {total_symbols} symbols; emitted top-level summary to keep response compact"
            ));
        }
    }

    Ok(ReadSummaryResponse {
        language,
        lines: line_stats.total as usize,
        bytes: stat.metadata.len() as usize,
        blank: line_stats.blank as usize,
        comment: line_stats.comment as usize,
        code: line_stats
            .total
            .saturating_sub(line_stats.blank + line_stats.comment) as usize,
        parseable: detected.is_parseable(),
        total_symbols,
        kind_counts,
        symbols,
        note,
    })
}

pub fn find(repo: &RepoRoot, query: &str, opts: FindOptions) -> AppResult<FindResponse> {
    let query = query.trim();
    if query.is_empty() {
        return Err(AppError::bad_request("query must not be empty"));
    }
    if opts.limit == 0 {
        return Err(AppError::bad_request("--limit must be at least 1"));
    }

    let mut cache = ParseCache::open(repo.root());
    let prune = opts.paths.is_empty() && opts.excludes.is_empty();

    let exclude_set = build_exclude_set(&opts.excludes)?;
    let mut files = apply_excludes(
        repo.collect_search_files(&opts.paths)?,
        exclude_set.as_ref(),
    );
    if opts.paths.is_empty() {
        files = interleave_by_top_level(files);
    }
    let result = find_resolved_files(files, query, opts, &mut cache);
    let should_prune = prune && matches!(result.as_ref(), Ok(response) if !response.truncated);
    cache.save(should_prune);
    result
}

fn find_resolved_files(
    files: Vec<ResolvedPath>,
    query: &str,
    opts: FindOptions,
    cache: &mut ParseCache,
) -> AppResult<FindResponse> {
    let needle = query.to_lowercase();
    let limit = opts.limit;
    // Cache hits + no snippet = skip the read entirely. With --snippet we still
    // need source bytes to extract the signature line.
    let needs_content_for_output = opts.snippet;

    let all_top_levels = collect_top_level_dirs(&files);
    let mut visited_top_levels: BTreeSet<String> = BTreeSet::new();

    let mut matches: Vec<FindMatch> = Vec::new();
    let mut truncated = false;
    let mut searched_files = 0usize;
    let mut all_kinds: BTreeSet<String> = BTreeSet::new();
    let collect_available_kinds = !opts.kinds.is_empty();
    // Suppression counter populated when `--per-file N` (N >= 1) caps a file.
    // Keys are full repo-relative paths; prefix-strip happens at response build.
    let mut more_in_file: BTreeMap<String, usize> = BTreeMap::new();

    let chunk_size = parallel_parse_chunk_size();
    let mut index = 0usize;

    'outer: while index < files.len() {
        if matches.len() >= limit {
            truncated = true;
            break;
        }

        let end = (index + chunk_size).min(files.len());
        let prepared: Vec<FindWorkItem> = files[index..end]
            .iter()
            .cloned()
            .map(|resolved| prepare_find_file(cache, resolved, needs_content_for_output))
            .collect();
        let outcomes = execute_find_items(prepared);

        index = end;

        for outcome in outcomes {
            if matches.len() >= limit {
                truncated = true;
                break 'outer;
            }

            let relative_path = find_result_path(&outcome);
            if let Some(top) = top_level_dir(relative_path) {
                visited_top_levels.insert(top.to_string());
            }

            let result = match outcome {
                FindWorkResult::Skipped { .. } => continue,
                FindWorkResult::Error { error, .. } => return Err(error),
                FindWorkResult::Ready(result) => result,
            };

            if let Some(update) = result.cache_update {
                insert_cache_update(cache, update);
            }

            searched_files += 1;

            // Per-file diversity counter: resets per outer-loop iter (per file).
            // When --per-file N caps a file, surplus matches are tallied into
            // `more_in_file[path]` instead of being pushed.
            let mut current_file_count = 0usize;
            for symbol in result.symbols {
                if matches.len() >= limit {
                    truncated = true;
                    break 'outer;
                }
                if qualname_matches_query(&symbol.qualname, &needle) {
                    if collect_available_kinds {
                        all_kinds.insert(symbol.kind.clone());
                    }
                    if !matches_kinds(&opts.kinds, &symbol.kind) {
                        continue;
                    }
                    if opts.per_file > 0 && current_file_count >= opts.per_file {
                        *more_in_file
                            .entry(result.relative_path.clone())
                            .or_insert(0) += 1;
                        continue;
                    }
                    let snippet = opts.snippet.then(|| {
                        snippet_for_symbol_signature(
                            result.content.as_deref().unwrap_or(""),
                            symbol.range.start_byte,
                            symbol.range.end_byte,
                        )
                    });
                    matches.push(FindMatch {
                        path: result.relative_path.clone(),
                        kind: symbol.kind.clone(),
                        qualname: symbol.qualname.clone(),
                        lines: [symbol.range.start_line, symbol.range.end_line],
                        bytes: opts
                            .bytes
                            .then_some([symbol.range.start_byte, symbol.range.end_byte]),
                        snippet,
                    });
                    current_file_count += 1;
                }
            }
        }
    }
    let available_kinds = if !opts.kinds.is_empty() && matches.is_empty() && !all_kinds.is_empty() {
        Some(all_kinds.into_iter().collect())
    } else {
        None
    };

    let note = if matches.is_empty() && searched_files == 0 {
        Some(
            "no parseable files at this path; for plaintext search across all file types, use `search`"
                .to_string(),
        )
    } else {
        None
    };

    // Decide flat-vs-grouped output. Rule: non-empty global LCP → flat (single
    // bucket already implied since common_prefix() truncates back to the last
    // `/`). Empty LCP → bucket by top-level dir; if 2+ buckets, emit groups.
    let global_prefix = common_prefix(matches.iter().map(|m| m.path.as_str()));

    let (prefix_out, matches_out, groups_out, more_in_file_out) = if !global_prefix.is_empty() {
        for m in &mut matches {
            m.path = strip_prefix(&m.path, &global_prefix);
        }
        let stripped_overflow: BTreeMap<String, usize> = more_in_file
            .into_iter()
            .map(|(p, n)| (strip_prefix(&p, &global_prefix), n))
            .collect();
        let matches_enum = if opts.terse {
            FindMatches::Terse(matches.into_iter().map(format_terse_match).collect())
        } else {
            FindMatches::Full(matches)
        };
        (global_prefix, matches_enum, Vec::new(), stripped_overflow)
    } else {
        let mut by_bucket: BTreeMap<String, Vec<FindMatch>> = BTreeMap::new();
        for m in matches {
            let key = top_level_dir(&m.path).unwrap_or("").to_string();
            by_bucket.entry(key).or_default().push(m);
        }
        if by_bucket.len() <= 1 {
            // Edge case: 0 or 1 bucket with no shared dir prefix. Stay flat.
            let merged: Vec<FindMatch> = by_bucket.into_values().flatten().collect();
            let matches_enum = if opts.terse {
                FindMatches::Terse(merged.into_iter().map(format_terse_match).collect())
            } else {
                FindMatches::Full(merged)
            };
            (String::new(), matches_enum, Vec::new(), more_in_file)
        } else {
            let mut groups: Vec<FindGroup> = Vec::new();
            for (_bucket, mut bmatches) in by_bucket {
                let bp = common_prefix(bmatches.iter().map(|m| m.path.as_str()));
                let local_overflow: BTreeMap<String, usize> = more_in_file
                    .iter()
                    .filter(|(p, _)| p.starts_with(&bp))
                    .map(|(p, n)| (strip_prefix(p, &bp), *n))
                    .collect();
                for m in &mut bmatches {
                    m.path = strip_prefix(&m.path, &bp);
                }
                let matches_enum = if opts.terse {
                    FindMatches::Terse(bmatches.into_iter().map(format_terse_match).collect())
                } else {
                    FindMatches::Full(bmatches)
                };
                groups.push(FindGroup {
                    prefix: bp,
                    matches: matches_enum,
                    more_in_file: local_overflow,
                });
            }
            let empty_matches = if opts.terse {
                FindMatches::Terse(Vec::new())
            } else {
                FindMatches::Full(Vec::new())
            };
            (String::new(), empty_matches, groups, BTreeMap::new())
        }
    };

    let unsampled_dirs = unsampled_top_levels(truncated, &all_top_levels, &visited_top_levels);

    Ok(FindResponse {
        prefix: prefix_out,
        matches: matches_out,
        groups: groups_out,
        more_in_file: more_in_file_out,
        truncated,
        searched_files,
        unsampled_dirs,
        available_kinds,
        note,
    })
}

fn format_terse_match(m: FindMatch) -> String {
    let mut s = format!("{}:{} {}({})", m.path, m.lines[0], m.qualname, m.kind);
    if let Some(snippet) = m.snippet {
        s.push_str(" :: ");
        s.push_str(&snippet);
    }
    s
}

pub fn loc_symbols(repo: &RepoRoot, opts: LocSymbolsOptions) -> AppResult<LocSymbolsResponse> {
    validate_loc_options(opts.min_lines, opts.max_lines, opts.limit)?;

    let kinds = if opts.kinds.is_empty() {
        vec!["callable".to_string()]
    } else {
        opts.kinds.clone()
    };
    let exclude_set = build_exclude_set(&opts.excludes)?;
    let files = apply_excludes(
        repo.collect_search_files(&opts.paths)?,
        exclude_set.as_ref(),
    );

    let mut cache = ParseCache::open(repo.root());
    let mut results: Vec<LocSymbolResult> = Vec::new();
    let mut scanned_files = 0usize;
    let chunk_size = parallel_parse_chunk_size();
    let mut index = 0usize;

    while index < files.len() {
        let end = (index + chunk_size).min(files.len());
        let prepared: Vec<FindWorkItem> = files[index..end]
            .iter()
            .cloned()
            .map(|resolved| prepare_loc_symbol_file(&mut cache, resolved, &opts.languages))
            .collect();
        let outcomes: Vec<FindWorkResult> =
            prepared.into_par_iter().map(execute_find_file).collect();

        index = end;

        for outcome in outcomes {
            let result = match outcome {
                FindWorkResult::Skipped { .. } => continue,
                FindWorkResult::Error { error, .. } => return Err(error),
                FindWorkResult::Ready(result) => result,
            };

            if let Some(update) = result.cache_update {
                insert_cache_update(&mut cache, update);
            }

            scanned_files += 1;
            let Some(content) = result.content.as_deref() else {
                continue;
            };
            let language = result.language.as_str().to_string();

            for symbol in result.symbols {
                if !matches_kinds(&kinds, &symbol.kind) {
                    continue;
                }

                let code = symbol_code_lines(content, &symbol, &result.language)?;
                if !line_count_in_range(code, opts.min_lines, opts.max_lines) {
                    continue;
                }

                let snippet = opts.snippet.then(|| {
                    snippet_for_symbol_signature(
                        content,
                        symbol.range.start_byte,
                        symbol.range.end_byte,
                    )
                });
                results.push(LocSymbolResult {
                    path: result.relative_path.clone(),
                    language: language.clone(),
                    kind: symbol.kind.clone(),
                    qualname: symbol.qualname.clone(),
                    lines: [symbol.range.start_line, symbol.range.end_line],
                    code,
                    bytes: opts
                        .bytes
                        .then_some([symbol.range.start_byte, symbol.range.end_byte]),
                    snippet,
                });
            }
        }
    }

    cache.save(false);
    sort_loc_symbols(&mut results, opts.sort);
    let total_matches = results.len();
    let truncated = total_matches > opts.limit;
    if truncated {
        results.truncate(opts.limit);
    }

    Ok(LocSymbolsResponse {
        paths: opts.paths,
        languages: opts.languages,
        kinds,
        min_lines: opts.min_lines,
        max_lines: opts.max_lines,
        limit: opts.limit,
        sort: opts.sort.as_str().to_string(),
        scanned_files,
        total_matches,
        truncated,
        results,
    })
}

pub fn loc_files(repo: &RepoRoot, opts: LocFilesOptions) -> AppResult<LocFilesResponse> {
    validate_loc_options(opts.min_lines, opts.max_lines, opts.limit)?;

    let include_set = build_include_set(&opts.globs)?;
    let exclude_set = build_exclude_set(&opts.excludes)?;
    let mut cache = ParseCache::open(repo.root());
    let mut results: Vec<LocFileResult> = Vec::new();
    let mut scanned_files = 0usize;

    for resolved in repo.collect_search_files(&[])? {
        if !path_matches_include_set(&resolved.relative_path, include_set.as_ref()) {
            continue;
        }
        if path_matches_exclude_set(&resolved.relative_path, exclude_set.as_ref()) {
            continue;
        }

        let metadata = match std::fs::metadata(&resolved.full_path) {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        let language =
            Language::detect(Path::new(&resolved.full_path)).unwrap_or(Language::Plaintext);
        if !language.is_parseable() {
            continue;
        }
        if !matches_languages(&opts.languages, &language) {
            continue;
        }

        let Some(stats) = cache_line_stats_for(&mut cache, &resolved, &metadata, &language) else {
            continue;
        };
        scanned_files += 1;

        let lines = stats.total as usize;
        let blank = stats.blank as usize;
        let comment = stats.comment as usize;
        let code = lines.saturating_sub(blank).saturating_sub(comment);
        if !line_count_in_range(code, opts.min_lines, opts.max_lines) {
            continue;
        }

        results.push(LocFileResult {
            path: resolved.relative_path,
            language: language.as_str().to_string(),
            lines,
            code,
            blank,
            comment,
        });
    }

    cache.save(false);
    sort_loc_files(&mut results, opts.sort);
    let total_matches = results.len();
    let truncated = total_matches > opts.limit;
    if truncated {
        results.truncate(opts.limit);
    }

    Ok(LocFilesResponse {
        globs: opts.globs,
        languages: opts.languages,
        min_lines: opts.min_lines,
        max_lines: opts.max_lines,
        limit: opts.limit,
        sort: opts.sort.as_str().to_string(),
        scanned_files,
        total_matches,
        truncated,
        results,
    })
}

pub fn files(repo: &RepoRoot, opts: FilesOptions) -> AppResult<FilesResponse> {
    if opts.limit == 0 {
        return Err(AppError::bad_request("--limit must be at least 1"));
    }

    let include_set = build_include_set(&opts.globs)?;
    let exclude_set = build_exclude_set(&opts.excludes)?;

    let mut files: Vec<String> = repo
        .collect_search_files(&[])?
        .into_iter()
        .map(|r| r.relative_path)
        .filter(|path| match &include_set {
            None => true,
            Some(set) => set.is_match(path),
        })
        .filter(|path| match &exclude_set {
            None => true,
            Some(set) => !set.is_match(path),
        })
        .collect();

    files.sort();

    let truncated = files.len() > opts.limit;
    let all_files = files;
    let mut files = all_files.clone();
    if truncated {
        files.truncate(opts.limit);
    }
    let groups = if truncated {
        build_files_groups(&all_files, &files, &opts.globs)?
    } else {
        Vec::new()
    };

    let note = if truncated {
        Some(
            "response truncated; grouped samples show first/last matches per glob or root"
                .to_string(),
        )
    } else {
        None
    };

    Ok(FilesResponse {
        files,
        truncated,
        groups,
        note,
    })
}

const FILES_GROUP_SAMPLE: usize = 3;

fn build_files_groups(
    all_files: &[String],
    shown_files: &[String],
    globs: &[String],
) -> AppResult<Vec<FilesGroup>> {
    if globs.is_empty() {
        let mut by_root: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for path in all_files {
            let root = top_level_dir(path).unwrap_or(".").to_string();
            by_root.entry(root).or_default().push(path.clone());
        }
        return Ok(by_root
            .into_iter()
            .map(|(root, paths)| {
                let shown = shown_files
                    .iter()
                    .filter(|path| top_level_dir(path).unwrap_or(".") == root)
                    .count();
                let (first, last) = sample_file_group(&paths);
                FilesGroup {
                    pattern: None,
                    root: Some(root),
                    total: paths.len(),
                    shown,
                    first,
                    last,
                }
            })
            .collect());
    }

    let mut groups = Vec::new();
    for pattern in globs {
        let glob = Glob::new(pattern)
            .map_err(|e| AppError::bad_request(format!("invalid glob `{pattern}`: {e}")))?;
        let matcher = glob.compile_matcher();
        let paths: Vec<String> = all_files
            .iter()
            .filter(|path| matcher.is_match(path.as_str()))
            .cloned()
            .collect();
        if paths.is_empty() {
            continue;
        }
        let shown = shown_files
            .iter()
            .filter(|path| matcher.is_match(path.as_str()))
            .count();
        let (first, last) = sample_file_group(&paths);
        groups.push(FilesGroup {
            pattern: Some(pattern.clone()),
            root: None,
            total: paths.len(),
            shown,
            first,
            last,
        });
    }
    Ok(groups)
}

fn sample_file_group(paths: &[String]) -> (Vec<String>, Vec<String>) {
    let first = paths.iter().take(FILES_GROUP_SAMPLE).cloned().collect();
    let last = if paths.len() > FILES_GROUP_SAMPLE {
        paths
            .iter()
            .rev()
            .take(FILES_GROUP_SAMPLE)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    } else {
        Vec::new()
    };
    (first, last)
}

pub fn langs(repo: &RepoRoot) -> AppResult<LangsResponse> {
    let resolved_files = repo.collect_search_files(&[])?;
    let mut cache = ParseCache::open(repo.root());
    // Per language: (files, total, blank, comment, parseable).
    let mut counts: BTreeMap<String, (usize, usize, usize, usize, bool)> = BTreeMap::new();

    for resolved in resolved_files {
        let metadata = match std::fs::metadata(&resolved.full_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }

        let language =
            Language::detect(Path::new(&resolved.full_path)).unwrap_or(Language::Plaintext);
        // Binary detection runs lazily inside the miss path of
        // cache_line_stats_for ~ on warm runs we never open the file.
        let Some(stats) = cache_line_stats_for(&mut cache, &resolved, &metadata, &language) else {
            continue;
        };

        let entry = counts.entry(language.as_str().to_string()).or_insert((
            0,
            0,
            0,
            0,
            language.is_parseable(),
        ));
        entry.0 += 1;
        entry.1 += stats.total as usize;
        entry.2 += stats.blank as usize;
        entry.3 += stats.comment as usize;
    }

    cache.save(false);

    let mut summaries: Vec<LangSummary> = counts
        .into_iter()
        .map(
            |(language, (files, lines, blank, comment, parseable))| LangSummary {
                language,
                files,
                lines,
                blank,
                comment,
                code: lines.saturating_sub(blank).saturating_sub(comment),
                parseable,
            },
        )
        .collect();
    summaries.sort_by(|a, b| {
        b.files
            .cmp(&a.files)
            .then_with(|| a.language.cmp(&b.language))
    });

    Ok(LangsResponse {
        languages: summaries,
    })
}

pub fn framework_next_info(repo: &RepoRoot, root: Option<&str>) -> AppResult<NextInfoResponse> {
    framework::next_info(repo, root)
}

pub fn framework_next_list_routes(
    repo: &RepoRoot,
    root: Option<&str>,
) -> AppResult<NextRoutesResponse> {
    framework::next_list_routes(repo, root)
}

pub fn framework_next_list_layouts(
    repo: &RepoRoot,
    root: Option<&str>,
) -> AppResult<NextLayoutsResponse> {
    framework::next_list_layouts(repo, root)
}

pub fn framework_next_list_server_actions(
    repo: &RepoRoot,
    root: Option<&str>,
) -> AppResult<NextServerActionsResponse> {
    framework::next_list_server_actions(repo, root)
}

pub fn cache_status(repo: &RepoRoot) -> CacheStatusResponse {
    let inspection = ParseCache::inspect(repo.root());
    let mut languages: Vec<CacheLangCount> = inspection
        .languages
        .into_iter()
        .map(|(language, files)| CacheLangCount { language, files })
        .collect();
    // Most populous languages first; alphabetical on ties.
    languages.sort_by(|a, b| {
        b.files
            .cmp(&a.files)
            .then_with(|| a.language.cmp(&b.language))
    });

    CacheStatusResponse {
        enabled: inspection.enabled,
        disabled_via_env: inspection.disabled_via_env,
        current_version: inspection.current_version,
        cache_dir: inspection
            .cache_dir
            .map(|p| p.to_string_lossy().into_owned()),
        cache_file: inspection
            .cache_file
            .map(|p| p.to_string_lossy().into_owned()),
        exists: inspection.exists,
        size_bytes: inspection.size_bytes,
        modified_unix_secs: inspection.modified_unix_secs,
        stored_version: inspection.stored_version,
        stored_repo_root: inspection.stored_repo_root,
        version_match: inspection.version_match,
        repo_root_match: inspection.repo_root_match,
        entry_count: inspection.entry_count,
        languages,
    }
}

pub fn cache_path(repo: &RepoRoot) -> CachePathResponse {
    CachePathResponse {
        path: ParseCache::cache_dir_for(repo.root()).map(|p| p.to_string_lossy().into_owned()),
    }
}

pub fn cache_clear(repo: &RepoRoot, all: bool) -> AppResult<CacheClearResponse> {
    if all {
        let outcome = ParseCache::clear_all().map_err(AppError::from)?;
        Ok(CacheClearResponse {
            scope: "all".to_string(),
            path: outcome.path.to_string_lossy().into_owned(),
            cleared: outcome.existed,
            repos_removed: Some(outcome.repos_removed),
        })
    } else {
        let outcome = ParseCache::clear(repo.root()).map_err(AppError::from)?;
        Ok(CacheClearResponse {
            scope: "repo".to_string(),
            path: outcome.path.to_string_lossy().into_owned(),
            cleared: outcome.existed,
            repos_removed: None,
        })
    }
}

// ~~ Diff (uncommitted-change review) ~~

const DEFAULT_AGAINST_REF: &str = "HEAD";
const UNTRACKED_STATUS: char = '?';

pub fn diff_overview(repo: &RepoRoot, opts: DiffOptions) -> AppResult<DiffOverviewResponse> {
    git::validate_ref(&opts.against)?;
    let git_root = git::resolve_git_root(repo.root())?;

    if !git::ref_exists(&git_root.toplevel, &opts.against) {
        return Err(AppError::bad_request(format!(
            "ref does not resolve to a commit: {} (no commits yet, or ref does not exist)",
            opts.against
        )));
    }

    let exclude_set = build_exclude_set(&opts.excludes)?;

    // Pull the relevant name-status / numstat passes. The combined-scope path
    // additionally probes staged-only and unstaged-only sets so per-file
    // staged/unstaged booleans are accurate; narrow scopes skip those probes.
    let (cached, base_ref) = scope_to_diff_args(&opts);
    let (primary_entries, primary_numstat, staged_set, unstaged_set, untracked) =
        std::thread::scope(|scope| -> AppResult<_> {
            let primary_entries_handle = (opts.scope != DiffScope::Untracked)
                .then(|| scope.spawn(|| git::name_status(&git_root.toplevel, base_ref, cached)));
            let primary_numstat_handle = (opts.scope != DiffScope::Untracked)
                .then(|| scope.spawn(|| git::numstat(&git_root.toplevel, base_ref, cached)));
            let staged_handle = (opts.scope == DiffScope::All).then(|| {
                scope.spawn(|| {
                    git::name_status(&git_root.toplevel, Some(opts.against.as_str()), true)
                })
            });
            let unstaged_handle = (opts.scope == DiffScope::All)
                .then(|| scope.spawn(|| git::name_status(&git_root.toplevel, None, false)));
            let untracked_handle = (opts.scope != DiffScope::Staged)
                .then(|| scope.spawn(|| git::list_untracked(&git_root.toplevel)));

            let primary_entries = match primary_entries_handle {
                Some(handle) => join_git_worker(handle)?,
                None => Vec::new(),
            };
            let primary_numstat = match primary_numstat_handle {
                Some(handle) => join_git_worker(handle)?,
                None => Vec::new(),
            };

            // Renames have two endpoints; insert both so cross-subtree rename
            // synthesized entries (which key on the in-subtree side) still match
            // their staged/unstaged origin.
            let mut staged: HashSet<String> = HashSet::new();
            if let Some(handle) = staged_handle {
                for e in join_git_worker(handle)? {
                    if let Some(op) = e.old_path {
                        staged.insert(op);
                    }
                    staged.insert(e.path);
                }
            }

            let mut unstaged: HashSet<String> = HashSet::new();
            if let Some(handle) = unstaged_handle {
                for e in join_git_worker(handle)? {
                    if let Some(op) = e.old_path {
                        unstaged.insert(op);
                    }
                    unstaged.insert(e.path);
                }
            }

            let untracked = match untracked_handle {
                Some(handle) => join_git_worker(handle)?,
                None => Vec::new(),
            };

            Ok((
                primary_entries,
                primary_numstat,
                staged,
                unstaged,
                untracked,
            ))
        })?;

    // Index numstat by path for quick lookup. Renames key on the new path.
    let numstat_map: BTreeMap<String, git::NumstatEntry> = primary_numstat
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect();

    let mut summaries: Vec<DiffFileSummary> = Vec::new();
    let mut filtered_count: usize = 0;

    for entry in primary_entries {
        let new_in_subtree = rebase_to_subdir(&entry.path, &git_root.repo_subdir);
        let old_in_subtree = entry
            .old_path
            .as_deref()
            .and_then(|p| rebase_to_subdir(p, &git_root.repo_subdir));
        let is_rename = entry.old_path.is_some();
        let numstat = numstat_map.get(&entry.path);
        let binary = numstat.map(|n| n.added.is_none()).unwrap_or(false);

        match (new_in_subtree, old_in_subtree, is_rename) {
            (None, None, _) => {
                filtered_count += 1;
                continue;
            }
            (Some(new_path), Some(old_path), true) => {
                // Rename whose endpoints both fall inside the subtree.
                let staged = opts.scope == DiffScope::All && staged_set.contains(&entry.path);
                let unstaged = opts.scope == DiffScope::All && unstaged_set.contains(&entry.path);
                summaries.push(DiffFileSummary {
                    path: new_path,
                    status: entry.status.to_string(),
                    old_path: Some(old_path),
                    old_path_needs_prefix: false,
                    added: numstat.and_then(|n| n.added),
                    removed: numstat.and_then(|n| n.removed),
                    staged,
                    unstaged,
                    binary,
                    note: None,
                });
            }
            (Some(new_path), None, false) => {
                // Plain in-subtree change (M / A / D / etc.); not a rename.
                let staged = opts.scope == DiffScope::All && staged_set.contains(&entry.path);
                let unstaged = opts.scope == DiffScope::All && unstaged_set.contains(&entry.path);
                summaries.push(DiffFileSummary {
                    path: new_path,
                    status: entry.status.to_string(),
                    old_path: None,
                    old_path_needs_prefix: false,
                    added: numstat.and_then(|n| n.added),
                    removed: numstat.and_then(|n| n.removed),
                    staged,
                    unstaged,
                    binary,
                    note: None,
                });
            }
            (Some(new_path), None, true) => {
                // Cross-subtree rename ARRIVING. Surface as A; the original
                // path lives outside our subtree so we can't express it as a
                // proper rename without leaking toplevel paths.
                let staged = opts.scope == DiffScope::All && staged_set.contains(&entry.path);
                let unstaged = opts.scope == DiffScope::All && unstaged_set.contains(&entry.path);
                summaries.push(DiffFileSummary {
                    path: new_path,
                    status: "A".to_string(),
                    old_path: None,
                    old_path_needs_prefix: false,
                    added: None,
                    removed: None,
                    staged,
                    unstaged,
                    binary,
                    note: Some(format!(
                        "renamed into this subtree from `{}` (outside)",
                        entry.old_path.as_deref().unwrap_or("?")
                    )),
                });
            }
            (None, Some(old_path), true) => {
                // Cross-subtree rename DEPARTING. Surface as D anchored on
                // the old path; the file is gone from our subtree.
                let staged = opts.scope == DiffScope::All
                    && entry
                        .old_path
                        .as_deref()
                        .map(|p| staged_set.contains(p))
                        .unwrap_or(false);
                let unstaged = opts.scope == DiffScope::All
                    && entry
                        .old_path
                        .as_deref()
                        .map(|p| unstaged_set.contains(p))
                        .unwrap_or(false);
                summaries.push(DiffFileSummary {
                    path: old_path,
                    status: "D".to_string(),
                    old_path: None,
                    old_path_needs_prefix: false,
                    added: None,
                    removed: None,
                    staged,
                    unstaged,
                    binary: false,
                    note: Some(format!(
                        "renamed out of this subtree to `{}` (outside)",
                        entry.path
                    )),
                });
            }
            (Some(_), Some(_), false) | (None, _, false) => unreachable!(
                "non-rename entry never has an old path; `(_, Some, false)` is impossible"
            ),
        }
    }

    for path in untracked {
        let Some(repo_path) = rebase_to_subdir(&path, &git_root.repo_subdir) else {
            filtered_count += 1;
            continue;
        };
        summaries.push(DiffFileSummary {
            path: repo_path,
            status: UNTRACKED_STATUS.to_string(),
            old_path: None,
            old_path_needs_prefix: false,
            added: None,
            removed: None,
            staged: false,
            unstaged: false,
            binary: false,
            note: None,
        });
    }

    if let Some(set) = exclude_set.as_ref() {
        summaries.retain(|f| !set.is_match(&f.path));
    }

    summaries.sort_by(|a, b| a.path.cmp(&b.path));

    let prefix = common_prefix(summaries.iter().map(|f| f.path.as_str()));
    if !prefix.is_empty() {
        for f in &mut summaries {
            f.path = strip_prefix(&f.path, &prefix);
            if let Some(op) = f.old_path.as_mut() {
                if let Some(stripped) = op.strip_prefix(&prefix) {
                    *op = stripped.to_string();
                    f.old_path_needs_prefix = true;
                }
            }
        }
    }

    let clean = summaries.is_empty();
    let against = if opts.against == DEFAULT_AGAINST_REF {
        None
    } else {
        Some(opts.against.clone())
    };
    let scope_label = match opts.scope {
        DiffScope::All => String::new(),
        DiffScope::Staged => "staged".to_string(),
        DiffScope::Unstaged => "unstaged".to_string(),
        DiffScope::Untracked => "untracked".to_string(),
    };
    let note = if filtered_count > 0 {
        Some(format!(
            "{} file(s) outside `{}/` filtered (hitagi repo root is a subdir of the git toplevel)",
            filtered_count, &git_root.repo_subdir
        ))
    } else {
        None
    };

    Ok(DiffOverviewResponse {
        prefix,
        files: summaries,
        against,
        scope: scope_label,
        clean,
        note,
    })
}

pub fn diff_file(
    repo: &RepoRoot,
    path: &str,
    opts: DiffOptions,
    drill: DiffFileOptions,
) -> AppResult<DiffFileResponse> {
    if drill.symbol.is_some() && drill.raw {
        return Err(AppError::bad_request(
            "--symbol and --raw cannot be combined",
        ));
    }
    if drill.raw && drill.body != DiffBodyMode::Full {
        return Err(AppError::bad_request("--raw and --body cannot be combined"));
    }
    if drill.raw && drill.snippet {
        return Err(AppError::bad_request(
            "--raw and --snippet cannot be combined",
        ));
    }

    git::validate_ref(&opts.against)?;
    let git_root = git::resolve_git_root(repo.root())?;
    if !git::ref_exists(&git_root.toplevel, &opts.against) {
        return Err(AppError::bad_request(format!(
            "ref does not resolve to a commit: {} (no commits yet, or ref does not exist)",
            opts.against
        )));
    }

    let normalized = repo.validate_diff_path(path)?;
    let candidates = collect_diff_candidates(&git_root, &opts)?;
    let candidate = resolve_diff_path(&candidates, &normalized, path)?.clone();

    if candidate.status == UNTRACKED_STATUS {
        return diff_untracked_file(repo, &git_root, &candidate, drill);
    }

    let (cached, base_ref) = scope_to_diff_args(&opts);
    let parsed_full = git::diff_one_file(
        &git_root.toplevel,
        base_ref,
        cached,
        &candidate.toplevel_relative,
        false,
    )?;
    let parsed_unified = if drill.raw {
        parsed_full.clone()
    } else {
        git::diff_one_file(
            &git_root.toplevel,
            base_ref,
            cached,
            &candidate.toplevel_relative,
            true,
        )?
    };

    let added = parsed_full
        .hunks
        .iter()
        .map(|h| h.added)
        .sum::<usize>()
        .into();
    let removed = parsed_full
        .hunks
        .iter()
        .map(|h| h.removed)
        .sum::<usize>()
        .into();
    let added = if parsed_full.binary {
        None
    } else {
        Some(added)
    };
    let removed = if parsed_full.binary {
        None
    } else {
        Some(removed)
    };

    let old_path_repo = candidate
        .old_path
        .as_deref()
        .and_then(|p| rebase_to_subdir(p, &git_root.repo_subdir));

    let language_label = Language::detect(Path::new(&candidate.repo_relative))
        .ok()
        .map(|l| l.as_str().to_string());

    if parsed_full.binary {
        return Ok(DiffFileResponse {
            path: candidate.repo_relative,
            status: candidate.status.to_string(),
            old_path: old_path_repo,
            added,
            removed,
            language: language_label,
            raw: None,
            hunks: None,
            note: Some("binary file ~ no diff content".to_string()),
            binary: true,
        });
    }

    if drill.raw {
        let mut note = None;
        let raw_text = if parsed_unified.raw_text.len() > MAX_FILE_BYTES {
            note = Some(format!(
                "diff text exceeded size limit ({} bytes); pass --symbol to narrow",
                parsed_unified.raw_text.len()
            ));
            String::new()
        } else {
            parsed_unified.raw_text.clone()
        };
        return Ok(DiffFileResponse {
            path: candidate.repo_relative,
            status: candidate.status.to_string(),
            old_path: old_path_repo,
            added,
            removed,
            language: language_label,
            raw: if note.is_some() { None } else { Some(raw_text) },
            hunks: None,
            note,
            binary: false,
        });
    }

    // Symbol annotation. Working-tree side for normal files, HEAD-side blob
    // for deletions ~ the latter is parsed in-memory and never cached.
    let (lang_label, language, symbols) =
        collect_symbols_for_diff(repo, &git_root, &candidate, &opts.against)?;

    let mut hunks: Vec<DiffHunk> = parsed_unified
        .hunks
        .iter()
        .map(|h| build_diff_hunk(h, &symbols, drill.body, drill.snippet))
        .collect();

    if let Some(query) = drill.symbol.as_deref() {
        language.ok_or_else(|| {
            AppError::unsupported(format!(
                "no parser for {} (cannot filter by --symbol on non-parseable files)",
                candidate.repo_relative
            ))
        })?;
        let parsed_file = ParsedFile {
            symbols: symbols.clone(),
        };
        let target = resolve_symbol(&parsed_file, query)?;
        let lo = target.range.start_line;
        let hi = target.range.end_line;
        hunks.retain(|h| h.new_lines[0] <= hi && h.new_lines[1] >= lo);
    }

    // Size-cap fallback: drop hunk bodies if the unified diff exceeds the cap.
    let mut note: Option<String> = None;
    let total_body: usize = hunks
        .iter()
        .map(|h| h.body.as_deref().map(str::len).unwrap_or(0))
        .sum();
    if total_body > MAX_FILE_BYTES {
        note = Some(format!(
            "diff exceeded size limit ({total_body} bytes); pass --symbol to narrow or --raw for the unified text"
        ));
        for h in &mut hunks {
            h.body = None;
        }
    }

    Ok(DiffFileResponse {
        path: candidate.repo_relative,
        status: candidate.status.to_string(),
        old_path: old_path_repo,
        added,
        removed,
        language: lang_label,
        raw: None,
        hunks: Some(hunks),
        note,
        binary: false,
    })
}

pub fn diff_files(
    repo: &RepoRoot,
    paths: &[String],
    opts: DiffOptions,
    drill: DiffFileOptions,
) -> AppResult<DiffMultiFileResponse> {
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        files.push(diff_file(repo, path, opts.clone(), drill.clone())?);
    }
    Ok(DiffMultiFileResponse { files })
}

pub fn diff_summary(
    repo: &RepoRoot,
    paths: &[String],
    opts: DiffOptions,
    summary: DiffSummaryOptions,
) -> AppResult<DiffSummaryResponse> {
    let overview = diff_overview(repo, opts.clone())?;
    let against = overview.against.clone();
    let scope = overview.scope.clone();
    let note = overview.note.clone();
    let all_files = summary_files_from_overview(overview);
    let selection = select_diff_summary_files(repo, &all_files, paths)?;
    let mut files = selection.files;

    if summary.symbols {
        files = annotate_diff_summary_symbols(repo, &opts, files)?;
    }

    let groups = build_diff_summary_groups(&selection.groups, &files);
    Ok(DiffSummaryResponse {
        clean: files.is_empty(),
        files,
        groups,
        against,
        scope,
        commit: summary.commit || summary.group_by_state,
        note,
    })
}

pub fn diff_paths(
    repo: &RepoRoot,
    paths: &[String],
    opts: DiffOptions,
) -> AppResult<DiffPathsResponse> {
    let overview = diff_overview(repo, opts)?;
    let against = overview.against.clone();
    let scope = overview.scope.clone();
    let note = overview.note.clone();
    let all_files = summary_files_from_overview(overview);
    let selection = select_diff_summary_files(repo, &all_files, paths)?;
    let paths: Vec<String> = selection.files.into_iter().map(|file| file.path).collect();
    Ok(DiffPathsResponse {
        clean: paths.is_empty(),
        paths,
        against,
        scope,
        note,
    })
}

pub fn diff_paths_are_all_directories(
    repo: &RepoRoot,
    paths: &[String],
    opts: DiffOptions,
) -> AppResult<bool> {
    if paths.is_empty() {
        return Ok(false);
    }
    let overview = diff_overview(repo, opts)?;
    let all_files = summary_files_from_overview(overview);
    let filters = resolve_diff_summary_filters(repo, &all_files, paths)?;
    Ok(filters.iter().all(|filter| filter.is_dir))
}

#[derive(Debug, Clone)]
struct DiffSummarySelection {
    files: Vec<DiffSummaryFile>,
    groups: Vec<DiffSummaryGroupSpec>,
}

#[derive(Debug, Clone)]
struct DiffSummaryGroupSpec {
    path: String,
    paths: Vec<String>,
}

#[derive(Debug, Clone)]
struct DiffSummaryFilter {
    path: String,
    paths: Vec<String>,
    is_dir: bool,
}

fn annotate_diff_summary_symbols(
    repo: &RepoRoot,
    opts: &DiffOptions,
    mut files: Vec<DiffSummaryFile>,
) -> AppResult<Vec<DiffSummaryFile>> {
    if files.is_empty() {
        return Ok(files);
    }

    let git_root = git::resolve_git_root(repo.root())?;
    let drill = DiffFileOptions {
        symbol: None,
        raw: false,
        body: DiffBodyMode::None,
        snippet: false,
    };

    let mut tracked = Vec::new();
    let mut tracked_paths = Vec::new();
    let mut untracked = Vec::new();
    for (idx, file) in files.iter().enumerate() {
        if file.status.starts_with(UNTRACKED_STATUS) {
            untracked.push(idx);
            continue;
        }
        let toplevel_relative = toplevel_relative_diff_path(&file.path, &git_root.repo_subdir);
        tracked.push((idx, toplevel_relative.clone()));
        tracked_paths.push(toplevel_relative);
    }

    let (cached, base_ref) = scope_to_diff_args(opts);
    let parsed_diffs =
        git::diff_many_files(&git_root.toplevel, base_ref, cached, &tracked_paths, true)?;
    let mut parsed_by_path: BTreeMap<String, usize> = BTreeMap::new();
    for (idx, parsed) in parsed_diffs.iter().enumerate() {
        if let Some(path) = parsed.new_path.as_ref().or(parsed.old_path.as_ref()) {
            parsed_by_path.insert(path.clone(), idx);
        }
        if let Some(path) = parsed.old_path.as_ref() {
            parsed_by_path.entry(path.clone()).or_insert(idx);
        }
    }

    let mut cache = ParseCache::open(repo.root());
    for (idx, toplevel_relative) in tracked {
        files[idx].language = Language::detect(Path::new(&files[idx].path))
            .ok()
            .map(|l| l.as_str().to_string());
        if files[idx].binary {
            if files[idx].note.is_none() {
                files[idx].note = Some("binary file ~ no diff content".to_string());
            }
            continue;
        }

        let status = files[idx].status.chars().next().unwrap_or('M');
        let old_path = files[idx]
            .old_path
            .as_ref()
            .map(|p| toplevel_relative_diff_path(p, &git_root.repo_subdir));
        let candidate = DiffCandidate {
            repo_relative: files[idx].path.clone(),
            toplevel_relative: toplevel_relative.clone(),
            status,
            old_path,
        };
        let (lang_label, _, symbols) = collect_symbols_for_diff_cached(
            repo,
            &git_root,
            &candidate,
            &opts.against,
            &mut cache,
        )?;
        files[idx].language = lang_label;

        let parsed = parsed_by_path
            .get(&toplevel_relative)
            .and_then(|pos| parsed_diffs.get(*pos));
        let Some(parsed) = parsed else {
            continue;
        };
        if parsed.binary {
            files[idx].binary = true;
            if files[idx].note.is_none() {
                files[idx].note = Some("binary file ~ no diff content".to_string());
            }
            continue;
        }

        let hunks: Vec<DiffHunk> = parsed
            .hunks
            .iter()
            .map(|h| build_diff_hunk(h, &symbols, DiffBodyMode::None, false))
            .collect();
        let (symbols, more_symbols) = summary_symbols(Some(&hunks));
        files[idx].symbols = symbols;
        files[idx].more_symbols = more_symbols;
    }
    cache.save(false);

    for idx in untracked {
        let base = files[idx].clone();
        let file = diff_file(repo, &base.path, opts.clone(), drill.clone())?;
        files[idx] = summary_from_file(file, true, Some(&base));
    }

    Ok(files)
}

fn toplevel_relative_diff_path(repo_relative: &str, repo_subdir: &str) -> String {
    if repo_subdir.is_empty() {
        repo_relative.to_string()
    } else if repo_relative.is_empty() {
        repo_subdir.to_string()
    } else {
        format!("{repo_subdir}/{repo_relative}")
    }
}

fn summary_files_from_overview(overview: DiffOverviewResponse) -> Vec<DiffSummaryFile> {
    let prefix = overview.prefix;
    overview
        .files
        .into_iter()
        .map(|f| {
            let old_path_needs_prefix = f.old_path_needs_prefix;
            DiffSummaryFile {
                path: format!("{}{}", prefix, f.path),
                status: f.status,
                old_path: f.old_path.map(|old| {
                    if old_path_needs_prefix {
                        format!("{}{}", prefix, old)
                    } else {
                        old
                    }
                }),
                added: f.added,
                removed: f.removed,
                language: None,
                symbols: Vec::new(),
                staged: f.staged,
                unstaged: f.unstaged,
                binary: f.binary,
                more_symbols: 0,
                note: f.note,
            }
        })
        .collect()
}

fn select_diff_summary_files(
    repo: &RepoRoot,
    all_files: &[DiffSummaryFile],
    paths: &[String],
) -> AppResult<DiffSummarySelection> {
    if paths.is_empty() {
        return Ok(DiffSummarySelection {
            files: all_files.to_vec(),
            groups: Vec::new(),
        });
    }

    let filters = resolve_diff_summary_filters(repo, all_files, paths)?;
    let by_path: BTreeMap<String, DiffSummaryFile> = all_files
        .iter()
        .map(|file| (file.path.clone(), file.clone()))
        .collect();
    let mut selected: BTreeMap<String, DiffSummaryFile> = BTreeMap::new();
    let mut groups = Vec::new();

    for filter in filters {
        for path in &filter.paths {
            if let Some(file) = by_path.get(path) {
                selected.entry(path.clone()).or_insert_with(|| file.clone());
            }
        }
        if filter.is_dir {
            groups.push(DiffSummaryGroupSpec {
                path: filter.path,
                paths: filter.paths,
            });
        }
    }

    Ok(DiffSummarySelection {
        files: selected.into_values().collect(),
        groups,
    })
}

fn resolve_diff_summary_filters(
    repo: &RepoRoot,
    all_files: &[DiffSummaryFile],
    paths: &[String],
) -> AppResult<Vec<DiffSummaryFilter>> {
    paths
        .iter()
        .map(|path| resolve_diff_summary_filter(repo, all_files, path))
        .collect()
}

fn resolve_diff_summary_filter(
    repo: &RepoRoot,
    all_files: &[DiffSummaryFile],
    path: &str,
) -> AppResult<DiffSummaryFilter> {
    let normalized = repo.validate_diff_path(path)?;
    let resolved = repo.resolve_file_or_dir(path).ok();
    let base = resolved
        .as_ref()
        .map(|r| r.relative_path.clone())
        .unwrap_or(normalized);
    let existing_dir = resolved
        .as_ref()
        .map(|r| r.full_path.is_dir())
        .unwrap_or(false);
    let exact = all_files.iter().find(|file| file.path == base);
    let prefix = format!("{base}/");
    let prefix_paths: Vec<String> = all_files
        .iter()
        .filter(|file| file.path.starts_with(&prefix))
        .map(|file| file.path.clone())
        .collect();

    if existing_dir || (!prefix_paths.is_empty() && exact.is_none()) {
        return Ok(DiffSummaryFilter {
            path: base,
            paths: prefix_paths,
            is_dir: true,
        });
    }

    if let Some(file) = exact {
        return Ok(DiffSummaryFilter {
            path: file.path.clone(),
            paths: vec![file.path.clone()],
            is_dir: false,
        });
    }

    let components = repo::parse_requested_components(&base);
    let suffix_matches: Vec<&DiffSummaryFile> = all_files
        .iter()
        .filter(|file| repo::path_components_match_suffix(&file.path, &components))
        .collect();

    match suffix_matches.len() {
        0 => Err(AppError::not_found(format!(
            "path not found in diff: {path} (run `hitagi diff --paths` to list changed files)"
        ))),
        1 => Ok(DiffSummaryFilter {
            path: suffix_matches[0].path.clone(),
            paths: vec![suffix_matches[0].path.clone()],
            is_dir: false,
        }),
        _ => {
            let shown: Vec<&str> = suffix_matches
                .iter()
                .take(10)
                .map(|file| file.path.as_str())
                .collect();
            let extra = suffix_matches.len().saturating_sub(shown.len());
            let remainder = if extra == 0 {
                String::new()
            } else {
                format!(" (+{extra} more)")
            };
            Err(AppError::bad_request(format!(
                "path is ambiguous: {path} matched multiple changed paths: {}{remainder}",
                shown.join(", ")
            )))
        }
    }
}

fn build_diff_summary_groups(
    specs: &[DiffSummaryGroupSpec],
    files: &[DiffSummaryFile],
) -> Vec<DiffSummaryGroup> {
    if specs.is_empty() {
        return Vec::new();
    }
    let by_path: BTreeMap<String, DiffSummaryFile> = files
        .iter()
        .map(|file| (file.path.clone(), file.clone()))
        .collect();
    specs
        .iter()
        .map(|spec| {
            let group_files: Vec<DiffSummaryFile> = spec
                .paths
                .iter()
                .filter_map(|path| by_path.get(path).cloned())
                .collect();
            let added = group_files.iter().map(|file| file.added.unwrap_or(0)).sum();
            let removed = group_files
                .iter()
                .map(|file| file.removed.unwrap_or(0))
                .sum();
            DiffSummaryGroup {
                path: spec.path.clone(),
                file_count: group_files.len(),
                added,
                removed,
                files: group_files,
            }
        })
        .collect()
}

fn diff_untracked_file(
    repo: &RepoRoot,
    git_root: &git::GitRoot,
    candidate: &DiffCandidate,
    drill: DiffFileOptions,
) -> AppResult<DiffFileResponse> {
    let full_path = repo.root().join(&candidate.repo_relative);
    let language_label = Language::detect(Path::new(&candidate.repo_relative))
        .ok()
        .map(|l| l.as_str().to_string());

    let bytes = std::fs::read(&full_path)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Ok(DiffFileResponse {
            path: candidate.repo_relative.clone(),
            status: UNTRACKED_STATUS.to_string(),
            old_path: None,
            added: None,
            removed: None,
            language: language_label,
            raw: None,
            hunks: None,
            note: Some(format!(
                "untracked file exceeded size limit ({} bytes); use `hitagi read --lines` to inspect slices",
                bytes.len()
            )),
            binary: false,
        });
    }
    if bytes.contains(&0) {
        return Ok(DiffFileResponse {
            path: candidate.repo_relative.clone(),
            status: UNTRACKED_STATUS.to_string(),
            old_path: None,
            added: None,
            removed: None,
            language: language_label,
            raw: None,
            hunks: None,
            note: Some("binary untracked file ~ no diff content".to_string()),
            binary: true,
        });
    }
    let content = match std::str::from_utf8(&bytes) {
        Ok(content) => content,
        Err(_) => {
            return Ok(DiffFileResponse {
                path: candidate.repo_relative.clone(),
                status: UNTRACKED_STATUS.to_string(),
                old_path: None,
                added: None,
                removed: None,
                language: language_label,
                raw: None,
                hunks: None,
                note: Some("non-UTF-8 untracked file ~ no diff content".to_string()),
                binary: true,
            });
        }
    };

    let added = count_added_lines(content);
    if drill.raw {
        let raw = synthetic_untracked_raw(&candidate.repo_relative, content, added);
        return Ok(DiffFileResponse {
            path: candidate.repo_relative.clone(),
            status: UNTRACKED_STATUS.to_string(),
            old_path: None,
            added: Some(added),
            removed: Some(0),
            language: language_label,
            raw: Some(raw),
            hunks: None,
            note: None,
            binary: false,
        });
    }

    let (lang_label, language, symbols) =
        collect_symbols_for_diff(repo, git_root, candidate, DEFAULT_AGAINST_REF)?;
    let mut hunks = if content.is_empty() {
        Vec::new()
    } else {
        let parsed = git::ParsedHunk {
            raw: git::RawHunk {
                old_start: 0,
                old_len: 0,
                new_start: 1,
                new_len: added,
            },
            body: synthetic_added_body(content),
            added,
            removed: 0,
        };
        vec![build_diff_hunk(
            &parsed,
            &symbols,
            drill.body,
            drill.snippet,
        )]
    };

    if let Some(query) = drill.symbol.as_deref() {
        language.ok_or_else(|| {
            AppError::unsupported(format!(
                "no parser for {} (cannot filter by --symbol on non-parseable files)",
                candidate.repo_relative
            ))
        })?;
        let parsed_file = ParsedFile {
            symbols: symbols.clone(),
        };
        let target = resolve_symbol(&parsed_file, query)?;
        let lo = target.range.start_line;
        let hi = target.range.end_line;
        hunks.retain(|h| h.new_lines[0] <= hi && h.new_lines[1] >= lo);
    }

    Ok(DiffFileResponse {
        path: candidate.repo_relative.clone(),
        status: UNTRACKED_STATUS.to_string(),
        old_path: None,
        added: Some(added),
        removed: Some(0),
        language: lang_label,
        raw: None,
        hunks: Some(hunks),
        note: None,
        binary: false,
    })
}

#[derive(Debug, Clone)]
struct DiffCandidate {
    repo_relative: String,
    toplevel_relative: String,
    status: char,
    old_path: Option<String>,
}

fn scope_to_diff_args(opts: &DiffOptions) -> (bool, Option<&str>) {
    match opts.scope {
        DiffScope::All => (false, Some(opts.against.as_str())),
        DiffScope::Staged => (true, Some(opts.against.as_str())),
        DiffScope::Unstaged => (false, None),
        DiffScope::Untracked => (false, None),
    }
}

fn join_git_worker<T>(handle: std::thread::ScopedJoinHandle<'_, AppResult<T>>) -> AppResult<T> {
    handle
        .join()
        .map_err(|_| AppError::internal("git worker panicked"))?
}

fn rebase_to_subdir(toplevel_path: &str, subdir: &str) -> Option<String> {
    if subdir.is_empty() {
        return Some(toplevel_path.to_string());
    }
    let prefix = format!("{subdir}/");
    if toplevel_path == subdir {
        return Some(String::new());
    }
    toplevel_path.strip_prefix(&prefix).map(String::from)
}

fn collect_diff_candidates(
    git_root: &git::GitRoot,
    opts: &DiffOptions,
) -> AppResult<Vec<DiffCandidate>> {
    let mut candidates: Vec<DiffCandidate> = Vec::new();
    if opts.scope != DiffScope::Untracked {
        let (cached, base_ref) = scope_to_diff_args(opts);
        let entries = git::name_status(&git_root.toplevel, base_ref, cached)?;

        for e in entries.into_iter() {
            let new_in = rebase_to_subdir(&e.path, &git_root.repo_subdir);
            let old_in = e
                .old_path
                .as_deref()
                .and_then(|p| rebase_to_subdir(p, &git_root.repo_subdir));
            let is_rename = e.old_path.is_some();
            match (new_in, old_in, is_rename) {
                (Some(repo_relative), _, _) => {
                    // Destination of a rename, OR a plain in-subtree change.
                    candidates.push(DiffCandidate {
                        repo_relative,
                        toplevel_relative: e.path,
                        status: e.status,
                        old_path: e.old_path,
                    });
                }
                (None, Some(repo_relative), true) => {
                    // Cross-subtree rename departing ~ surface the old side as D
                    // so `hitagi diff <old-path>` resolves and drills the deletion.
                    let toplevel_relative = e.old_path.expect("rename has old_path");
                    candidates.push(DiffCandidate {
                        repo_relative,
                        toplevel_relative,
                        status: 'D',
                        old_path: None,
                    });
                }
                _ => {}
            }
        }
    }

    if !matches!(opts.scope, DiffScope::Staged) {
        for path in git::list_untracked(&git_root.toplevel)? {
            if let Some(repo_relative) = rebase_to_subdir(&path, &git_root.repo_subdir) {
                candidates.push(DiffCandidate {
                    repo_relative,
                    toplevel_relative: path,
                    status: UNTRACKED_STATUS,
                    old_path: None,
                });
            }
        }
    }

    Ok(candidates)
}

fn resolve_diff_path<'a>(
    candidates: &'a [DiffCandidate],
    normalized: &str,
    original: &str,
) -> AppResult<&'a DiffCandidate> {
    if let Some(c) = candidates.iter().find(|c| c.repo_relative == normalized) {
        return Ok(c);
    }

    let components = repo::parse_requested_components(normalized);
    let suffix_matches: Vec<&DiffCandidate> = candidates
        .iter()
        .filter(|c| repo::path_components_match_suffix(&c.repo_relative, &components))
        .collect();

    match suffix_matches.len() {
        0 => Err(AppError::not_found(format!(
            "path not found in diff: {original} (run `hitagi diff` to list changed files)"
        ))),
        1 => Ok(suffix_matches[0]),
        _ => {
            let shown: Vec<&str> = suffix_matches
                .iter()
                .take(10)
                .map(|c| c.repo_relative.as_str())
                .collect();
            let extra = suffix_matches.len().saturating_sub(shown.len());
            let remainder = if extra == 0 {
                String::new()
            } else {
                format!(" (+{extra} more)")
            };
            Err(AppError::bad_request(format!(
                "path is ambiguous: {original} matched multiple changed paths: {}{remainder}",
                shown.join(", ")
            )))
        }
    }
}

fn collect_symbols_for_diff(
    repo: &RepoRoot,
    git_root: &git::GitRoot,
    candidate: &DiffCandidate,
    against: &str,
) -> AppResult<(Option<String>, Option<Language>, Vec<SymbolInfo>)> {
    let mut cache = ParseCache::open(repo.root());
    let result = collect_symbols_for_diff_cached(repo, git_root, candidate, against, &mut cache);
    cache.save(false);
    result
}

fn collect_symbols_for_diff_cached(
    repo: &RepoRoot,
    git_root: &git::GitRoot,
    candidate: &DiffCandidate,
    against: &str,
    cache: &mut ParseCache,
) -> AppResult<(Option<String>, Option<Language>, Vec<SymbolInfo>)> {
    let detected = Language::detect(Path::new(&candidate.repo_relative)).ok();
    let lang_label = detected.as_ref().map(|l| l.as_str().to_string());
    let parseable = detected.filter(|l| l.is_parseable());

    if candidate.status == 'D' {
        // HEAD-side blob ~ in-memory parse, no cache write.
        let Some(language) = parseable else {
            return Ok((lang_label, None, Vec::new()));
        };
        let bytes = match git::show_blob(&git_root.toplevel, against, &candidate.toplevel_relative)
        {
            Ok(b) => b,
            Err(_) => return Ok((lang_label, None, Vec::new())),
        };
        if bytes.len() > MAX_FILE_BYTES || bytes.contains(&0) {
            return Ok((lang_label, None, Vec::new()));
        }
        let Ok(content) = std::str::from_utf8(&bytes) else {
            return Ok((lang_label, None, Vec::new()));
        };
        match parse_source(&language, content) {
            Ok(parsed) => Ok((lang_label, Some(language), parsed.symbols)),
            Err(_) => Ok((lang_label, None, Vec::new())),
        }
    } else {
        // Working-tree side ~ reuse the on-disk parse cache.
        let Some(language) = parseable else {
            return Ok((lang_label, None, Vec::new()));
        };
        let full_path: PathBuf = repo.root().join(&candidate.repo_relative);
        if !full_path.exists() {
            return Ok((lang_label, None, Vec::new()));
        }
        let resolved = ResolvedPath {
            relative_path: candidate.repo_relative.clone(),
            full_path,
        };
        let stat = match stat_file(&resolved) {
            Ok(s) => s,
            Err(_) => return Ok((lang_label, None, Vec::new())),
        };
        if stat.language.as_ref() != Some(&language) {
            return Ok((lang_label, None, Vec::new()));
        }
        let symbols = match cached_or_parsed(cache, &resolved, &stat.metadata, &language, None) {
            Ok(s) => s,
            Err(_) => Vec::new(),
        };
        Ok((lang_label, Some(language), symbols))
    }
}

fn build_diff_hunk(
    h: &git::ParsedHunk,
    symbols: &[SymbolInfo],
    body_mode: DiffBodyMode,
    include_snippet: bool,
) -> DiffHunk {
    let new_lo = if h.raw.new_len == 0 {
        // Pure deletion at this anchor ~ pin to the new-side line.
        h.raw.new_start.max(1)
    } else {
        h.raw.new_start.max(1)
    };
    let new_hi = new_lo + h.raw.new_len.max(1) - 1;
    let old_lo = if h.raw.old_len == 0 {
        h.raw.old_start.max(1)
    } else {
        h.raw.old_start.max(1)
    };
    let old_hi = old_lo + h.raw.old_len.max(1) - 1;

    let span = symbols_for_line_range(symbols, new_lo, new_hi);
    let primary = span.primary;
    let spans: Vec<String> = if span.overlapping.len() > 1 {
        span.overlapping
            .iter()
            .map(|s| s.qualname.clone())
            .collect()
    } else {
        Vec::new()
    };

    DiffHunk {
        old_lines: [old_lo, old_hi],
        new_lines: [new_lo, new_hi],
        added: h.added,
        removed: h.removed,
        symbol: primary.map(|s| s.qualname.clone()),
        kind: primary.map(|s| s.kind.clone()),
        spans,
        snippet: include_snippet
            .then(|| diff_hunk_snippet(&h.body))
            .flatten(),
        body: filtered_diff_body(&h.body, body_mode),
    }
}

fn filtered_diff_body(body: &str, mode: DiffBodyMode) -> Option<String> {
    match mode {
        DiffBodyMode::Full => Some(body.to_string()),
        DiffBodyMode::ChangedLines => Some(
            body.lines()
                .filter(|line| line.starts_with('+') || line.starts_with('-'))
                .map(|line| format!("{line}\n"))
                .collect(),
        ),
        DiffBodyMode::AddedOnly => Some(
            body.lines()
                .filter(|line| line.starts_with('+'))
                .map(|line| format!("{line}\n"))
                .collect(),
        ),
        DiffBodyMode::None => None,
    }
}

fn diff_hunk_snippet(body: &str) -> Option<String> {
    body.lines()
        .find(|line| line.starts_with('+') || line.starts_with('-'))
        .map(|line| {
            let trimmed = line.trim();
            trimmed.chars().take(100).collect::<String>()
        })
}

fn count_added_lines(content: &str) -> usize {
    if content.is_empty() {
        0
    } else {
        content.lines().count()
    }
}

fn synthetic_added_body(content: &str) -> String {
    let mut body = String::new();
    for line in content.split_inclusive('\n') {
        body.push('+');
        body.push_str(line);
    }
    body
}

fn synthetic_untracked_raw(path: &str, content: &str, added: usize) -> String {
    let mut raw = String::new();
    let _ = writeln!(raw, "diff --git a/{path} b/{path}");
    raw.push_str("new file mode 100644\n");
    raw.push_str("--- /dev/null\n");
    let _ = writeln!(raw, "+++ b/{path}");
    if added > 0 {
        let _ = writeln!(raw, "@@ -0,0 +1,{added} @@");
        raw.push_str(&synthetic_added_body(content));
        if !raw.ends_with('\n') {
            raw.push('\n');
        }
    }
    raw
}

const SUMMARY_SYMBOL_LIMIT: usize = 8;

fn summary_from_file(
    file: DiffFileResponse,
    include_symbols: bool,
    base: Option<&DiffSummaryFile>,
) -> DiffSummaryFile {
    let (symbols, more_symbols) = if include_symbols {
        summary_symbols(file.hunks.as_deref())
    } else {
        (Vec::new(), 0)
    };

    DiffSummaryFile {
        path: file.path,
        status: file.status,
        old_path: file.old_path,
        added: file.added.or_else(|| base.and_then(|b| b.added)),
        removed: file.removed.or_else(|| base.and_then(|b| b.removed)),
        language: file.language,
        symbols,
        staged: base.map(|b| b.staged).unwrap_or(false),
        unstaged: base.map(|b| b.unstaged).unwrap_or(false),
        binary: file.binary,
        more_symbols,
        note: file.note.or_else(|| base.and_then(|b| b.note.clone())),
    }
}

fn summary_symbols(hunks: Option<&[DiffHunk]>) -> (Vec<String>, usize) {
    let mut all = BTreeSet::new();
    if let Some(hunks) = hunks {
        for hunk in hunks {
            if let Some(symbol) = &hunk.symbol {
                all.insert(symbol.clone());
            }
            for span in &hunk.spans {
                all.insert(span.clone());
            }
        }
    }

    let more = all.len().saturating_sub(SUMMARY_SYMBOL_LIMIT);
    let symbols = all.into_iter().take(SUMMARY_SYMBOL_LIMIT).collect();
    (symbols, more)
}

fn to_output_symbol(s: SymbolInfo, include_bytes: bool) -> OutputSymbol {
    OutputSymbol {
        kind: s.kind,
        qualname: s.qualname,
        lines: [s.range.start_line, s.range.end_line],
        bytes: include_bytes.then_some([s.range.start_byte, s.range.end_byte]),
    }
}

fn to_output_symbol_detail(s: SymbolDetail, include_bytes: bool) -> OutputSymbolDetail {
    OutputSymbolDetail {
        kind: s.kind,
        qualname: s.qualname,
        content: s.content,
        lines: [s.range.start_line, s.range.end_line],
        bytes: include_bytes.then_some([s.range.start_byte, s.range.end_byte]),
    }
}

struct FileStat {
    metadata: Metadata,
    language: Option<Language>,
}

fn stat_file(resolved: &ResolvedPath) -> AppResult<FileStat> {
    let metadata = std::fs::metadata(&resolved.full_path)?;

    if !metadata.is_file() {
        return Err(AppError::bad_request(format!(
            "path is not a file: {}",
            resolved.relative_path
        )));
    }

    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(AppError::too_large(format!(
            "file exceeds configured limit: {}",
            resolved.relative_path
        )));
    }

    let language = Language::detect(Path::new(&resolved.full_path)).ok();
    Ok(FileStat { metadata, language })
}

fn read_after_stat(resolved: &ResolvedPath) -> AppResult<String> {
    let bytes = std::fs::read(&resolved.full_path)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(AppError::too_large(format!(
            "file exceeds configured limit: {}",
            resolved.relative_path
        )));
    }

    if bytes.contains(&0) {
        return Err(AppError::InvalidUtf8(format!(
            "file appears to be binary: {}",
            resolved.relative_path
        )));
    }

    String::from_utf8(bytes).map_err(|_| {
        AppError::InvalidUtf8(format!(
            "file is not valid UTF-8: {}",
            resolved.relative_path
        ))
    })
}

fn load_source(resolved: &ResolvedPath) -> AppResult<LoadedSource> {
    let stat = stat_file(resolved)?;
    let content = read_after_stat(resolved)?;
    Ok(LoadedSource {
        language: stat.language,
        content,
    })
}

/// Return symbols for `resolved` from the cache, or parse if missing/stale.
///
/// `content`: pass the already-read source when the caller has it. Pass None
/// when content isn't otherwise needed ~ we'll read on cache miss only.
fn cached_or_parsed(
    cache: &mut ParseCache,
    resolved: &ResolvedPath,
    metadata: &Metadata,
    language: &Language,
    content: Option<&str>,
) -> AppResult<Vec<SymbolInfo>> {
    if let Some(symbols) = cache.lookup_one(&resolved.relative_path, metadata, language) {
        return Ok(symbols);
    }

    let owned;
    let source: &str = match content {
        Some(s) => s,
        None => {
            owned = read_after_stat(resolved)?;
            &owned
        }
    };

    let parsed = parse_source(language, source)?;
    let line_stats = Some(count_lines(source.as_bytes(), language));
    cache.insert(
        resolved.relative_path.clone(),
        metadata,
        language.clone(),
        line_stats,
        parsed.symbols.clone(),
    );
    Ok(parsed.symbols)
}

fn parallel_parse_chunk_size() -> usize {
    rayon::current_num_threads().saturating_mul(4).max(1)
}

fn prepare_find_file(
    cache: &mut ParseCache,
    resolved: ResolvedPath,
    needs_content: bool,
) -> FindWorkItem {
    prepare_parsed_file(cache, resolved, needs_content, &[])
}

fn prepare_loc_symbol_file(
    cache: &mut ParseCache,
    resolved: ResolvedPath,
    languages: &[String],
) -> FindWorkItem {
    prepare_parsed_file(cache, resolved, true, languages)
}

fn prepare_parsed_file(
    cache: &mut ParseCache,
    resolved: ResolvedPath,
    needs_content: bool,
    languages: &[String],
) -> FindWorkItem {
    let stat = match stat_file(&resolved) {
        Ok(value) => value,
        Err(AppError::TooLarge(_)) | Err(AppError::InvalidUtf8(_)) => {
            return FindWorkItem::Skip {
                relative_path: resolved.relative_path,
            };
        }
        Err(error) => {
            return FindWorkItem::Error {
                relative_path: resolved.relative_path,
                error,
            };
        }
    };

    let language = match stat.language.filter(|l| l.is_parseable()) {
        Some(language) => language,
        None => {
            return FindWorkItem::Skip {
                relative_path: resolved.relative_path,
            };
        }
    };
    if !matches_languages(languages, &language) {
        return FindWorkItem::Skip {
            relative_path: resolved.relative_path,
        };
    }
    let cached_symbols = cache.lookup(&resolved.relative_path, &stat.metadata, &language);

    FindWorkItem::Load(PreparedFindFile {
        resolved,
        metadata: stat.metadata,
        language,
        cached_symbols,
        needs_content,
    })
}

fn execute_find_file(item: FindWorkItem) -> FindWorkResult {
    let prepared = match item {
        FindWorkItem::Skip { relative_path } => return FindWorkResult::Skipped { relative_path },
        FindWorkItem::Error {
            relative_path,
            error,
        } => {
            return FindWorkResult::Error {
                relative_path,
                error,
            }
        }
        FindWorkItem::Load(prepared) => prepared,
    };

    if let Some(symbols) = prepared.cached_symbols {
        let content = if prepared.needs_content {
            match read_after_stat(&prepared.resolved) {
                Ok(value) => Some(value),
                Err(AppError::TooLarge(_)) | Err(AppError::InvalidUtf8(_)) => {
                    return FindWorkResult::Skipped {
                        relative_path: prepared.resolved.relative_path,
                    };
                }
                Err(error) => {
                    return FindWorkResult::Error {
                        relative_path: prepared.resolved.relative_path,
                        error,
                    };
                }
            }
        } else {
            None
        };

        return FindWorkResult::Ready(FindFileResult {
            relative_path: prepared.resolved.relative_path,
            language: prepared.language,
            symbols,
            content,
            cache_update: None,
        });
    }

    let content = match read_after_stat(&prepared.resolved) {
        Ok(value) => value,
        Err(AppError::TooLarge(_)) | Err(AppError::InvalidUtf8(_)) => {
            return FindWorkResult::Skipped {
                relative_path: prepared.resolved.relative_path,
            };
        }
        Err(error) => {
            return FindWorkResult::Error {
                relative_path: prepared.resolved.relative_path,
                error,
            };
        }
    };
    let parsed = match parse_source(&prepared.language, &content) {
        Ok(value) => value,
        Err(AppError::Parse(_)) | Err(AppError::Unsupported(_)) => {
            return FindWorkResult::Skipped {
                relative_path: prepared.resolved.relative_path,
            };
        }
        Err(error) => {
            return FindWorkResult::Error {
                relative_path: prepared.resolved.relative_path,
                error,
            };
        }
    };

    let symbols = parsed.symbols;
    let line_stats = Some(count_lines(content.as_bytes(), &prepared.language));
    let cache_update = ParseCacheUpdate {
        relative_path: prepared.resolved.relative_path.clone(),
        metadata: prepared.metadata,
        language: prepared.language.clone(),
        line_stats,
        symbols: symbols.clone(),
    };
    let content = prepared.needs_content.then_some(content);

    FindWorkResult::Ready(FindFileResult {
        relative_path: prepared.resolved.relative_path,
        language: prepared.language,
        symbols,
        content,
        cache_update: Some(cache_update),
    })
}

fn execute_find_items(items: Vec<FindWorkItem>) -> Vec<FindWorkResult> {
    // Warm, no-snippet `find` work is already in memory; Rayon only helps when
    // a chunk may need file reads or parsing.
    if items.iter().all(can_execute_find_item_inline) {
        return items.into_iter().map(execute_find_file).collect();
    }

    items.into_par_iter().map(execute_find_file).collect()
}

fn can_execute_find_item_inline(item: &FindWorkItem) -> bool {
    match item {
        FindWorkItem::Skip { .. } | FindWorkItem::Error { .. } => true,
        FindWorkItem::Load(prepared) => {
            prepared.cached_symbols.is_some() && !prepared.needs_content
        }
    }
}

fn find_result_path(result: &FindWorkResult) -> &str {
    match result {
        FindWorkResult::Skipped { relative_path } | FindWorkResult::Error { relative_path, .. } => {
            relative_path
        }
        FindWorkResult::Ready(result) => &result.relative_path,
    }
}

fn insert_cache_update(cache: &mut ParseCache, update: ParseCacheUpdate) {
    let ParseCacheUpdate {
        relative_path,
        metadata,
        language,
        line_stats,
        symbols,
    } = update;
    cache.insert(relative_path, &metadata, language, line_stats, symbols);
}

/// Returns the cached line count for `resolved` if (mtime, size, language)
/// match; otherwise reads the file, counts newlines, and writes a lang-only
/// stamp into the cache (works for both parseable and non-parseable
/// languages ~ the stamp doesn't satisfy `cache.lookup` for symbols, so a
/// later outline/find/symbol call still re-parses and upgrades the entry).
/// Binary / oversized / unreadable files yield `None` and are NOT cached
/// (so a future content-change becomes detectable on the next call).
fn cache_line_stats_for(
    cache: &mut ParseCache,
    resolved: &ResolvedPath,
    metadata: &Metadata,
    language: &Language,
) -> Option<LineStats> {
    if let Some(stats) = cache.lookup_line_stats(&resolved.relative_path, metadata, language) {
        return Some(stats);
    }
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return None;
    }
    let bytes = std::fs::read(&resolved.full_path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    let stats = count_lines(&bytes, language);
    cache.insert_lang_only(
        resolved.relative_path.clone(),
        metadata,
        language.clone(),
        stats,
    );
    Some(stats)
}

fn validate_loc_options(
    min_lines: Option<usize>,
    max_lines: Option<usize>,
    limit: usize,
) -> AppResult<()> {
    if limit == 0 {
        return Err(AppError::bad_request("--limit must be at least 1"));
    }
    if let (Some(min), Some(max)) = (min_lines, max_lines) {
        if min > max {
            return Err(AppError::bad_request(
                "--min-lines cannot be greater than --max-lines",
            ));
        }
    }
    Ok(())
}

fn line_count_in_range(code: usize, min_lines: Option<usize>, max_lines: Option<usize>) -> bool {
    min_lines.map_or(true, |min| code >= min) && max_lines.map_or(true, |max| code <= max)
}

fn symbol_code_lines(source: &str, symbol: &SymbolInfo, language: &Language) -> AppResult<usize> {
    if symbol.range.end_byte > source.len() || symbol.range.start_byte > symbol.range.end_byte {
        return Err(AppError::internal("invalid symbol byte range"));
    }

    let bytes = &source.as_bytes()[symbol.range.start_byte..symbol.range.end_byte];
    let stats = count_lines(bytes, language);
    Ok(stats.total.saturating_sub(stats.blank + stats.comment) as usize)
}

fn sort_loc_symbols(results: &mut [LocSymbolResult], sort: LocSort) {
    match sort {
        LocSort::CodeDesc => results.sort_by(|a, b| {
            b.code
                .cmp(&a.code)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.lines[0].cmp(&b.lines[0]))
                .then_with(|| a.qualname.cmp(&b.qualname))
        }),
        LocSort::CodeAsc => results.sort_by(|a, b| {
            a.code
                .cmp(&b.code)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.lines[0].cmp(&b.lines[0]))
                .then_with(|| a.qualname.cmp(&b.qualname))
        }),
        LocSort::Path => results.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.lines[0].cmp(&b.lines[0]))
                .then_with(|| a.qualname.cmp(&b.qualname))
        }),
    }
}

fn sort_loc_files(results: &mut [LocFileResult], sort: LocSort) {
    match sort {
        LocSort::CodeDesc => {
            results.sort_by(|a, b| b.code.cmp(&a.code).then_with(|| a.path.cmp(&b.path)))
        }
        LocSort::CodeAsc => {
            results.sort_by(|a, b| a.code.cmp(&b.code).then_with(|| a.path.cmp(&b.path)))
        }
        LocSort::Path => results.sort_by(|a, b| a.path.cmp(&b.path)),
    }
}

fn matches_languages(filter: &[String], language: &Language) -> bool {
    filter.is_empty()
        || filter
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(language.as_str()))
}

fn path_matches_include_set(path: &str, set: Option<&GlobSet>) -> bool {
    set.map_or(true, |set| set.is_match(path))
}

fn path_matches_exclude_set(path: &str, set: Option<&GlobSet>) -> bool {
    set.map_or(false, |set| set.is_match(path))
}

fn matches_kinds(filter: &[String], kind: &str) -> bool {
    filter.is_empty() || filter.iter().any(|k| kind_filter_matches(k, kind))
}

fn qualname_matches_query(qualname: &str, needle_lower: &str) -> bool {
    if qualname.is_ascii() && needle_lower.is_ascii() {
        return contains_ascii_case_insensitive(qualname.as_bytes(), needle_lower.as_bytes());
    }
    qualname.to_lowercase().contains(needle_lower)
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle_lower: &[u8]) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if needle_lower.len() > haystack.len() {
        return false;
    }

    haystack
        .windows(needle_lower.len())
        .any(|window| window.eq_ignore_ascii_case(needle_lower))
}

fn kind_filter_matches(filter: &str, kind: &str) -> bool {
    if filter.eq_ignore_ascii_case(kind) {
        return true;
    }

    if filter.eq_ignore_ascii_case("callable") {
        return kind_matches_any(kind, &["function", "method", "arrow_function"]);
    }
    if filter.eq_ignore_ascii_case("container") {
        return kind_matches_any(
            kind,
            &["class", "struct", "interface", "enum", "trait", "object"],
        );
    }
    if filter.eq_ignore_ascii_case("value") {
        return kind_matches_any(
            kind,
            &["property", "field", "variant", "variable", "constant"],
        );
    }

    false
}

fn kind_matches_any(kind: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| kind.eq_ignore_ascii_case(candidate))
}

fn within_depth(qualname: &str, max_depth: Option<usize>) -> bool {
    match max_depth {
        Some(d) => qualname.matches('.').count() < d,
        None => true,
    }
}

fn build_include_set(globs: &[String]) -> AppResult<Option<GlobSet>> {
    if globs.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in globs {
        let glob = Glob::new(pattern)
            .map_err(|e| AppError::bad_request(format!("invalid glob `{pattern}`: {e}")))?;
        builder.add(glob);
    }
    let set = builder
        .build()
        .map_err(|e| AppError::bad_request(format!("failed to build glob set: {e}")))?;
    Ok(Some(set))
}

pub(crate) fn build_exclude_set(excludes: &[String]) -> AppResult<Option<GlobSet>> {
    if excludes.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in excludes {
        for expanded in expand_exclude_pattern(pattern) {
            let glob = Glob::new(&expanded).map_err(|e| {
                AppError::bad_request(format!("invalid --exclude glob `{pattern}`: {e}"))
            })?;
            builder.add(glob);
        }
    }
    let set = builder
        .build()
        .map_err(|e| AppError::bad_request(format!("failed to build exclude set: {e}")))?;
    Ok(Some(set))
}

fn expand_exclude_pattern(pattern: &str) -> Vec<String> {
    let has_glob_chars = pattern.contains(['*', '?', '[', ']', '{', '}']);
    let has_slash = pattern.contains('/');

    if has_glob_chars || has_slash {
        vec![pattern.to_string()]
    } else {
        // bare name like `vendor` or `target` ~ exclude any path containing it as a directory
        vec![
            format!("{pattern}/**"),
            format!("**/{pattern}/**"),
            pattern.to_string(),
        ]
    }
}

pub(crate) fn apply_excludes(
    files: Vec<ResolvedPath>,
    excludes: Option<&GlobSet>,
) -> Vec<ResolvedPath> {
    match excludes {
        None => files,
        Some(set) => files
            .into_iter()
            .filter(|f| !set.is_match(&f.relative_path))
            .collect(),
    }
}

/// First path component (directory) of a repo-relative path, or None if the
/// path is a single file at the root (no slash). Used as the bucket key for
/// truncation-bias bookkeeping.
fn top_level_dir(relative_path: &str) -> Option<&str> {
    relative_path.split_once('/').map(|(head, _)| head)
}

/// All distinct top-level dirs present in `files`. Sorted (BTreeSet).
fn collect_top_level_dirs(files: &[ResolvedPath]) -> BTreeSet<String> {
    files
        .iter()
        .filter_map(|f| top_level_dir(&f.relative_path).map(String::from))
        .collect()
}

/// Re-order `files` so iteration round-robins across top-level buckets.
/// Within each bucket the input (alphabetical/walk) order is preserved.
/// Files at repo root (no `/` in their relative path) share the `""` bucket.
/// Bucket order is BTreeMap iteration order = lexicographic. Used by find /
/// search when no explicit paths were given, so a `--limit` walk produces a
/// fair sample across top-level subdirs instead of exhausting the budget on
/// whichever dir comes first alphabetically.
fn interleave_by_top_level(files: Vec<ResolvedPath>) -> Vec<ResolvedPath> {
    let mut buckets: BTreeMap<String, Vec<ResolvedPath>> = BTreeMap::new();
    for f in files {
        let key = top_level_dir(&f.relative_path).unwrap_or("").to_string();
        buckets.entry(key).or_default().push(f);
    }
    let mut bucket_vecs: Vec<Vec<ResolvedPath>> = buckets
        .into_values()
        .map(|mut v| {
            v.reverse();
            v
        })
        .collect();
    let mut out = Vec::new();
    loop {
        let mut took = false;
        for b in bucket_vecs.iter_mut() {
            if let Some(item) = b.pop() {
                out.push(item);
                took = true;
            }
        }
        if !took {
            break;
        }
    }
    out
}

/// `all - visited` when `truncated`, else empty. Sorted output.
fn unsampled_top_levels(
    truncated: bool,
    all_top_levels: &BTreeSet<String>,
    visited_top_levels: &BTreeSet<String>,
) -> Vec<String> {
    if !truncated {
        return Vec::new();
    }
    all_top_levels
        .difference(visited_top_levels)
        .cloned()
        .collect()
}

fn common_prefix<'a>(paths: impl IntoIterator<Item = &'a str>) -> String {
    let mut paths = paths.into_iter();
    let Some(mut prefix) = paths.next() else {
        return String::new();
    };

    for path in paths {
        let mut end = 0;
        for ((i, left), (_, right)) in prefix.char_indices().zip(path.char_indices()) {
            if left != right {
                break;
            }
            end = i + left.len_utf8();
        }
        prefix = &prefix[..end];
    }

    match prefix.rfind('/') {
        Some(i) => prefix[..=i].to_string(),
        None => String::new(),
    }
}

fn strip_prefix(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix).unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    struct TempRepo {
        root: PathBuf,
    }

    impl TempRepo {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root =
                std::env::temp_dir().join(format!("hitagi-{name}-{}-{unique}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn resolved(&self, relative_path: &str) -> ResolvedPath {
            ResolvedPath {
                relative_path: relative_path.to_string(),
                full_path: self.root.join(relative_path),
            }
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn find_stops_loading_files_when_global_limit_is_filled() {
        let repo = TempRepo::new("find-limit");
        fs::write(repo.root.join("first.rs"), "pub struct AuthService {}\n").unwrap();

        let response = find_resolved_files(
            vec![repo.resolved("first.rs"), repo.resolved("missing.rs")],
            "Auth",
            FindOptions {
                paths: Vec::new(),
                excludes: Vec::new(),
                kinds: Vec::new(),
                limit: 1,
                bytes: false,
                snippet: false,
                terse: false,
                per_file: 0,
            },
            &mut ParseCache::disabled(),
        )
        .unwrap();

        assert_eq!(response.truncated, true);
        assert_eq!(response.searched_files, 1);
        let FindMatches::Full(matches) = response.matches else {
            panic!("expected full find matches");
        };
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "first.rs");
        assert_eq!(matches[0].qualname, "AuthService");
    }

    #[test]
    fn find_prefix_handles_unicode_path_components() {
        let repo = TempRepo::new("find-unicode-prefix");
        fs::create_dir_all(repo.root.join("a/仩")).unwrap();
        fs::create_dir_all(repo.root.join("a/重")).unwrap();
        fs::write(repo.root.join("a/仩/one.rs"), "pub struct OneThing {}\n").unwrap();
        fs::write(repo.root.join("a/重/two.rs"), "pub struct TwoThing {}\n").unwrap();

        let response = find_resolved_files(
            vec![repo.resolved("a/仩/one.rs"), repo.resolved("a/重/two.rs")],
            "Thing",
            FindOptions {
                paths: Vec::new(),
                excludes: Vec::new(),
                kinds: Vec::new(),
                limit: 10,
                bytes: false,
                snippet: false,
                terse: false,
                per_file: 0,
            },
            &mut ParseCache::disabled(),
        )
        .unwrap();

        assert_eq!(response.prefix, "a/");
        let FindMatches::Full(matches) = response.matches else {
            panic!("expected full find matches");
        };
        let paths: Vec<&str> = matches.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, vec!["仩/one.rs", "重/two.rs"]);
    }

    #[test]
    fn qualname_query_match_preserves_case_insensitive_semantics() {
        assert!(qualname_matches_query("SearchEngine.runBM25", "search"));
        assert!(qualname_matches_query("SearchEngine.runBM25", "bm25"));
        assert!(!qualname_matches_query("SearchEngine.runBM25", "dense"));
        assert!(qualname_matches_query("İndex", &"İ".to_lowercase()));
    }

    #[test]
    fn kind_alias_match_is_case_insensitive_without_allocating_aliases() {
        assert!(kind_filter_matches("CALLABLE", "function"));
        assert!(kind_filter_matches("Container", "struct"));
        assert!(kind_filter_matches("value", "constant"));
        assert!(!kind_filter_matches("callable", "struct"));
    }
}
