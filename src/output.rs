use std::fmt::Write as _;
use std::io::IsTerminal;
use std::sync::OnceLock;

use crate::{
    error::AppResult,
    models::{
        AgentPromptResponse, CacheClearResponse, CachePathResponse, CacheStatusResponse,
        DiffFileResponse, DiffFileSummary, DiffHunk, DiffMultiFileResponse, DiffOverviewResponse,
        DiffPathsResponse, DiffSummaryFile, DiffSummaryGroup, DiffSummaryResponse, FilesGroup,
        FilesResponse, FindGroup, FindMatch, FindMatches, FindRelatedResponse, FindResponse,
        IndexBuildResponse, IndexCleanResponse, IndexStatusResponse, LangsResponse, LocFileResult,
        LocFilesResponse, LocSymbolResult, LocSymbolsResponse, NextInfoResponse,
        NextLayoutsResponse, NextRoutesResponse, NextServerActionsResponse, OutlineResponse,
        OutputSymbol, ReadFileResponse, ReadSummaryResponse, SearchHit, SearchResponse,
        SymbolResponse,
    },
};

pub fn print_outline(path: &str, value: &OutlineResponse) -> AppResult<()> {
    emit(render_outline(path, value))
}

pub fn print_symbol(path: &str, value: &SymbolResponse) -> AppResult<()> {
    emit(render_symbol(path, value))
}

pub fn print_search(value: &SearchResponse) -> AppResult<()> {
    emit(render_search(value))
}

pub fn print_find_related(value: &FindRelatedResponse) -> AppResult<()> {
    emit(render_find_related(value))
}

pub fn print_index_status(value: &IndexStatusResponse) -> AppResult<()> {
    emit(render_index_status(value))
}

pub fn print_index_build(value: &IndexBuildResponse) -> AppResult<()> {
    emit(render_index_build(value))
}

pub fn print_index_clean(value: &IndexCleanResponse) -> AppResult<()> {
    emit(render_index_clean(value))
}

pub fn print_read(path: &str, value: &ReadFileResponse) -> AppResult<()> {
    emit(render_read(path, value))
}

pub fn print_read_summary(path: &str, value: &ReadSummaryResponse) -> AppResult<()> {
    emit(render_read_summary(path, value))
}

pub fn print_find(query: &str, value: &FindResponse) -> AppResult<()> {
    emit(render_find(query, value))
}

pub fn print_files(value: &FilesResponse) -> AppResult<()> {
    emit(render_files(value))
}

pub fn print_loc_symbols(value: &LocSymbolsResponse) -> AppResult<()> {
    emit(render_loc_symbols(value))
}

pub fn print_loc_files(value: &LocFilesResponse) -> AppResult<()> {
    emit(render_loc_files(value))
}

pub fn print_langs(value: &LangsResponse) -> AppResult<()> {
    emit(render_langs(value))
}

pub fn print_next_info(value: &NextInfoResponse) -> AppResult<()> {
    emit(render_next_info(value))
}

pub fn print_next_routes(value: &NextRoutesResponse) -> AppResult<()> {
    emit(render_next_routes(value))
}

pub fn print_next_layouts(value: &NextLayoutsResponse) -> AppResult<()> {
    emit(render_next_layouts(value))
}

pub fn print_next_server_actions(value: &NextServerActionsResponse) -> AppResult<()> {
    emit(render_next_server_actions(value))
}

pub fn print_agent_prompt(value: &AgentPromptResponse) -> AppResult<()> {
    emit(render_agent_prompt(value))
}

pub fn print_cache_status(value: &CacheStatusResponse) -> AppResult<()> {
    emit(render_cache_status(value))
}

pub fn print_cache_path(value: &CachePathResponse) -> AppResult<()> {
    emit(render_cache_path(value))
}

pub fn print_cache_clear(value: &CacheClearResponse) -> AppResult<()> {
    emit(render_cache_clear(value))
}

pub fn print_diff_overview(value: &DiffOverviewResponse) -> AppResult<()> {
    emit(render_diff_overview(value))
}

pub fn print_diff_file(path: &str, value: &DiffFileResponse) -> AppResult<()> {
    emit(render_diff_file(path, value))
}

pub fn print_diff_files(value: &DiffMultiFileResponse) -> AppResult<()> {
    emit(render_diff_files(value))
}

pub fn print_diff_summary(value: &DiffSummaryResponse) -> AppResult<()> {
    emit(render_diff_summary(value))
}

