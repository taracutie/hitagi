use std::collections::{BTreeMap, BTreeSet};
use std::fs::Metadata;
use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::{
    cache::ParseCache,
    error::{AppError, AppResult},
    lang::Language,
    models::{
        CacheClearResponse, CacheLangCount, CachePathResponse, CacheStatusResponse, FilesResponse,
        FindMatch, FindMatches, FindResponse, LangSummary, LangsResponse, OutlineResponse,
        OutputSymbol, OutputSymbolDetail, ReadFileResponse, SearchResponse, SymbolDetail,
        SymbolInfo, SymbolResponse,
    },
    parser::{parse_source, ParsedFile},
    queries::{search_file, search_file_plain, snippet_for_symbol_signature, symbol_detail},
    repo::{RepoRoot, ResolvedPath},
};

pub const MAX_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

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

    let all_kinds: BTreeSet<String> = symbols.iter().map(|s| s.kind.clone()).collect();

    let symbols: Vec<OutputSymbol> = symbols
        .into_iter()
        .filter(|s| within_depth(&s.qualname, opts.depth))
        .filter(|s| matches_kinds(&opts.kinds, &s.kind))
        .map(|s| to_output_symbol(s, opts.bytes))
        .collect();

    let available_kinds = if !opts.kinds.is_empty() && symbols.is_empty() {
        Some(all_kinds.into_iter().collect())
    } else {
        None
    };

    Ok(OutlineResponse {
        language: language.as_str().to_string(),
        symbols,
        available_kinds,
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
    let mut raw_results: Vec<(String, Vec<String>)> = Vec::new();
    let mut total = 0usize;
    let mut truncated = false;

    for resolved in files {
        if total >= limit {
            truncated = true;
            break;
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

    Ok(SearchResponse {
        prefix,
        results,
        truncated,
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

    let mut matches: Vec<FindMatch> = Vec::new();
    let mut truncated = false;
    let mut searched_files = 0usize;
    let mut all_kinds: BTreeSet<String> = BTreeSet::new();

    'outer: for resolved in files {
        if matches.len() >= limit {
            truncated = true;
            break;
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
            cache.insert(
                resolved.relative_path.clone(),
                &stat.metadata,
                language,
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

    Ok(FindResponse {
        prefix,
        matches,
        truncated,
        searched_files,
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

        if first_chunk_has_nul(Path::new(&resolved.full_path)) {
            continue;
        }

        let (label, parseable) = match Language::detect(Path::new(&resolved.full_path)) {
            Ok(lang) => (lang.as_str().to_string(), lang.is_parseable()),
            Err(_) => ("plaintext".to_string(), false),
        };

        let lines = if metadata.len() > MAX_FILE_BYTES as u64 {
            0
        } else {
            std::fs::read(&resolved.full_path)
                .map(|bytes| bytes.iter().filter(|b| **b == b'\n').count())
                .unwrap_or(0)
        };

        let entry = counts.entry(label).or_insert((0, 0, parseable));
        entry.0 += 1;
        entry.1 += lines;
    }

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
    cache.insert(
        resolved.relative_path.clone(),
        metadata,
        language,
        parsed.symbols.clone(),
    );
    Ok(parsed.symbols)
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
