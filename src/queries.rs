#[cfg(test)]
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
        qualname: symbol.qualname.clone(),
        content,
        range: symbol.range.clone(),
    })
}

const SNIPPET_MAX_CHARS: usize = 100;
#[cfg(test)]
const SNIPPET_SEPARATOR: &str = " :: ";

#[cfg(test)]
pub fn search_file(
    parsed: &ParsedFile,
    source: &str,
    path: &str,
    query: &str,
    max_results: usize,
    snippet: bool,
    include_unscoped: bool,
) -> Vec<String> {
    if query.is_empty() {
        return Vec::new();
    }

    let terms: Vec<&str> = query
        .split(" OR ")
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();

    // Headroom over max_results so the unscoped-suppression rule below has a
    // chance to surface inside-symbol matches when the file's first hits
    // happen to be imports.
    let raw_cap = max_results.saturating_mul(2).max(64);

    struct RawMatch {
        scoped: bool,
        entry: String,
    }
    let mut raw: Vec<RawMatch> = Vec::new();
    let mut seen = HashSet::new();

    'terms: for term in &terms {
        let mut offset = 0;
        while offset < source.len() && raw.len() < raw_cap {
            let Some(found) = source[offset..].find(term) else {
                break;
            };

            let start_byte = offset + found;
            let scoped_sym = enclosing_symbol(parsed, start_byte);
            let dedup_key = if let Some(symbol) = scoped_sym {
                format!("{path}:{}:{}", symbol.qualname, symbol.range.start_byte)
            } else {
                let line = line_at_byte(source, start_byte);
                format!("{path}:line:{line}")
            };

            if seen.insert(dedup_key) {
                raw.push(RawMatch {
                    scoped: scoped_sym.is_some(),
                    entry: format_match(parsed, source, start_byte, snippet),
                });
            }

            offset = start_byte + term.len().max(1);
        }
        if raw.len() >= raw_cap {
            break 'terms;
        }
    }

    if !include_unscoped && raw.iter().any(|m| m.scoped) {
        raw.retain(|m| m.scoped);
    }

    raw.into_iter().take(max_results).map(|m| m.entry).collect()
}

#[cfg(test)]
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

#[cfg(test)]
fn line_at_byte(source: &str, byte_offset: usize) -> usize {
    let cap = byte_offset.min(source.len());
    source.as_bytes()[..cap]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

#[cfg(test)]
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

#[cfg(test)]
fn enclosing_symbol<'a>(parsed: &'a ParsedFile, byte_offset: usize) -> Option<&'a SymbolInfo> {
    parsed
        .symbols
        .iter()
        .filter(|symbol| {
            symbol.range.start_byte <= byte_offset && byte_offset < symbol.range.end_byte
        })
        .min_by_key(|symbol| symbol.range.end_byte - symbol.range.start_byte)
}

/// Symbols whose line range intersects `[lo, hi]` (1-indexed, inclusive on both ends).
///
/// Used by `hitagi diff` to attach the enclosing symbol to each hunk. The
/// `primary` is the innermost symbol that fully contains the target range; if
/// no symbol contains it (e.g. a hunk straddles two top-level functions),
/// fall back to the smallest range that merely overlaps. `overlapping` is the
/// full intersection set, sorted by `start_line` for stable output.
pub struct SymbolSpan<'a> {
    pub primary: Option<&'a SymbolInfo>,
    pub overlapping: Vec<&'a SymbolInfo>,
}

pub fn symbols_for_line_range<'a>(
    symbols: &'a [SymbolInfo],
    lo: usize,
    hi: usize,
) -> SymbolSpan<'a> {
    let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    let mut overlapping: Vec<&SymbolInfo> = symbols
        .iter()
        .filter(|s| s.range.start_line <= hi && s.range.end_line >= lo)
        .collect();

    // Prefer the smallest symbol that fully contains [lo, hi]; fall back to
    // the smallest one that merely overlaps (e.g. a hunk on the seam between
    // two functions).
    let primary = overlapping
        .iter()
        .filter(|s| s.range.start_line <= lo && s.range.end_line >= hi)
        .min_by_key(|s| s.range.end_line - s.range.start_line)
        .copied()
        .or_else(|| {
            overlapping
                .iter()
                .min_by_key(|s| s.range.end_line - s.range.start_line)
                .copied()
        });

    overlapping.sort_by(|a, b| {
        a.range
            .start_line
            .cmp(&b.range.start_line)
            .then_with(|| a.range.end_line.cmp(&b.range.end_line))
            .then_with(|| a.qualname.cmp(&b.qualname))
    });

    SymbolSpan {
        primary,
        overlapping,
    }
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
        format!("symbol is ambiguous: {query} matched: {}", shown.join(", "))
    } else {
        format!(
            "symbol is ambiguous: {query} matched: {} (+{extra} more)",
            shown.join(", ")
        )
    }
}