pub fn print_diff_paths(value: &DiffPathsResponse) -> AppResult<()> {
    emit(render_diff_paths(value))
}

fn emit(text: String) -> AppResult<()> {
    print_text(&text);
    Ok(())
}

fn print_text(text: &str) {
    if text.ends_with('\n') {
        print!("{text}");
    } else {
        println!("{text}");
    }
}

fn render_outline(path: &str, value: &OutlineResponse) -> String {
    let mut out = String::new();
    let visible = value.symbols.len();
    let _ = writeln!(
        out,
        "outline {path}\n{} • {visible}/{} symbols",
        value.language, value.total_symbols
    );
    if value.auto_summarized {
        out.push_str("auto-summarized • depth 1\n");
    }
    if !value.kind_counts.is_empty() {
        out.push_str("file kinds");
        for (kind, count) in &value.kind_counts {
            let _ = write!(out, " • {kind} {count}");
        }
        out.push('\n');
    }
    // The structured response keeps available_kinds for callers; the text
    // rendering would just duplicate the `file kinds` line above (same info,
    // less detail), so we skip it.
    if let Some(note) = &value.note {
        let _ = writeln!(out, "note • {note}");
    }
    for symbol in &value.symbols {
        render_symbol_line(&mut out, symbol);
    }
    out
}

fn render_symbol_line(out: &mut String, symbol: &OutputSymbol) {
    let depth = symbol.qualname.matches('.').count();
    for _ in 0..depth {
        out.push_str("  ");
    }
    let _ = write!(
        out,
        "• L{}-{} {} {}",
        symbol.lines[0], symbol.lines[1], symbol.kind, symbol.qualname
    );
    if let Some(bytes) = symbol.bytes {
        let _ = write!(out, " • bytes {}-{}", bytes[0], bytes[1]);
    }
    out.push('\n');
}

fn render_symbol(path: &str, value: &SymbolResponse) -> String {
    let s = &value.symbol;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "symbol {path}\n{} • L{}-{} {} {}",
        value.language, s.lines[0], s.lines[1], s.kind, s.qualname
    );
    if let Some(bytes) = s.bytes {
        let _ = writeln!(out, "bytes • {}-{}", bytes[0], bytes[1]);
    }
    out.push('\n');
    out.push_str(&s.content);
    out
}

fn render_search(value: &SearchResponse) -> String {
    let mut out = String::new();
    let alpha_str = if value.mode == "bm25" {
        String::new()
    } else {
        format!(" α={:.2}", value.alpha)
    };
    let _ = writeln!(
        out,
        "search \"{}\" • {}{} • {} hits / {} chunks in {} files • {}ms",
        value.query,
        value.mode,
        alpha_str,
        value.results.len(),
        value.indexed_chunks,
        value.indexed_files,
        value.elapsed_ms
    );
    if !value.languages.is_empty() {
        let _ = writeln!(out, "language filter • {}", value.languages.join(", "));
    }
    if !value.paths.is_empty() {
        let _ = writeln!(out, "scope • {}", value.paths.join(", "));
    }
    for hit in &value.results {
        render_search_hit(&mut out, hit);
    }
    for warning in &value.warnings {
        let _ = writeln!(out, "warning • {warning}");
    }
    out
}

fn render_search_hit(out: &mut String, hit: &SearchHit) {
    let lang = hit.language.as_deref().unwrap_or("plaintext");
    let _ = write!(
        out,
        "{}:{}-{}\t{:.4}\t{}\t{lang}",
        hit.path, hit.lines[0], hit.lines[1], hit.score, hit.source
    );
    if let Some(snippet) = &hit.snippet {
        let _ = write!(out, " :: {snippet}");
    }
    out.push('\n');
}

fn render_find_related(value: &FindRelatedResponse) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "find-related {}:{} • {} hits / {} chunks in {} files • {}ms",
        value.path,
        value.line,
        value.results.len(),
        value.indexed_chunks,
        value.indexed_files,
        value.elapsed_ms
    );
    let _ = writeln!(
        out,
        "source • {}:{}-{}",
        value.source_chunk.path, value.source_chunk.lines[0], value.source_chunk.lines[1]
    );
    for hit in &value.results {
        render_search_hit(&mut out, hit);
    }
    for warning in &value.warnings {
        let _ = writeln!(out, "warning • {warning}");
    }
    out
}

