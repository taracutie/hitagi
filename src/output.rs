use std::fmt::Write as _;

use serde::Serialize;

use crate::{
    error::{AppError, AppResult},
    models::{
        AgentPromptResponse, CacheClearResponse, CachePathResponse, CacheStatusResponse,
        DiffFileResponse, DiffHunk, DiffOverviewResponse, FilesResponse, FindGroup, FindMatch,
        FindMatches, FindResponse, LangsResponse, OutlineResponse, OutputSymbol, ReadFileResponse,
        SearchResponse, SymbolResponse,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Text,
    Json,
}

pub fn print_outline(path: &str, value: &OutlineResponse, mode: OutputMode) -> AppResult<()> {
    emit(value, mode, || render_outline(path, value))
}

pub fn print_symbol(path: &str, value: &SymbolResponse, mode: OutputMode) -> AppResult<()> {
    emit(value, mode, || render_symbol(path, value))
}

pub fn print_search(query: &str, value: &SearchResponse, mode: OutputMode) -> AppResult<()> {
    emit(value, mode, || render_search(query, value))
}

pub fn print_read(path: &str, value: &ReadFileResponse, mode: OutputMode) -> AppResult<()> {
    emit(value, mode, || render_read(path, value))
}

pub fn print_find(query: &str, value: &FindResponse, mode: OutputMode) -> AppResult<()> {
    emit(value, mode, || render_find(query, value))
}

pub fn print_files(value: &FilesResponse, mode: OutputMode) -> AppResult<()> {
    emit(value, mode, || render_files(value))
}

pub fn print_langs(value: &LangsResponse, mode: OutputMode) -> AppResult<()> {
    emit(value, mode, || render_langs(value))
}

pub fn print_agent_prompt(value: &AgentPromptResponse, mode: OutputMode) -> AppResult<()> {
    emit(value, mode, || render_agent_prompt(value))
}

pub fn print_cache_status(value: &CacheStatusResponse, mode: OutputMode) -> AppResult<()> {
    emit(value, mode, || render_cache_status(value))
}

pub fn print_cache_path(value: &CachePathResponse, mode: OutputMode) -> AppResult<()> {
    emit(value, mode, || render_cache_path(value))
}

pub fn print_cache_clear(value: &CacheClearResponse, mode: OutputMode) -> AppResult<()> {
    emit(value, mode, || render_cache_clear(value))
}

pub fn print_diff_overview(value: &DiffOverviewResponse, mode: OutputMode) -> AppResult<()> {
    emit(value, mode, || render_diff_overview(value))
}

pub fn print_diff_file(path: &str, value: &DiffFileResponse, mode: OutputMode) -> AppResult<()> {
    emit(value, mode, || render_diff_file(path, value))
}

fn emit<T, F>(value: &T, mode: OutputMode, render_text: F) -> AppResult<()>
where
    T: Serialize,
    F: FnOnce() -> String,
{
    match mode {
        OutputMode::Json => print_json(value),
        OutputMode::Text => {
            print_text(&render_text());
            Ok(())
        }
    }
}

fn print_json<T: Serialize>(value: &T) -> AppResult<()> {
    let serialized = serde_json::to_string(value)
        .map_err(|error| AppError::internal(format!("failed to serialize response: {error}")))?;
    println!("{serialized}");
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
    // available_kinds JSON field is preserved for programmatic consumers; the
    // text rendering would just duplicate the `file kinds` line above (same
    // info, less detail), so we skip it.
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

fn render_search(query: &str, value: &SearchResponse) -> String {
    let mut out = String::new();
    let count = search_match_count(value);
    let _ = writeln!(out, "search \"{query}\"\n{count} matches");
    if value.truncated {
        out.push_str("truncated • true\n");
    }
    if !value.unsampled_dirs.is_empty() {
        let _ = writeln!(out, "unsampled • {}", value.unsampled_dirs.join(", "));
    }
    render_search_results(&mut out, &value.prefix, &value.results);
    for group in &value.groups {
        render_search_results(&mut out, &group.prefix, &group.results);
    }
    out
}

fn render_search_results(
    out: &mut String,
    prefix: &str,
    results: &std::collections::BTreeMap<String, Vec<String>>,
) {
    if results.is_empty() {
        return;
    }
    let render_prefix_once = !prefix.is_empty();
    if render_prefix_once {
        let _ = writeln!(out, "{prefix}");
    }
    for (path, matches) in results {
        let path = if render_prefix_once {
            path.to_string()
        } else {
            format!("{prefix}{path}")
        };
        render_search_file(out, &path, matches);
    }
}

fn render_search_file(out: &mut String, path: &str, matches: &[String]) {
    let _ = writeln!(out, "{path}");
    for entry in matches {
        let _ = writeln!(out, "  • {entry}");
    }
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
    // available_kinds is kept on the JSON shape (programmatic consumers test
    // it) but suppressed in text ~ the per-match `kind` already conveys what
    // kinds were searched, and the empty-result case is obvious from "0 matches".
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
    let path = format!("{prefix}{}", m.path);
    let _ = write!(
        out,
        "• {path}:L{}-{} {} {}",
        m.lines[0], m.lines[1], m.kind, m.qualname
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

fn render_files(value: &FilesResponse) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "files\n{} files", value.files.len());
    if value.truncated {
        out.push_str("truncated • true\n");
    }
    if let Some(note) = &value.note {
        let _ = writeln!(out, "note • {note}");
    }
    for file in &value.files {
        let _ = writeln!(out, "• {file}");
    }
    out
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
    for file in &value.files {
        let path = format!("{}{}", value.prefix, file.path);
        let _ = write!(out, "{} {path}", file.status);
        if let (Some(added), Some(removed)) = (file.added, file.removed) {
            let _ = write!(out, " +{added} -{removed}");
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
    out
}

fn render_diff_file(path: &str, value: &DiffFileResponse) -> String {
    let mut out = String::new();
    let _ = write!(out, "diff {path}\n{} {}", value.status, value.path);
    if let (Some(added), Some(removed)) = (value.added, value.removed) {
        let _ = write!(out, " +{added} -{removed}");
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

fn render_diff_hunk(out: &mut String, hunk: &DiffHunk) {
    let _ = write!(
        out,
        "@@ -{}-{} +{}-{} • +{} -{}",
        hunk.old_lines[0],
        hunk.old_lines[1],
        hunk.new_lines[0],
        hunk.new_lines[1],
        hunk.added,
        hunk.removed
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
    out.push('\n');
    if let Some(body) = &hunk.body {
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
}

fn search_match_count(value: &SearchResponse) -> usize {
    let flat = value.results.values().map(Vec::len).sum::<usize>();
    let grouped = value
        .groups
        .iter()
        .flat_map(|group| group.results.values())
        .map(Vec::len)
        .sum::<usize>();
    flat + grouped
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
