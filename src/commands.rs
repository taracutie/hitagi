use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::Metadata;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::{
    cache::ParseCache,
    error::{AppError, AppResult},
    git,
    lang::Language,
    models::{
        CacheClearResponse, CacheLangCount, CachePathResponse, CacheStatusResponse,
        DiffFileResponse, DiffFileSummary, DiffHunk, DiffOverviewResponse, FilesResponse,
        FindMatch, FindMatches, FindResponse, LangSummary, LangsResponse, OutlineResponse,
        OutputSymbol, OutputSymbolDetail, ReadFileResponse, SearchResponse, SymbolDetail,
        SymbolInfo, SymbolResponse,
    },
    parser::{parse_source, ParsedFile},
    queries::{
        resolve_symbol, search_file, search_file_plain, snippet_for_symbol_signature,
        symbol_detail, symbols_for_line_range,
    },
    repo::{self, RepoRoot, ResolvedPath},
};

pub const MAX_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

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

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub paths: Vec<String>,
    pub excludes: Vec<String>,
    pub limit: usize,
    pub snippet: bool,
}

#[derive(Debug, Default, Clone)]
pub struct ReadOptions {
    pub lines: Option<(usize, usize)>,
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
}

#[derive(Debug, Clone)]
pub struct FilesOptions {
    pub globs: Vec<String>,
    pub excludes: Vec<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffScope {
    All,
    Staged,
    Unstaged,
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
}

struct LoadedSource {
    language: Option<Language>,
    content: String,
}

pub fn outline(repo: &RepoRoot, path: &str, opts: OutlineOptions) -> AppResult<OutlineResponse> {
    let resolved = repo.resolve_file(path)?;
    let mut cache = ParseCache::open(repo.root());

    let stat = stat_file(&resolved)?;
    let language = stat.language.filter(|l| l.is_parseable()).ok_or_else(|| {
        AppError::unsupported(format!("no parser for {}", resolved.relative_path))
    })?;

    let symbols = cached_or_parsed(&mut cache, &resolved, &stat.metadata, language, None)?;
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
    let auto_summarized =
        !caller_constrained && total_symbols > OUTLINE_AUTO_SUMMARY_THRESHOLD;
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
        language,
        Some(&content),
    )?;
    cache.save(false);

    let parsed = ParsedFile { language, symbols };
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

    let mut cache = ParseCache::open(repo.root());
    let prune = opts.paths.is_empty() && opts.excludes.is_empty();

    let exclude_set = build_exclude_set(&opts.excludes)?;
    let files = apply_excludes(
        repo.collect_search_files(&opts.paths)?,
        exclude_set.as_ref(),
    );
    let result = search_resolved_files(files, query, opts.limit, opts.snippet, &mut cache);
    let should_prune = prune && matches!(result.as_ref(), Ok(response) if !response.truncated);
    cache.save(should_prune);
    result
}