fn render_index_status(value: &IndexStatusResponse) -> String {
    let mut out = String::new();
    let sparse = if value.sparse_present {
        "present"
    } else {
        "missing"
    };
    let dense = if value.dense_present {
        "present"
    } else {
        "missing"
    };
    let _ = writeln!(
        out,
        "index status\nsparse {sparse} • dense {dense} • {} chunks in {} files",
        value.indexed_chunks, value.indexed_files
    );
    if let Some(file) = &value.cache_file {
        let _ = writeln!(out, "file • {file}");
    }
    if let Some(model_id) = &value.model_id {
        let _ = write!(out, "model • {model_id}");
        if let Some(kind) = &value.encoder_kind {
            let _ = write!(out, " ({kind})");
        }
        if let Some(dim) = value.dim {
            let _ = write!(out, " • dim {dim}");
        }
        out.push('\n');
    }
    if let Some(fp) = &value.model_fingerprint {
        let _ = writeln!(out, "fingerprint • {fp}");
    }
    if let Some(secs) = value.sparse_built_at_unix_secs {
        let _ = writeln!(out, "sparse built • {secs}");
    }
    if let Some(secs) = value.dense_built_at_unix_secs {
        let _ = writeln!(out, "dense built • {secs}");
    }
    if !value.languages.is_empty() {
        out.push_str("languages\n");
        for (lang, count) in &value.languages {
            let _ = writeln!(out, "  • {lang:<12} {count} chunks");
        }
    }
    out
}

fn render_index_build(value: &IndexBuildResponse) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "index build • {} • {} chunks in {} files • {}ms",
        value.mode, value.indexed_chunks, value.indexed_files, value.elapsed_ms
    );
    if !value.languages.is_empty() {
        out.push_str("languages\n");
        for (lang, count) in &value.languages {
            let _ = writeln!(out, "  • {lang:<12} {count} chunks");
        }
    }
    for warning in &value.warnings {
        let _ = writeln!(out, "warning • {warning}");
    }
    out
}

fn render_index_clean(value: &IndexCleanResponse) -> String {
    let mut out = String::new();
    let state = if value.cleared {
        "cleared"
    } else {
        "already empty"
    };
    let _ = writeln!(out, "index clean • {state}");
    if let Some(file) = &value.cache_file {
        let _ = writeln!(out, "file • {file}");
    }
    out
}

fn render_read(path: &str, value: &ReadFileResponse) -> String {
    let mut out = String::new();
    let _ = write!(out, "read {path}\n{}", value.language);
    if let Some(lines) = value.lines {
        let _ = write!(out, " • L{}-{}", lines[0], lines[1]);
        if let Some(total) = value.total_lines {
            let _ = write!(out, " of {total}");
        }
    }
    out.push_str("\n\n");
    out.push_str(&value.content);
    out
}

fn render_read_summary(path: &str, value: &ReadSummaryResponse) -> String {
    let mut out = String::new();
    let parseable = if value.parseable {
        "parseable"
    } else {
        "plain"
    };
    let _ = writeln!(
        out,
        "read summary {path}\n{} • {} lines • {} bytes • {parseable}",
        value.language, value.lines, value.bytes
    );
    let _ = writeln!(
        out,
        "line stats • code {} • blank {} • comment {}",
        value.code, value.blank, value.comment
    );
    if value.parseable {
        let _ = writeln!(
            out,
            "symbols • {}/{}",
            value.symbols.len(),
            value.total_symbols
        );
    }
    if !value.kind_counts.is_empty() {
        out.push_str("file kinds");
        for (kind, count) in &value.kind_counts {
            let _ = write!(out, " • {kind} {count}");
        }
        out.push('\n');
    }
    if let Some(note) = &value.note {
        let _ = writeln!(out, "note • {note}");
    }
    for symbol in &value.symbols {
        render_symbol_line(&mut out, symbol);
    }
    out
}

fn render_find(query: &str, value: &FindResponse) -> String {
    let mut out = String::new();
    let count = find_response_count(value);
    let _ = writeln!(
        out,
        "find \"{query}\"\n{count} matches • {} files searched",
        value.searched_files
    );
    if value.truncated {
        out.push_str("truncated • true\n");
    }
    if !value.unsampled_dirs.is_empty() {
        let _ = writeln!(out, "unsampled • {}", value.unsampled_dirs.join(", "));
    }
    // The structured response keeps available_kinds for callers, but text
    // suppresses it: per-match `kind` already conveys what kinds were searched,
    // and the empty-result case is obvious from "0 matches".
    if let Some(note) = &value.note {
        let _ = writeln!(out, "note • {note}");
    }
    render_find_bucket(&mut out, &value.prefix, &value.matches, &value.more_in_file);
    for group in &value.groups {
        render_find_group(&mut out, group);
    }
    out
}