fn suggest_symbols(parsed: &ParsedFile, query: &str, max: usize) -> Vec<String> {
    let needle = query.to_lowercase();
    let substring_matches: Vec<String> = parsed
        .symbols
        .iter()
        .filter(|s| s.qualname.to_lowercase().contains(&needle))
        .take(max)
        .map(|s| s.qualname.clone())
        .collect();
    if !substring_matches.is_empty() {
        return substring_matches;
    }

    let mut scored: Vec<ScoredSuggestion> = parsed
        .symbols
        .iter()
        .filter_map(|symbol| score_symbol_suggestion(symbol, &needle))
        .collect();
    scored.sort_by(|a, b| {
        a.distance
            .cmp(&b.distance)
            .then_with(|| b.similarity.total_cmp(&a.similarity))
            .then_with(|| a.qualname.cmp(&b.qualname))
    });
    scored
        .into_iter()
        .take(max)
        .map(|suggestion| suggestion.qualname)
        .collect()
}

struct ScoredSuggestion {
    qualname: String,
    distance: usize,
    similarity: f64,
}

fn score_symbol_suggestion(symbol: &SymbolInfo, needle: &str) -> Option<ScoredSuggestion> {
    let qualname = symbol.qualname.to_lowercase();
    let leaf = qualname.rsplit('.').next().unwrap_or(&qualname);

    let qualname_score = typo_score(needle, &qualname);
    let leaf_score = typo_score(needle, leaf);
    let (distance, similarity) = if leaf_score.0 < qualname_score.0
        || (leaf_score.0 == qualname_score.0 && leaf_score.1 > qualname_score.1)
    {
        leaf_score
    } else {
        qualname_score
    };

    (distance <= 3 || similarity >= 0.6).then(|| ScoredSuggestion {
        qualname: symbol.qualname.clone(),
        distance,
        similarity,
    })
}

fn typo_score(a: &str, b: &str) -> (usize, f64) {
    let distance = levenshtein(a, b);
    let width = a.chars().count().max(b.chars().count()).max(1);
    let similarity = 1.0 - (distance as f64 / width as f64);
    (distance, similarity)
}