fn search_resolved_files(
    files: Vec<ResolvedPath>,
    query: &str,
    limit: usize,
    snippet: bool,
    cache: &mut ParseCache,
) -> AppResult<SearchResponse> {
    let all_top_levels = collect_top_level_dirs(&files);
    let mut visited_top_levels: BTreeSet<String> = BTreeSet::new();

    let mut raw_results: Vec<(String, Vec<String>)> = Vec::new();
    let mut total = 0usize;
    let mut truncated = false;

    for resolved in files {
        if total >= limit {
            truncated = true;
            break;
        }
        if let Some(top) = top_level_dir(&resolved.relative_path) {
            visited_top_levels.insert(top.to_string());
        }
        let remaining = limit - total;
        let file_limit = remaining.saturating_add(1);

        // search always needs content (substring grep on source). Stat first so
        // we can use the metadata for the cache key, then read.
        let stat = match stat_file(&resolved) {
            Ok(value) => value,
            Err(AppError::TooLarge(_)) | Err(AppError::InvalidUtf8(_)) => continue,
            Err(error) => return Err(error),
        };

        let content = match read_after_stat(&resolved) {
            Ok(value) => value,
            Err(AppError::TooLarge(_)) | Err(AppError::InvalidUtf8(_)) => continue,
            Err(error) => return Err(error),
        };

        let mut matches: Vec<String> = match stat.language.filter(|l| l.is_parseable()) {
            Some(language) => {
                let symbols = match cached_or_parsed(
                    cache,
                    &resolved,
                    &stat.metadata,
                    language,
                    Some(&content),
                ) {
                    Ok(s) => s,
                    Err(AppError::Parse(_)) | Err(AppError::Unsupported(_)) => continue,
                    Err(error) => return Err(error),
                };
                let parsed = ParsedFile { language, symbols };
                search_file(
                    &parsed,
                    &content,
                    &resolved.relative_path,
                    query,
                    file_limit,
                    snippet,
                )
            }
            None => search_file_plain(&content, query, file_limit, snippet),
        };

        if matches.len() > remaining {
            matches.truncate(remaining);
            truncated = true;
        }

        if matches.is_empty() {
            continue;
        }

        total += matches.len();
        raw_results.push((resolved.relative_path.clone(), matches));
        if truncated {
            break;
        }
    }

    let result_paths: Vec<String> = raw_results.iter().map(|(p, _)| p.clone()).collect();
    let prefix = common_prefix(&result_paths);

    let mut results: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, matches) in raw_results {
        let key = strip_prefix(&path, &prefix);
        results.entry(key).or_default().extend(matches);
    }

    let unsampled_dirs = unsampled_top_levels(truncated, &all_top_levels, &visited_top_levels);

    Ok(SearchResponse {
        prefix,
        results,
        truncated,
        unsampled_dirs,
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
    let files = apply_excludes(
        repo.collect_search_files(&opts.paths)?,
        exclude_set.as_ref(),
    );
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

    'outer: for resolved in files {
        if matches.len() >= limit {
            truncated = true;
            break;
        }
        if let Some(top) = top_level_dir(&resolved.relative_path) {
            visited_top_levels.insert(top.to_string());
        }

        let stat = match stat_file(&resolved) {
            Ok(value) => value,
            Err(AppError::TooLarge(_)) | Err(AppError::InvalidUtf8(_)) => continue,
            Err(error) => return Err(error),
        };

        let language = match stat.language.filter(|l| l.is_parseable()) {
            Some(l) => l,
            None => continue,
        };

        // Try the cache first. On hit + no snippet, skip read+parse entirely.
        // On hit + snippet, skip parse but still read for the snippet bytes.
        // On miss, fall through to read+parse and populate cache.
        let (symbols, content) = if let Some(symbols) =
            cache.lookup(&resolved.relative_path, &stat.metadata, language)
        {
            let content = if needs_content_for_output {
                match read_after_stat(&resolved) {
                    Ok(c) => Some(c),
                    Err(AppError::TooLarge(_)) | Err(AppError::InvalidUtf8(_)) => continue,
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            (symbols, content)
        } else {
            let content = match read_after_stat(&resolved) {
                Ok(c) => c,
                Err(AppError::TooLarge(_)) | Err(AppError::InvalidUtf8(_)) => continue,
                Err(error) => return Err(error),
            };
            let parsed = match parse_source(language, &content) {
                Ok(p) => p,
                Err(AppError::Parse(_)) | Err(AppError::Unsupported(_)) => continue,
                Err(error) => return Err(error),
            };
            let line_count = Some(count_newlines(content.as_bytes()));
            cache.insert(
                resolved.relative_path.clone(),
                &stat.metadata,
                language,
                line_count,
                parsed.symbols.clone(),
            );
            (parsed.symbols, Some(content))
        };

        searched_files += 1;

        for symbol in symbols {
            if matches.len() >= limit {
                truncated = true;
                break 'outer;
            }
            if symbol.qualname.to_lowercase().contains(&needle) {
                all_kinds.insert(symbol.kind.clone());
                if !matches_kinds(&opts.kinds, &symbol.kind) {
                    continue;
                }
                let snippet = opts.snippet.then(|| {
                    snippet_for_symbol_signature(
                        content.as_deref().unwrap_or(""),
                        symbol.range.start_byte,
                        symbol.range.end_byte,
                    )
                });
                matches.push(FindMatch {
                    path: resolved.relative_path.clone(),
                    kind: symbol.kind.clone(),
                    name: symbol.name.clone(),
                    qualname: symbol.qualname.clone(),
                    lines: [symbol.range.start_line, symbol.range.end_line],
                    bytes: opts
                        .bytes
                        .then_some([symbol.range.start_byte, symbol.range.end_byte]),
                    snippet,
                });
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

    let path_list: Vec<String> = matches.iter().map(|m| m.path.clone()).collect();
    let prefix = common_prefix(&path_list);
    if !prefix.is_empty() {
        for m in &mut matches {
            m.path = strip_prefix(&m.path, &prefix);
        }
    }

    let matches = if opts.terse {
        FindMatches::Terse(matches.into_iter().map(format_terse_match).collect())
    } else {
        FindMatches::Full(matches)
    };

    let unsampled_dirs = unsampled_top_levels(truncated, &all_top_levels, &visited_top_levels);

    Ok(FindResponse {
        prefix,
        matches,
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
    if truncated {
        files.truncate(opts.limit);
    }

    let note = if truncated {
        Some("response truncated; pass globs (e.g. \"**/*.rs\") or --limit N to refine".to_string())
    } else {
        None
    };

    Ok(FilesResponse {
        files,
        truncated,
        note,
    })
}

pub fn langs(repo: &RepoRoot) -> AppResult<LangsResponse> {
    let resolved_files = repo.collect_search_files(&[])?;
    let mut cache = ParseCache::open(repo.root());
    // Tracks (files, lines, parseable) per language label.
    let mut counts: BTreeMap<String, (usize, usize, bool)> = BTreeMap::new();

    for resolved in resolved_files {
        let metadata = match std::fs::metadata(&resolved.full_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }

        let language = Language::detect(Path::new(&resolved.full_path)).unwrap_or(Language::Plaintext);
        // Binary detection runs lazily inside the miss path of
        // cache_line_count_for ~ on warm runs we never open the file.
        let Some(lines) = cache_line_count_for(&mut cache, &resolved, &metadata, language) else {
            continue;
        };

        let entry = counts
            .entry(language.as_str().to_string())
            .or_insert((0, 0, language.is_parseable()));
        entry.0 += 1;
        entry.1 += lines as usize;
    }

    cache.save(false);

    let mut summaries: Vec<LangSummary> = counts
        .into_iter()
        .map(|(language, (files, lines, parseable))| LangSummary {
            language,
            files,
            lines,
            parseable,
        })
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
    let primary_entries = git::name_status(&git_root.toplevel, base_ref, cached)?;
    let primary_numstat = git::numstat(&git_root.toplevel, base_ref, cached)?;

    let (staged_set, unstaged_set) = if opts.scope == DiffScope::All {
        // Renames have two endpoints; insert both so cross-subtree rename
        // synthesized entries (which key on the in-subtree side) still match
        // their staged/unstaged origin.
        let mut staged: HashSet<String> = HashSet::new();
        for e in
            git::name_status(&git_root.toplevel, Some(opts.against.as_str()), true)?.into_iter()
        {
            if let Some(op) = e.old_path {
                staged.insert(op);
            }
            staged.insert(e.path);
        }
        let mut unstaged: HashSet<String> = HashSet::new();
        for e in git::name_status(&git_root.toplevel, None, false)?.into_iter() {
            if let Some(op) = e.old_path {
                unstaged.insert(op);
            }
            unstaged.insert(e.path);
        }
        (staged, unstaged)
    } else {
        (HashSet::new(), HashSet::new())
    };

    let untracked = if opts.scope == DiffScope::Staged {
        Vec::new()
    } else {
        git::list_untracked(&git_root.toplevel)?
    };

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
                let staged =
                    opts.scope == DiffScope::All && staged_set.contains(&entry.path);
                let unstaged =
                    opts.scope == DiffScope::All && unstaged_set.contains(&entry.path);
                summaries.push(DiffFileSummary {
                    path: new_path,
                    status: entry.status.to_string(),
                    old_path: Some(old_path),
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
                let staged =
                    opts.scope == DiffScope::All && staged_set.contains(&entry.path);
                let unstaged =
                    opts.scope == DiffScope::All && unstaged_set.contains(&entry.path);
                summaries.push(DiffFileSummary {
                    path: new_path,
                    status: entry.status.to_string(),
                    old_path: None,
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
                let staged =
                    opts.scope == DiffScope::All && staged_set.contains(&entry.path);
                let unstaged =
                    opts.scope == DiffScope::All && unstaged_set.contains(&entry.path);
                summaries.push(DiffFileSummary {
                    path: new_path,
                    status: "A".to_string(),
                    old_path: None,
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

    let path_list: Vec<String> = summaries.iter().map(|f| f.path.clone()).collect();
    let prefix = common_prefix(&path_list);
    if !prefix.is_empty() {
        for f in &mut summaries {
            f.path = strip_prefix(&f.path, &prefix);
            if let Some(op) = f.old_path.as_mut() {
                *op = strip_prefix(op, &prefix);
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

    // Untracked files have no diff to show ~ surface a clear note.
    if candidate.status == UNTRACKED_STATUS {
        let language = Language::detect(Path::new(&candidate.repo_relative))
            .ok()
            .map(|l| l.as_str().to_string());
        return Ok(DiffFileResponse {
            path: candidate.repo_relative,
            status: UNTRACKED_STATUS.to_string(),
            old_path: None,
            added: None,
            removed: None,
            language,
            raw: None,
            hunks: None,
            note: Some(
                "untracked file ~ no diff to show, use `hitagi read` for content".to_string(),
            ),
            binary: false,
        });
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
    let added = if parsed_full.binary { None } else { Some(added) };
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
    let (lang_label, language, symbols) = collect_symbols_for_diff(repo, &git_root, &candidate, &opts.against)?;

    let mut hunks: Vec<DiffHunk> = parsed_unified
        .hunks
        .iter()
        .map(|h| build_diff_hunk(h, &symbols))
        .collect();

    if let Some(query) = drill.symbol.as_deref() {
        let language = language.ok_or_else(|| {
            AppError::unsupported(format!(
                "no parser for {} (cannot filter by --symbol on non-parseable files)",
                candidate.repo_relative
            ))
        })?;
        let parsed_file = ParsedFile {
            language,
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
    }
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
    let (cached, base_ref) = scope_to_diff_args(opts);
    let entries = git::name_status(&git_root.toplevel, base_ref, cached)?;

    let mut candidates: Vec<DiffCandidate> = Vec::new();
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
    let detected = Language::detect(Path::new(&candidate.repo_relative)).ok();
    let lang_label = detected.map(|l| l.as_str().to_string());
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
        match parse_source(language, content) {
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
            repo_root: repo.root().to_path_buf(),
            relative_path: candidate.repo_relative.clone(),
            full_path,
        };
        let stat = match stat_file(&resolved) {
            Ok(s) => s,
            Err(_) => return Ok((lang_label, None, Vec::new())),
        };
        if stat.language.filter(|l| *l == language).is_none() {
            return Ok((lang_label, None, Vec::new()));
        }
        let mut cache = ParseCache::open(repo.root());
        let symbols = match cached_or_parsed(&mut cache, &resolved, &stat.metadata, language, None)
        {
            Ok(s) => s,
            Err(_) => Vec::new(),
        };
        cache.save(false);
        Ok((lang_label, Some(language), symbols))
    }
}

fn build_diff_hunk(h: &git::ParsedHunk, symbols: &[SymbolInfo]) -> DiffHunk {
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
        span.overlapping.iter().map(|s| s.qualname.clone()).collect()
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
        body: Some(h.body.clone()),
    }
}

fn to_output_symbol(s: SymbolInfo, include_bytes: bool) -> OutputSymbol {
    OutputSymbol {
        kind: s.kind,
        name: s.name,
        qualname: s.qualname,
        lines: [s.range.start_line, s.range.end_line],
        bytes: include_bytes.then_some([s.range.start_byte, s.range.end_byte]),
    }
}

fn to_output_symbol_detail(s: SymbolDetail, include_bytes: bool) -> OutputSymbolDetail {
    OutputSymbolDetail {
        kind: s.kind,
        name: s.name,
        qualname: s.qualname,
        content: s.content,
        lines: [s.range.start_line, s.range.end_line],
        bytes: include_bytes.then_some([s.range.start_byte, s.range.end_byte]),
    }
}

fn first_chunk_has_nul(path: &Path) -> bool {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = Vec::with_capacity(8192);
    if file.take(8192).read_to_end(&mut buf).is_err() {
        return false;
    }
    buf.contains(&0)
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
    language: Language,
    content: Option<&str>,
) -> AppResult<Vec<SymbolInfo>> {
    if let Some(symbols) = cache.lookup(&resolved.relative_path, metadata, language) {
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
    let line_count = Some(count_newlines(source.as_bytes()));
    cache.insert(
        resolved.relative_path.clone(),
        metadata,
        language,
        line_count,
        parsed.symbols.clone(),
    );
    Ok(parsed.symbols)
}

/// Returns the cached line count for `resolved` if (mtime, size, language)
/// match; otherwise reads the file, counts newlines, and writes a lang-only
/// stamp into the cache (works for both parseable and non-parseable
/// languages ~ the stamp doesn't satisfy `cache.lookup` for symbols, so a
/// later outline/find/symbol call still re-parses and upgrades the entry).
/// Binary / oversized / unreadable files yield `None` and are NOT cached
/// (so a future content-change becomes detectable on the next call).
fn cache_line_count_for(
    cache: &mut ParseCache,
    resolved: &ResolvedPath,
    metadata: &Metadata,
    language: Language,
) -> Option<u32> {
    if let Some(line_count) = cache.lookup_line_count(&resolved.relative_path, metadata, language) {
        return Some(line_count);
    }
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return None;
    }
    let bytes = std::fs::read(&resolved.full_path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    let line_count = count_newlines(&bytes);
    cache.insert_lang_only(
        resolved.relative_path.clone(),
        metadata,
        language,
        line_count,
    );
    Some(line_count)
}

fn count_newlines(bytes: &[u8]) -> u32 {
    let n = bytes.iter().filter(|b| **b == b'\n').count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn matches_kinds(filter: &[String], kind: &str) -> bool {
    filter.is_empty() || filter.iter().any(|k| k.eq_ignore_ascii_case(kind))
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

fn build_exclude_set(excludes: &[String]) -> AppResult<Option<GlobSet>> {
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

fn apply_excludes(files: Vec<ResolvedPath>, excludes: Option<&GlobSet>) -> Vec<ResolvedPath> {
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

fn common_prefix(paths: &[String]) -> String {
    if paths.is_empty() {
        return String::new();
    }

    let mut prefix = paths[0].as_str();

    for path in &paths[1..] {
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
                repo_root: self.root.clone(),
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
    fn search_stops_loading_files_when_global_limit_is_filled() {
        let repo = TempRepo::new("search-limit");
        fs::write(repo.root.join("first.txt"), "needle\n").unwrap();

        let response = search_resolved_files(
            vec![repo.resolved("first.txt"), repo.resolved("missing.txt")],
            "needle",
            1,
            false,
            &mut ParseCache::disabled(),
        )
        .unwrap();

        assert_eq!(response.results.get("first.txt").unwrap(), &vec!["@L1"]);
        assert_eq!(response.truncated, true);
    }

    #[test]
    fn search_reports_truncated_when_single_file_exceeds_limit() {
        let repo = TempRepo::new("search-file-limit");
        fs::write(repo.root.join("first.txt"), "needle\nneedle\n").unwrap();

        let response = search_resolved_files(
            vec![repo.resolved("first.txt")],
            "needle",
            1,
            false,
            &mut ParseCache::disabled(),
        )
        .unwrap();

        assert_eq!(response.results.get("first.txt").unwrap(), &vec!["@L1"]);
        assert_eq!(response.truncated, true);
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
}