fn render_find_group(out: &mut String, group: &FindGroup) {
    render_find_bucket(out, &group.prefix, &group.matches, &group.more_in_file);
}

fn render_find_bucket(
    out: &mut String,
    prefix: &str,
    matches: &FindMatches,
    more_in_file: &std::collections::BTreeMap<String, usize>,
) {
    if find_matches_is_empty(matches) && more_in_file.is_empty() {
        return;
    }
    if prefix.is_empty() {
        render_find_matches(out, prefix, matches);
        render_more_in_file(out, prefix, more_in_file);
        return;
    }

    let _ = writeln!(out, "{prefix}");
    render_find_matches(out, "", matches);
    render_more_in_file(out, "", more_in_file);
}

fn render_find_matches(out: &mut String, prefix: &str, matches: &FindMatches) {
    match matches {
        FindMatches::Full(matches) => {
            for m in matches {
                render_find_match(out, prefix, m);
            }
        }
        FindMatches::Terse(matches) => {
            for m in matches {
                let _ = writeln!(out, "• {prefix}{m}");
            }
        }
    }
}

fn find_matches_is_empty(matches: &FindMatches) -> bool {
    match matches {
        FindMatches::Full(matches) => matches.is_empty(),
        FindMatches::Terse(matches) => matches.is_empty(),
    }
}

fn render_find_match(out: &mut String, prefix: &str, m: &FindMatch) {
    let _ = write!(
        out,
        "• {prefix}{}:L{}-{} {} {}",
        m.path, m.lines[0], m.lines[1], m.kind, m.qualname
    );
    if let Some(bytes) = m.bytes {
        let _ = write!(out, " • bytes {}-{}", bytes[0], bytes[1]);
    }
    if let Some(snippet) = &m.snippet {
        let _ = write!(out, " :: {snippet}");
    }
    out.push('\n');
}

fn render_more_in_file(
    out: &mut String,
    prefix: &str,
    more_in_file: &std::collections::BTreeMap<String, usize>,
) {
    for (path, count) in more_in_file {
        let _ = writeln!(out, "… {count} more in {prefix}{path}");
    }
}

fn render_loc_symbols(value: &LocSymbolsResponse) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "loc symbols\n{} shown / {} matches • {} files scanned • sort {}",
        value.results.len(),
        value.total_matches,
        value.scanned_files,
        value.sort
    );
    render_loc_filters(
        &mut out,
        value.min_lines,
        value.max_lines,
        Some(&value.kinds),
        Some(&value.languages),
        Some(&value.paths),
    );
    if value.truncated {
        out.push_str("truncated • true\n");
    }
    for result in &value.results {
        render_loc_symbol_result(&mut out, result);
    }
    out
}

fn render_loc_symbol_result(out: &mut String, result: &LocSymbolResult) {
    let _ = write!(
        out,
        "• {}:L{}-{} code {} {} {}",
        result.path, result.lines[0], result.lines[1], result.code, result.kind, result.qualname
    );
    if let Some(bytes) = result.bytes {
        let _ = write!(out, " • bytes {}-{}", bytes[0], bytes[1]);
    }
    if let Some(snippet) = &result.snippet {
        let _ = write!(out, " :: {snippet}");
    }
    out.push('\n');
}

fn render_loc_files(value: &LocFilesResponse) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "loc files\n{} shown / {} matches • {} files scanned • sort {}",
        value.results.len(),
        value.total_matches,
        value.scanned_files,
        value.sort
    );
    render_loc_filters(
        &mut out,
        value.min_lines,
        value.max_lines,
        None,
        Some(&value.languages),
        Some(&value.globs),
    );
    if value.truncated {
        out.push_str("truncated • true\n");
    }
    for result in &value.results {
        render_loc_file_result(&mut out, result);
    }
    out
}

fn render_loc_file_result(out: &mut String, result: &LocFileResult) {
    let _ = writeln!(
        out,
        "• {} code {} / {} lines • blank {} • comment {} • {}",
        result.path, result.code, result.lines, result.blank, result.comment, result.language
    );
}

