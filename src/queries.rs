use std::collections::HashSet;

use crate::{
    error::{AppError, AppResult},
    models::{RangeInfo, SymbolDetail, SymbolInfo},
    parser::ParsedFile,
};

const SUGGESTION_LIMIT: usize = 5;
const AMBIGUOUS_DISPLAY_LIMIT: usize = 10;

pub fn find_symbol<'a>(parsed: &'a ParsedFile, qualname: &str) -> Option<&'a SymbolInfo> {
    parsed
        .symbols
        .iter()
        .find(|symbol| symbol.qualname == qualname)
}

pub fn resolve_symbol<'a>(parsed: &'a ParsedFile, query: &str) -> AppResult<&'a SymbolInfo> {
    if let Some(symbol) = find_symbol(parsed, query) {
        return Ok(symbol);
    }

    let suffix = format!(".{query}");
    let candidates: Vec<&SymbolInfo> = parsed
        .symbols
        .iter()
        .filter(|s| s.qualname.ends_with(&suffix))
        .collect();

    match candidates.len() {
        0 => Err(AppError::not_found(symbol_not_found_message(parsed, query))),
        1 => Ok(candidates[0]),
        _ => Err(AppError::bad_request(ambiguous_symbol_message(
            query,
            &candidates,
        ))),
    }
}

pub fn symbol_detail(
    parsed: &ParsedFile,
    source: &str,
    query: &str,
    max_bytes: usize,
) -> AppResult<SymbolDetail> {
    let symbol = resolve_symbol(parsed, query)?;
    let content = content_for_range(source, &symbol.range, max_bytes)?;

    Ok(SymbolDetail {
        kind: symbol.kind.clone(),
        name: symbol.name.clone(),
        qualname: symbol.qualname.clone(),
        content,
        range: symbol.range.clone(),
    })
}

const SNIPPET_MAX_CHARS: usize = 100;
const SNIPPET_SEPARATOR: &str = " :: ";

pub fn search_file(
    parsed: &ParsedFile,
    source: &str,
    path: &str,
    query: &str,
    max_results: usize,
    snippet: bool,
) -> Vec<String> {
    if query.is_empty() {
        return Vec::new();
    }

    let terms: Vec<&str> = query
        .split(" OR ")
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();

    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for term in &terms {
        let mut offset = 0;
        while offset < source.len() && results.len() < max_results {
            let Some(found) = source[offset..].find(term) else {
                break;
            };

            let start_byte = offset + found;
            let dedup_key = if let Some(symbol) = enclosing_symbol(parsed, start_byte) {
                format!("{path}:{}:{}", symbol.qualname, symbol.range.start_byte)
            } else {
                let line = line_at_byte(source, start_byte);
                format!("{path}:line:{line}")
            };

            if seen.insert(dedup_key) {
                results.push(format_match(parsed, source, start_byte, snippet));
            }

            offset = start_byte + term.len().max(1);
        }
    }

    results
}

pub fn search_file_plain(
    source: &str,
    query: &str,
    max_results: usize,
    snippet: bool,
) -> Vec<String> {
    if query.is_empty() {
        return Vec::new();
    }

    let terms: Vec<&str> = query
        .split(" OR ")
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();

    let mut results = Vec::new();
    let mut seen_lines = HashSet::new();

    for term in &terms {
        let mut offset = 0;
        while offset < source.len() && results.len() < max_results {
            let Some(found) = source[offset..].find(term) else {
                break;
            };

            let start_byte = offset + found;
            let line = line_at_byte(source, start_byte);

            if seen_lines.insert(line) {
                let mut entry = format!("@L{line}");
                if snippet {
                    entry.push_str(SNIPPET_SEPARATOR);
                    entry.push_str(&snippet_at_byte(source, start_byte));
                }
                results.push(entry);
            }

            offset = start_byte + term.len().max(1);
        }
    }

    results
}

fn format_match(parsed: &ParsedFile, source: &str, start_byte: usize, snippet: bool) -> String {
    let line = line_at_byte(source, start_byte);
    let mut base = if let Some(symbol) = enclosing_symbol(parsed, start_byte) {
        format!("{}({}) @L{line}", symbol.qualname, symbol.kind)
    } else {
        format!("@L{line}")
    };

    if snippet {
        base.push_str(SNIPPET_SEPARATOR);
        base.push_str(&snippet_at_byte(source, start_byte));
    }

    base
}

fn line_at_byte(source: &str, byte_offset: usize) -> usize {
    let cap = byte_offset.min(source.len());
    source.as_bytes()[..cap].iter().filter(|b| **b == b'\n').count() + 1
}

pub fn snippet_at_byte(source: &str, byte_offset: usize) -> String {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let offset = byte_offset.min(len);

    let line_start = bytes[..offset]
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let line_end = bytes[offset..]
        .iter()
        .position(|b| *b == b'\n')
        .map(|i| offset + i)
        .unwrap_or(len);

    let line = &source[line_start..line_end];
    let trimmed = line.trim();
    let char_count = trimmed.chars().count();
    if char_count <= SNIPPET_MAX_CHARS {
        trimmed.to_string()
    } else {
        let truncated: String = trimmed.chars().take(SNIPPET_MAX_CHARS).collect();
        format!("{truncated}…")
    }
}