fn levenshtein(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }

    let b_chars: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b_chars.len()).collect();
    let mut current = vec![0; b_chars.len() + 1];

    for (i, a_char) in a.chars().enumerate() {
        current[0] = i + 1;
        for (j, b_char) in b_chars.iter().enumerate() {
            let substitution = usize::from(a_char != *b_char);
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }

    previous[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{
        lang::Language,
        models::{RangeInfo, SymbolInfo},
        parser::parse_source,
    };

    use super::{resolve_symbol, search_file, symbol_detail, symbols_for_line_range};

    fn sym(qualname: &str, start: usize, end: usize) -> SymbolInfo {
        SymbolInfo {
            kind: "function".to_string(),
            name: qualname.split('.').last().unwrap_or(qualname).to_string(),
            qualname: qualname.to_string(),
            range: RangeInfo {
                start_byte: 0,
                end_byte: 0,
                start_line: start,
                end_line: end,
            },
            parent: None,
        }
    }

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
        let parsed = parse_source(&Language::new("typescript"), &source).unwrap();
        let names: Vec<_> = parsed
            .symbols
            .iter()
            .map(|symbol| symbol.qualname.as_str())
            .collect();

        assert!(names.contains(&"AuthService"));
        assert!(names.contains(&"AuthService.handleAuth"));
        assert!(names.contains(&"AuthService.validateInput"));
    }

    #[test]
    fn finds_symbol_by_qualified_name() {
        let source = sample_source();
        let parsed = parse_source(&Language::new("typescript"), &source).unwrap();
        let symbol = symbol_detail(&parsed, &source, "AuthService.handleAuth", 1024).unwrap();

        assert_eq!(symbol.qualname, "AuthService.handleAuth");
        assert!(symbol.content.contains("TOKEN_DUP"));
    }

    #[test]
    fn finds_symbol_by_leaf_suffix_match() {
        let source = sample_source();
        let parsed = parse_source(&Language::new("typescript"), &source).unwrap();
        let symbol = symbol_detail(&parsed, &source, "handleAuth", 1024).unwrap();

        assert_eq!(symbol.qualname, "AuthService.handleAuth");
    }

    #[test]
    fn missing_symbol_suggests_near_misses() {
        let source = sample_source();
        let parsed = parse_source(&Language::new("typescript"), &source).unwrap();
        let error = resolve_symbol(&parsed, "Auth").unwrap_err();
        let msg = error.to_string();
        assert!(msg.contains("symbol not found: Auth"));
        assert!(msg.contains("AuthService"));
    }

    #[test]
    fn missing_symbol_suggests_typo_matches() {
        let source = sample_source();
        let parsed = parse_source(&Language::new("typescript"), &source).unwrap();
        let error = resolve_symbol(&parsed, "handelAuth").unwrap_err();
        let msg = error.to_string();
        assert!(msg.contains("symbol not found: handelAuth"));
        assert!(msg.contains("AuthService.handleAuth"));
    }

    #[test]
    fn deduplicates_search_results_by_scope() {
        let source = sample_source();
        let parsed = parse_source(&Language::new("typescript"), &source).unwrap();
        let results = search_file(
            &parsed,
            &source,
            "src/auth.ts",
            "TOKEN_DUP",
            10,
            false,
            false,
        );

        assert_eq!(results.len(), 1);
        assert!(results[0].contains("AuthService.handleAuth"));
        assert!(results[0].contains("@L"));
    }

    #[test]
    fn search_includes_snippet_when_requested() {
        let source = sample_source();
        let parsed = parse_source(&Language::new("typescript"), &source).unwrap();
        let results = search_file(
            &parsed,
            &source,
            "src/auth.ts",
            "TOKEN_DUP",
            10,
            true,
            false,
        );

        assert_eq!(results.len(), 1);
        assert!(results[0].contains(" :: "));
        assert!(results[0].contains("TOKEN_DUP"));
    }

    #[test]
    fn symbols_for_line_range_picks_innermost() {
        let symbols = vec![
            sym("Outer", 1, 100),
            sym("Outer.Inner", 10, 50),
            sym("Outer.Inner.Leaf", 20, 30),
            sym("Sibling", 60, 90),
        ];
        let span = symbols_for_line_range(&symbols, 22, 25);
        let primary = span.primary.expect("expected an enclosing symbol");
        assert_eq!(primary.qualname, "Outer.Inner.Leaf");
        let names: Vec<&str> = span
            .overlapping
            .iter()
            .map(|s| s.qualname.as_str())
            .collect();
        assert_eq!(names, vec!["Outer", "Outer.Inner", "Outer.Inner.Leaf"]);
    }

    #[test]
    fn symbols_for_line_range_collects_overlapping_when_multi_symbol_hunk() {
        let symbols = vec![sym("Foo", 1, 10), sym("Bar", 11, 20)];
        let span = symbols_for_line_range(&symbols, 8, 14);
        // Neither fully contains [8, 14], so primary falls back to the
        // smaller-range overlapping match (both have width 9, take first
        // by tie-break ~ start_line).
        let primary = span.primary.expect("primary should fall back to overlap");
        assert_eq!(primary.qualname, "Foo");
        let names: Vec<&str> = span
            .overlapping
            .iter()
            .map(|s| s.qualname.as_str())
            .collect();
        assert_eq!(names, vec!["Foo", "Bar"]);
    }

    #[test]
    fn symbols_for_line_range_no_overlap_returns_empty() {
        let symbols = vec![sym("Foo", 1, 10), sym("Bar", 50, 60)];
        let span = symbols_for_line_range(&symbols, 20, 30);
        assert!(span.primary.is_none());
        assert!(span.overlapping.is_empty());
    }

    #[test]
    fn symbols_for_line_range_handles_single_line_anchor() {
        let symbols = vec![sym("Outer", 1, 100), sym("Outer.Inner", 10, 20)];
        let span = symbols_for_line_range(&symbols, 15, 15);
        assert_eq!(span.primary.unwrap().qualname, "Outer.Inner");
    }
}