fn render_loc_filters(
    out: &mut String,
    min_lines: Option<usize>,
    max_lines: Option<usize>,
    kinds: Option<&[String]>,
    languages: Option<&[String]>,
    scopes: Option<&[String]>,
) {
    match (min_lines, max_lines) {
        (Some(min), Some(max)) => {
            let _ = writeln!(out, "line filter • {min}-{max}");
        }
        (Some(min), None) => {
            let _ = writeln!(out, "line filter • >= {min}");
        }
        (None, Some(max)) => {
            let _ = writeln!(out, "line filter • <= {max}");
        }
        (None, None) => {}
    }
    if let Some(kinds) = kinds.filter(|kinds| !kinds.is_empty()) {
        let _ = writeln!(out, "kind filter • {}", kinds.join(", "));
    }
    if let Some(languages) = languages.filter(|languages| !languages.is_empty()) {
        let _ = writeln!(out, "language filter • {}", languages.join(", "));
    }
    if let Some(scopes) = scopes.filter(|scopes| !scopes.is_empty()) {
        let _ = writeln!(out, "scope • {}", scopes.join(", "));
    }
}

fn render_files(value: &FilesResponse) -> String {
    let mut out = String::new();
    if value.truncated {
        let _ = writeln!(out, "files\n{} files shown", value.files.len());
    } else {
        let _ = writeln!(out, "files\n{} files", value.files.len());
    }
    if value.truncated {
        out.push_str("truncated • true\n");
    }
    if let Some(note) = &value.note {
        let _ = writeln!(out, "note • {note}");
    }
    if value.truncated && !value.groups.is_empty() {
        for group in &value.groups {
            render_files_group(&mut out, group);
        }
        return out;
    }
    for file in &value.files {
        let _ = writeln!(out, "• {file}");
    }
    out
}

fn render_files_group(out: &mut String, group: &FilesGroup) {
    match (&group.pattern, &group.root) {
        (Some(pattern), _) => {
            let _ = writeln!(
                out,
                "pattern {pattern} • {} total • {} shown",
                group.total, group.shown
            );
        }
        (_, Some(root)) => {
            let _ = writeln!(
                out,
                "root {root} • {} total • {} shown",
                group.total, group.shown
            );
        }
        _ => {
            let _ = writeln!(out, "group • {} total • {} shown", group.total, group.shown);
        }
    }
    if !group.first.is_empty() {
        let _ = writeln!(out, "  first • {}", group.first.join(", "));
    }
    if !group.last.is_empty() {
        let _ = writeln!(out, "  last • {}", group.last.join(", "));
    }
}

fn render_langs(value: &LangsResponse) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "languages\n{} languages", value.languages.len());
    for lang in &value.languages {
        let parseable = if lang.parseable { "parseable" } else { "plain" };
        let _ = writeln!(
            out,
            "• {:<12} {:>5} files {:>7} lines {:>7} code {:>6} comm {:>6} blank • {parseable}",
            lang.language, lang.files, lang.lines, lang.code, lang.comment, lang.blank
        );
    }
    out
}

fn render_next_info(value: &NextInfoResponse) -> String {
    let mut out = String::new();
    let header = match (value.detected, &value.router) {
        (true, Some(r)) => format!("next • detected • {} router", r),
        (true, None) => "next • detected • no router dirs".to_string(),
        (false, _) => "next • not detected".to_string(),
    };
    let layout = if value.src_layout { " • src/" } else { "" };
    let _ = writeln!(out, "{header}{layout}");
    if let Some(version) = &value.version {
        let _ = writeln!(out, "version {version}");
    }
    let _ = writeln!(out, "root {}", value.root);
    out
}

fn render_next_routes(value: &NextRoutesResponse) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "routes • {} • root {}", value.routes.len(), value.root);
    for route in &value.routes {
        let methods = route
            .methods
            .as_ref()
            .filter(|m| !m.is_empty())
            .map(|m| format!(" • {}", m.join(" ")))
            .unwrap_or_default();
        let advanced = if route.advanced { " (advanced)" } else { "" };
        let _ = writeln!(
            out,
            "• {:<32} {:<4} {:<5} {}{}{}",
            route.pattern, route.kind, route.router, route.file, methods, advanced
        );
    }
    out
}

fn render_next_layouts(value: &NextLayoutsResponse) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "layouts • {} • root {}",
        value.layouts.len(),
        value.root
    );
    for layout in &value.layouts {
        let _ = writeln!(
            out,
            "• {:<13} {:<32} {}",
            layout.kind, layout.scope, layout.file
        );
    }
    out
}

fn render_next_server_actions(value: &NextServerActionsResponse) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "server-actions • {} • root {}",
        value.actions.len(),
        value.root
    );
    for action in &value.actions {
        let _ = writeln!(
            out,
            "• {:<8} {:<24} {}",
            action.scope, action.name, action.file
        );
    }
    out
}