pub fn snippet_for_symbol_signature(source: &str, start_byte: usize, end_byte: usize) -> String {
    let cap = end_byte.min(source.len());
    if start_byte >= cap {
        return String::new();
    }
    let slice = &source[start_byte..cap];
    let line = slice.lines().next().unwrap_or("");
    let trimmed = line.trim();
    let char_count = trimmed.chars().count();
    if char_count <= SNIPPET_MAX_CHARS {
        trimmed.to_string()
    } else {
        let truncated: String = trimmed.chars().take(SNIPPET_MAX_CHARS).collect();
        format!("{truncated}…")
    }
}

fn enclosing_symbol<'a>(parsed: &'a ParsedFile, byte_offset: usize) -> Option<&'a SymbolInfo> {
    parsed
        .symbols
        .iter()
        .filter(|symbol| {
            symbol.range.start_byte <= byte_offset && byte_offset < symbol.range.end_byte
        })
        .min_by_key(|symbol| symbol.range.end_byte - symbol.range.start_byte)
}

fn content_for_range(source: &str, range: &RangeInfo, max_bytes: usize) -> AppResult<String> {
    if range.end_byte > source.len() || range.start_byte > range.end_byte {
        return Err(AppError::internal("invalid byte range"));
    }

    let bytes = &source.as_bytes()[range.start_byte..range.end_byte];
    if bytes.len() > max_bytes {
        return Err(AppError::too_large(
            "content exceeds configured response limit",
        ));
    }

    std::str::from_utf8(bytes)
        .map(|text| text.to_string())
        .map_err(|_| AppError::InvalidUtf8("content is not valid UTF-8".to_string()))
}

fn symbol_not_found_message(parsed: &ParsedFile, query: &str) -> String {
    let suggestions = suggest_symbols(parsed, query, SUGGESTION_LIMIT);
    if suggestions.is_empty() {
        format!("symbol not found: {query}")
    } else {
        format!(
            "symbol not found: {query}. Did you mean: {}",
            suggestions.join(", ")
        )
    }
}

fn ambiguous_symbol_message(query: &str, candidates: &[&SymbolInfo]) -> String {
    let shown: Vec<String> = candidates
        .iter()
        .take(AMBIGUOUS_DISPLAY_LIMIT)
        .map(|s| s.qualname.clone())
        .collect();
    let extra = candidates.len().saturating_sub(shown.len());
    if extra == 0 {
        format!(
            "symbol is ambiguous: {query} matched: {}",
            shown.join(", ")
        )
    } else {
        format!(
            "symbol is ambiguous: {query} matched: {} (+{extra} more)",
            shown.join(", ")
        )
    }
}

fn suggest_symbols(parsed: &ParsedFile, query: &str, max: usize) -> Vec<String> {
    let needle = query.to_lowercase();
    parsed
        .symbols
        .iter()
        .filter(|s| s.qualname.to_lowercase().contains(&needle))
        .take(max)
        .map(|s| s.qualname.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{lang::Language, parser::parse_source};

    use super::{resolve_symbol, search_file, symbol_detail};

    fn sample_source() -> String {
        std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("sample_repo")
                .join("src")
                .join("auth.ts"),
        )
        .unwrap()
    }

    #[test]
    fn builds_outline_for_sample_typescript_file() {
        let source = sample_source();
        let parsed = parse_source(Language::TypeScript, &source).unwrap();
        let names: Vec<_> = parsed
            .symbols
            .iter()
            .map(|symbol| symbol.qualname.as_str())
            .collect();

        assert!(names.contains(&"AuthService"));
        assert!(names.contains(&"AuthService.handleAuth"));
        assert!(names.contains(&"AuthService.validateInput"));
        assert!(names.contains(&"helper"));
    }

    #[test]
    fn finds_symbol_by_qualified_name() {
        let source = sample_source();
        let parsed = parse_source(Language::TypeScript, &source).unwrap();
        let symbol = symbol_detail(&parsed, &source, "AuthService.handleAuth", 1024).unwrap();

        assert_eq!(symbol.name, "handleAuth");
        assert!(symbol.content.contains("TOKEN_DUP"));
    }

    #[test]
    fn finds_symbol_by_leaf_suffix_match() {
        let source = sample_source();
        let parsed = parse_source(Language::TypeScript, &source).unwrap();
        let symbol = symbol_detail(&parsed, &source, "handleAuth", 1024).unwrap();

        assert_eq!(symbol.qualname, "AuthService.handleAuth");
    }

    #[test]
    fn missing_symbol_suggests_near_misses() {
        let source = sample_source();
        let parsed = parse_source(Language::TypeScript, &source).unwrap();
        let error = resolve_symbol(&parsed, "Auth").unwrap_err();
        let msg = error.to_string();
        assert!(msg.contains("symbol not found: Auth"));
        assert!(msg.contains("AuthService"));
    }

    #[test]
    fn deduplicates_search_results_by_scope() {
        let source = sample_source();
        let parsed = parse_source(Language::TypeScript, &source).unwrap();
        let results = search_file(&parsed, &source, "src/auth.ts", "TOKEN_DUP", 10, false);

        assert_eq!(results.len(), 1);
        assert!(results[0].contains("AuthService.handleAuth"));
        assert!(results[0].contains("@L"));
    }

    #[test]
    fn search_includes_snippet_when_requested() {
        let source = sample_source();
        let parsed = parse_source(Language::TypeScript, &source).unwrap();
        let results = search_file(&parsed, &source, "src/auth.ts", "TOKEN_DUP", 10, true);

        assert_eq!(results.len(), 1);
        assert!(results[0].contains(" :: "));
        assert!(results[0].contains("TOKEN_DUP"));
    }
}