fn render_agent_prompt(value: &AgentPromptResponse) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{} {}", value.action, value.agent);
    let _ = writeln!(out, "{} • changed {}", value.status, value.changed);
    for path in &value.paths {
        let _ = writeln!(out, "path • {path}");
    }
    out
}

fn render_cache_status(value: &CacheStatusResponse) -> String {
    let mut out = String::new();
    let enabled = if value.enabled { "enabled" } else { "disabled" };
    let exists = if value.exists { "exists" } else { "missing" };
    let _ = writeln!(
        out,
        "cache status\n{enabled} • {exists} • {} entries • {} bytes",
        value.entry_count, value.size_bytes
    );
    if value.disabled_via_env {
        out.push_str("disabled via HITAGI_NO_CACHE\n");
    }
    let _ = writeln!(out, "version • current {}", value.current_version);
    if let Some(stored) = &value.stored_version {
        let _ = writeln!(out, "stored version • {stored}");
    }
    if let Some(dir) = &value.cache_dir {
        let _ = writeln!(out, "dir • {dir}");
    }
    if let Some(file) = &value.cache_file {
        let _ = writeln!(out, "file • {file}");
    }
    if let Some(root) = &value.stored_repo_root {
        let _ = writeln!(out, "repo • {root}");
    }
    if let Some(modified) = value.modified_unix_secs {
        let _ = writeln!(out, "modified • {modified}");
    }
    let _ = writeln!(
        out,
        "matches • version {} • repo {}",
        value.version_match, value.repo_root_match
    );
    if !value.languages.is_empty() {
        out.push_str("languages\n");
        for lang in &value.languages {
            let _ = writeln!(out, "  • {:<12} {} files", lang.language, lang.files);
        }
    }
    out
}

fn render_cache_path(value: &CachePathResponse) -> String {
    match &value.path {
        Some(path) => format!("cache path\n{path}"),
        None => "cache path\nunavailable".to_string(),
    }
}

fn render_cache_clear(value: &CacheClearResponse) -> String {
    let mut out = String::new();
    let state = if value.cleared {
        "cleared"
    } else {
        "already clean"
    };
    let _ = writeln!(out, "cache clear {}\n{state} • {}", value.scope, value.path);
    if let Some(count) = value.repos_removed {
        let _ = writeln!(out, "repos removed • {count}");
    }
    out
}

fn use_color() -> bool {
    static USE: OnceLock<bool> = OnceLock::new();
    *USE.get_or_init(|| {
        std::env::var_os("NO_COLOR").is_none()
            && !matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
            && std::io::stdout().is_terminal()
    })
}

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_CYAN: &str = "\x1b[36m";

fn paint(code: &str, body: &str) -> String {
    if use_color() {
        format!("{code}{body}{ANSI_RESET}")
    } else {
        body.to_string()
    }
}

fn fmt_added(n: usize) -> String {
    paint(ANSI_GREEN, &format!("+{n}"))
}

fn fmt_removed(n: usize) -> String {
    paint(ANSI_RED, &format!("-{n}"))
}

fn fmt_status(code: &str) -> String {
    let ansi = match code {
        "M" => ANSI_YELLOW,
        "A" => ANSI_GREEN,
        "D" => ANSI_RED,
        "R" | "C" => ANSI_CYAN,
        _ => ANSI_DIM,
    };
    paint(ansi, code)
}

fn fmt_section(label: &str) -> String {
    let bar = paint(ANSI_DIM, "▌");
    let body = paint(ANSI_BOLD, label);
    format!("{bar} {body}")
}

fn render_diff_overview(value: &DiffOverviewResponse) -> String {
    let mut out = String::new();
    let mut header = "diff".to_string();
    if let Some(against) = &value.against {
        let _ = write!(header, " against {against}");
    }
    if !value.scope.is_empty() {
        let _ = write!(header, " {}", value.scope);
    }
    let _ = writeln!(out, "{header}");
    if value.clean {
        out.push_str("clean\n");
    } else {
        let _ = writeln!(out, "{} files", value.files.len());
    }
    if let Some(note) = &value.note {
        let _ = writeln!(out, "note • {note}");
    }

    for (group_index, group) in diff_overview_folder_groups(value).iter().enumerate() {
        if group_index > 0 {
            out.push('\n');
        }
        render_diff_overview_folder(&mut out, value, group);
    }
    out
}

struct DiffOverviewFolderGroup<'a> {
    folder: String,
    files: Vec<&'a DiffFileSummary>,
}

fn diff_overview_folder_groups(value: &DiffOverviewResponse) -> Vec<DiffOverviewFolderGroup<'_>> {
    let mut groups: Vec<DiffOverviewFolderGroup<'_>> = Vec::new();
    for file in &value.files {
        let path = format!("{}{}", value.prefix, file.path);
        let folder = diff_overview_folder(&path);
        if let Some(group) = groups.last_mut().filter(|group| group.folder == folder) {
            group.files.push(file);
        } else {
            groups.push(DiffOverviewFolderGroup {
                folder,
                files: vec![file],
            });
        }
    }
    groups
}

fn diff_overview_folder(path: &str) -> String {
    match path.find('/') {
        Some(index) => path[..=index].to_string(),
        None => "./".to_string(),
    }
}

fn render_diff_overview_folder(
    out: &mut String,
    value: &DiffOverviewResponse,
    group: &DiffOverviewFolderGroup<'_>,
) {
    let added: usize = group.files.iter().filter_map(|file| file.added).sum();
    let removed: usize = group.files.iter().filter_map(|file| file.removed).sum();
    let has_line_counts = group
        .files
        .iter()
        .any(|file| file.added.is_some() && file.removed.is_some());
    let file_label = if group.files.len() == 1 {
        "file"
    } else {
        "files"
    };
    let _ = write!(
        out,
        "{} {} • {} {}",
        paint(ANSI_DIM, "▾"),
        paint(ANSI_BOLD, &group.folder),
        group.files.len(),
        file_label,
    );
    if has_line_counts {
        let _ = write!(out, " • {} {}", fmt_added(added), fmt_removed(removed));
    }
    out.push('\n');

    for (index, file) in group.files.iter().enumerate() {
        let branch = if index + 1 == group.files.len() {
            "└─"
        } else {
            "├─"
        };
        render_diff_overview_file(out, value, file, branch);
    }
}

fn render_diff_overview_file(
    out: &mut String,
    value: &DiffOverviewResponse,
    file: &DiffFileSummary,
    branch: &str,
) {
    let path = format!("{}{}", value.prefix, file.path);
    let _ = write!(
        out,
        "  {} {} {path}",
        paint(ANSI_DIM, branch),
        fmt_status(&file.status)
    );
    if let (Some(added), Some(removed)) = (file.added, file.removed) {
        let _ = write!(out, " {} {}", fmt_added(added), fmt_removed(removed));
    }
    if let Some(old_path) = &file.old_path {
        if file.old_path_needs_prefix {
            let _ = write!(out, " ← {}{}", value.prefix, old_path);
        } else {
            let _ = write!(out, " ← {old_path}");
        }
    }
    if file.staged {
        out.push_str(" • staged");
    }
    if file.unstaged {
        out.push_str(" • unstaged");
    }
    if file.binary {
        out.push_str(" • binary");
    }
    if let Some(note) = &file.note {
        let _ = write!(out, " • {note}");
    }
    out.push('\n');
}

fn render_diff_file(path: &str, value: &DiffFileResponse) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "diff {path}\n{} {}",
        fmt_status(&value.status),
        value.path
    );
    if let (Some(added), Some(removed)) = (value.added, value.removed) {
        let _ = write!(out, " {} {}", fmt_added(added), fmt_removed(removed));
    }
    if let Some(language) = &value.language {
        let _ = write!(out, " • {language}");
    }
    if let Some(old_path) = &value.old_path {
        let _ = write!(out, " ← {old_path}");
    }
    if value.binary {
        out.push_str(" • binary");
    }
    out.push('\n');
    if let Some(note) = &value.note {
        let _ = writeln!(out, "note • {note}");
    }
    if let Some(raw) = &value.raw {
        out.push('\n');
        out.push_str(raw);
        return out;
    }
    if let Some(hunks) = &value.hunks {
        for hunk in hunks {
            render_diff_hunk(&mut out, hunk);
        }
    }
    out
}

fn render_diff_files(value: &DiffMultiFileResponse) -> String {
    let mut out = String::new();
    for (index, file) in value.files.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&render_diff_file(&file.path, file));
    }
    out
}

fn render_diff_summary(value: &DiffSummaryResponse) -> String {
    let mut out = String::new();
    let mut header = if value.commit {
        "diff commit".to_string()
    } else {
        "diff summary".to_string()
    };
    if let Some(against) = &value.against {
        let _ = write!(header, " against {against}");
    }
    if !value.scope.is_empty() {
        let _ = write!(header, " {}", value.scope);
    }
    let _ = writeln!(out, "{header}");
    if value.clean {
        out.push_str("clean\n");
    } else {
        let _ = writeln!(out, "{} files", value.files.len());
    }
    if let Some(note) = &value.note {
        let _ = writeln!(out, "note • {note}");
    }
    if !value.groups.is_empty() {
        for group in &value.groups {
            render_diff_summary_group(&mut out, group);
        }
    } else if value.commit && value.scope.is_empty() {
        render_diff_summary_state_groups(&mut out, &value.files);
    } else {
        for file in &value.files {
            render_diff_summary_file(&mut out, file);
        }
    }
    out
}

fn render_diff_summary_group(out: &mut String, group: &DiffSummaryGroup) {
    let _ = writeln!(
        out,
        "{} • {} files • {} {}",
        group.path,
        group.file_count,
        fmt_added(group.added),
        fmt_removed(group.removed),
    );
    for file in &group.files {
        render_diff_summary_file(out, file);
    }
}

fn render_diff_summary_state_groups(out: &mut String, files: &[DiffSummaryFile]) {
    let mut first = true;
    for (label, predicate) in [
        ("staged+unstaged", 0u8),
        ("staged", 1),
        ("unstaged", 2),
        ("untracked", 3),
        ("other", 4),
    ] {
        let bucket: Vec<_> = files
            .iter()
            .filter(|file| match predicate {
                0 => file.staged && file.unstaged,
                1 => file.staged && !file.unstaged,
                2 => file.unstaged && !file.staged,
                3 => file.status == "?",
                _ => !file.staged && !file.unstaged && file.status != "?",
            })
            .collect();
        if bucket.is_empty() {
            continue;
        }
        if !first {
            out.push('\n');
        }
        first = false;
        let _ = writeln!(out, "{}", fmt_section(label));
        for file in bucket {
            render_diff_summary_file(out, file);
        }
    }
}

fn render_diff_summary_file(out: &mut String, file: &DiffSummaryFile) {
    let _ = write!(out, "{} {}", fmt_status(&file.status), file.path);
    if let (Some(added), Some(removed)) = (file.added, file.removed) {
        let _ = write!(out, " {} {}", fmt_added(added), fmt_removed(removed));
    }
    if let Some(old_path) = &file.old_path {
        let _ = write!(out, " ← {old_path}");
    }
    if let Some(language) = &file.language {
        let _ = write!(out, " • {language}");
    }
    if !file.symbols.is_empty() {
        let _ = write!(out, " • {}", file.symbols.join(", "));
        if file.more_symbols > 0 {
            let _ = write!(out, ", +{} more", file.more_symbols);
        }
    }
    if file.staged {
        out.push_str(" • staged");
    }
    if file.unstaged {
        out.push_str(" • unstaged");
    }
    if file.binary {
        out.push_str(" • binary");
    }
    if let Some(note) = &file.note {
        let _ = write!(out, " • {note}");
    }
    out.push('\n');
}

fn render_diff_paths(value: &DiffPathsResponse) -> String {
    let mut out = String::new();
    for path in &value.paths {
        let _ = writeln!(out, "{path}");
    }
    out
}

fn render_diff_hunk(out: &mut String, hunk: &DiffHunk) {
    let _ = write!(
        out,
        "@@ -{}-{} +{}-{} • {} {}",
        hunk.old_lines[0],
        hunk.old_lines[1],
        hunk.new_lines[0],
        hunk.new_lines[1],
        fmt_added(hunk.added),
        fmt_removed(hunk.removed),
    );
    if let Some(symbol) = &hunk.symbol {
        let _ = write!(out, " • {symbol}");
    }
    if let Some(kind) = &hunk.kind {
        let _ = write!(out, "({kind})");
    }
    if !hunk.spans.is_empty() {
        let _ = write!(out, " • spans {}", hunk.spans.join(", "));
    }
    if let Some(snippet) = &hunk.snippet {
        let _ = write!(out, " :: {snippet}");
    }
    out.push('\n');
    if let Some(body) = &hunk.body {
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
}

fn find_response_count(value: &FindResponse) -> usize {
    find_matches_count(&value.matches)
        + value
            .groups
            .iter()
            .map(|group| find_matches_count(&group.matches))
            .sum::<usize>()
}

fn find_matches_count(matches: &FindMatches) -> usize {
    match matches {
        FindMatches::Full(matches) => matches.len(),
        FindMatches::Terse(matches) => matches.len(),
    }
}
