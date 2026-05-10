//! Query analysis, alpha auto-tuning, post-fusion boosts, top-k rerank.
//!
//! Query-ranking module keeps the alpha tuning, multi-source boosting,
//! and top-k reranking framework while removing benchmark-suite-specific
//! matchers to stay focused on general-purpose ranking behavior.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::BuildHasher;
use std::path::Path;

use once_cell::sync::Lazy;
use regex::Regex;

use super::chunk_store::ChunkStore;
use super::tokens::split_identifier;

static SYMBOL_QUERY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:[A-Za-z_][A-Za-z0-9_]*(?:(?:::|\\|->|\.)[A-Za-z_][A-Za-z0-9_]*)+|_[A-Za-z0-9_]*|[A-Za-z][A-Za-z0-9]*[A-Z_][A-Za-z0-9_]*|[A-Z][A-Za-z0-9]*)$")
        .unwrap()
});
static EMBEDDED_SYMBOL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"\b(?:[A-Z][a-z][a-zA-Z0-9]*[A-Z][a-zA-Z0-9]*|[a-z][a-zA-Z0-9]*[A-Z][a-zA-Z0-9]+|[A-Za-z_][A-Za-z0-9_]*_[A-Za-z0-9_]*[A-Za-z0-9])\b",
    )
    .unwrap()
});
static TEST_FILE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?:^|/)(?:test_[^/]*\.py|[^/]*_test\.py|[^/]*_test\.go|[^/]*Tests?\.java|[^/]*Test\.php|[^/]*_spec\.rb|[^/]*_test\.rb|[^/]*\.test\.[jt]sx?|[^/]*\.spec\.[jt]sx?|[^/]*Tests?\.kt|[^/]*Spec\.kt|[^/]*Tests?\.swift|[^/]*Spec\.swift|[^/]*Tests?\.cs|test_[^/]*\.cpp|[^/]*_test\.cpp|test_[^/]*\.c|[^/]*_test\.c|[^/]*Spec\.scala|[^/]*Suite\.scala|[^/]*Test\.scala|[^/]*_test\.dart|test_[^/]*\.dart|[^/]*_spec\.lua|[^/]*_test\.lua|test_[^/]*\.lua|test_helpers?[^/]*\.\w+)$",
    )
    .unwrap()
});
static TEST_DIR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:^|/)(?:tests?|__tests__|spec|testing)(?:/|$)").unwrap());
static COMPAT_DIR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:^|/)(?:compat|_compat|legacy)(?:/|$)").unwrap());
static EXAMPLES_DIR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:^|/)(?:_?examples?|docs?_src)(?:/|$)").unwrap());
static TYPE_DEFS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\.d\.ts$").unwrap());
static STOPWORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    "a an and are as at be by do does for from has have how if in is it not of on or the to was what when where which who why with"
        .split_whitespace()
        .collect()
});

const DEFINITION_KEYWORDS: &[&str] = &[
    "class",
    "module",
    "defmodule",
    "def",
    "interface",
    "struct",
    "enum",
    "trait",
    "type",
    "func",
    "function",
    "object",
    "abstract class",
    "data class",
    "fn",
    "fun",
    "package",
    "namespace",
    "protocol",
    "record",
    "typedef",
];
const SQL_DEFINITION_KEYWORDS: &[&str] = &[
    "CREATE TABLE",
    "CREATE VIEW",
    "CREATE PROCEDURE",
    "CREATE FUNCTION",
];

pub fn resolve_alpha(query: &str, alpha: Option<f32>) -> f32 {
    alpha.unwrap_or_else(|| {
        if is_symbol_query(query) {
            0.25
        } else if is_architecture_query(query) || is_natural_language_question(query) {
            0.65
        } else if is_mixed_code_phrase(query) {
            0.45
        } else if is_short_keyword_query(query) {
            0.35
        } else {
            0.55
        }
    })
}

pub(crate) fn truncate_top_k(scores: &mut Vec<(usize, f32)>, k: usize) {
    if scores.len() <= k {
        scores.sort_unstable_by(desc_score);
        return;
    }
    scores.select_nth_unstable_by(k, desc_score);
    scores.truncate(k);
    scores.sort_unstable_by(desc_score);
}

fn desc_score(a: &(usize, f32), b: &(usize, f32)) -> Ordering {
    b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal)
}

pub fn is_symbol_query(query: &str) -> bool {
    SYMBOL_QUERY_RE.is_match(query.trim())
}

fn is_architecture_query(query: &str) -> bool {
    let lowered = query.trim().to_lowercase();
    lowered.contains(" architecture")
        || lowered.contains(" design")
        || lowered.contains(" flow")
        || lowered.contains(" lifecycle")
        || lowered.contains(" pipeline")
        || lowered.starts_with("how does ")
        || lowered.starts_with("how are ")
}

fn is_natural_language_question(query: &str) -> bool {
    let lowered = query.trim().to_lowercase();
    lowered.ends_with('?')
        || lowered.starts_with("how ")
        || lowered.starts_with("what ")
        || lowered.starts_with("where ")
        || lowered.starts_with("when ")
        || lowered.starts_with("why ")
        || lowered.starts_with("which ")
        || lowered.starts_with("who ")
}

fn is_mixed_code_phrase(query: &str) -> bool {
    EMBEDDED_SYMBOL_RE.is_match(query)
        || query.split_whitespace().any(|part| {
            is_symbol_query(part.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_'))
        })
}

fn is_short_keyword_query(query: &str) -> bool {
    let keywords = super::tokens::tokenize(query)
        .into_iter()
        .filter(|w| w.len() > 1 && !STOPWORDS.contains(w.as_str()))
        .take(4)
        .count();
    (1..=3).contains(&keywords)
}

#[derive(Clone, Debug)]
pub struct QueryIntent {
    keywords: Vec<String>,
    identifiers: Vec<String>,
    wants_test: bool,
    wants_docs: bool,
    wants_vendor: bool,
    wants_generated: bool,
    wants_type_defs: bool,
    wants_public_api: bool,
    wants_component: bool,
    wants_hook: bool,
    wants_route: bool,
    wants_schema: bool,
    wants_config: bool,
    wants_model: bool,
    wants_cli: bool,
    wants_cache: bool,
}

impl QueryIntent {
    pub fn new(query: &str) -> Self {
        let lowered = query.trim().to_lowercase();
        let keywords = unique_words(
            super::tokens::tokenize(query)
                .into_iter()
                .filter(|w| w.len() > 1 && !STOPWORDS.contains(w.as_str()))
                .collect(),
        );
        let mut identifiers: Vec<String> = EMBEDDED_SYMBOL_RE
            .find_iter(query)
            .map(|m| m.as_str().to_lowercase())
            .collect();
        for part in query.split_whitespace() {
            let cleaned =
                part.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '.');
            if cleaned.len() > 2 && is_symbol_query(cleaned) {
                identifiers.push(extract_symbol_name(cleaned).to_lowercase());
            }
        }
        identifiers = unique_words(identifiers);

        let wants_test = contains_any(
            &keywords,
            &[
                "test",
                "tests",
                "spec",
                "assert",
                "expect",
                "fixture",
                "mock",
                "getbytext",
            ],
        ) || lowered.contains("test file")
            || lowered.contains("test block");
        let wants_docs = contains_any(
            &keywords,
            &[
                "doc",
                "docs",
                "documentation",
                "readme",
                "changelog",
                "help",
                "usage",
                "guide",
            ],
        );
        let wants_vendor = contains_any(&keywords, &["vendor", "thirdparty", "grammar"]);
        let wants_generated = contains_any(&keywords, &["generated", "codegen"]);
        let wants_type_defs = lowered.contains(".d.ts")
            || contains_any(
                &keywords,
                &["declaration", "declarations", "typedef", "types", "type"],
            );
        let wants_public_api = lowered.contains("public api")
            || contains_any(&keywords, &["public", "api", "barrel", "export", "exports"]);

        Self {
            keywords,
            identifiers,
            wants_test,
            wants_docs,
            wants_vendor,
            wants_generated,
            wants_type_defs,
            wants_public_api,
            wants_component: contains_any_word(query, &["component", "components"]),
            wants_hook: contains_any_word(query, &["hook", "hooks"]),
            wants_route: contains_any_word(query, &["route", "routes", "loader", "params"]),
            wants_schema: contains_any_word(query, &["schema", "schemas", "model", "models"]),
            wants_config: contains_any_word(query, &["config", "configuration", "settings"]),
            wants_model: contains_any_word(query, &["model", "models"]),
            wants_cli: contains_any_word(query, &["cli", "command", "commands"]),
            wants_cache: contains_any_word(query, &["cache", "caches", "cached"]),
        }
    }

    fn has_path_intent(&self) -> bool {
        !self.identifiers.is_empty()
            || (1..=4).contains(&self.keywords.len())
            || self.wants_test
            || self.wants_docs
            || self.wants_vendor
            || self.wants_generated
            || self.wants_type_defs
            || self.wants_public_api
            || self.wants_component
            || self.wants_hook
            || self.wants_route
            || self.wants_schema
            || self.wants_config
            || self.wants_model
            || self.wants_cli
            || self.wants_cache
    }
}

fn contains_any(words: &[String], needles: &[&str]) -> bool {
    words.iter().any(|word| needles.contains(&word.as_str()))
}

fn contains_any_word(text: &str, needles: &[&str]) -> bool {
    let words = super::tokens::tokenize(text);
    words.iter().any(|word| needles.contains(&word.as_str()))
}

/// Boost the best-scoring chunk in each multi-chunk file. Encourages the
/// final top-k to include at most one entry per file when scores are close
/// together (the rerank step still applies a per-file diversity penalty on
/// top of this).
pub fn boost_multi_chunk_files<S: BuildHasher>(
    scores: &mut HashMap<usize, f32, S>,
    chunks: &ChunkStore,
) {
    if scores.is_empty() {
        return;
    }
    let max_score = scores.values().copied().fold(0.0f32, f32::max);
    if max_score == 0.0 {
        return;
    }
    let mut file_sum: HashMap<&str, f32> = HashMap::new();
    let mut best_chunk: HashMap<&str, usize> = HashMap::new();
    for (&chunk_id, &score) in scores.iter() {
        let file = chunks.file_path(chunk_id);
        *file_sum.entry(file).or_default() += score;
        if best_chunk
            .get(file)
            .is_none_or(|&best| score > scores[&best])
        {
            best_chunk.insert(file, chunk_id);
        }
    }
    let max_file_sum = file_sum.values().copied().fold(0.0f32, f32::max);
    let boost_unit = max_score * 0.2;
    for (file, chunk_id) in best_chunk {
        if let Some(score) = scores.get_mut(&chunk_id) {
            *score += boost_unit * file_sum[file] / max_file_sum;
        }
    }
}

pub fn apply_query_boost_in_place<S: BuildHasher>(
    mut boosted: HashMap<usize, f32, S>,
    intent: &QueryIntent,
    query: &str,
    chunks: &ChunkStore,
    file_mapping: Option<&BTreeMap<String, Vec<usize>>>,
    selector: Option<&[usize]>,
) -> HashMap<usize, f32, S> {
    if boosted.is_empty() {
        return boosted;
    }
    let max_score = boosted.values().copied().fold(f32::NEG_INFINITY, f32::max);
    let allowed: Option<HashSet<usize>> = selector.map(|ids| ids.iter().copied().collect());
    let allowed = allowed.as_ref();
    if is_symbol_query(query) {
        boost_symbol_definitions(&mut boosted, query, max_score, chunks, allowed);
    } else {
        if stem_boost_query(query) {
            boost_stem_matches(&mut boosted, query, max_score, chunks);
        }
        if may_contain_embedded_symbol(query) {
            boost_embedded_symbols(&mut boosted, query, max_score, chunks, allowed);
        }
    }
    boost_path_intent(&mut boosted, intent, max_score, chunks);
    boost_named_non_candidates(
        &mut boosted,
        intent,
        max_score,
        chunks,
        file_mapping,
        allowed,
    );
    boosted
}

fn may_contain_embedded_symbol(query: &str) -> bool {
    EMBEDDED_SYMBOL_RE.is_match(query)
}

fn stem_boost_query(query: &str) -> bool {
    let lowered = query.to_lowercase();
    [
        "public",
        "api",
        "config",
        "schema",
        "builder",
        "worker",
        "reporter",
        "snapshot",
        "state",
        "task",
        "type",
        "error",
        "parser",
        "serializer",
        "deserializer",
        "router",
        "request",
        "response",
    ]
    .iter()
    .any(|kw| lowered.contains(kw))
}

fn boost_symbol_definitions<S: BuildHasher>(
    boosted: &mut HashMap<usize, f32, S>,
    query: &str,
    max_score: f32,
    chunks: &ChunkStore,
    allowed: Option<&HashSet<usize>>,
) {
    let symbol_name = extract_symbol_name(query);
    let mut names = HashSet::from([symbol_name.clone()]);
    if symbol_name != query.trim() {
        names.insert(query.trim().to_owned());
    }
    let matchers = definition_matchers(&names);
    let boost_unit = max_score * 3.0;
    for chunk_id in boosted.keys().copied().collect::<Vec<_>>() {
        let tier = definition_tier(
            chunks.content(chunk_id),
            chunks.file_path(chunk_id),
            &names,
            &matchers,
            boost_unit,
        );
        if tier > 0.0 {
            *boosted.get_mut(&chunk_id).unwrap() += tier;
        }
    }
    let symbol_lower = symbol_name.to_lowercase();
    for chunk_id in 0..chunks.len() {
        if boosted.contains_key(&chunk_id) {
            continue;
        }
        if !selector_allows(allowed, chunk_id) {
            continue;
        }
        let file_path = chunks.file_path(chunk_id);
        let stem = path_stem_lower(file_path);
        if stem_matches(&stem, &symbol_lower) {
            let tier = definition_tier(
                chunks.content(chunk_id),
                file_path,
                &names,
                &matchers,
                boost_unit,
            );
            if tier > 0.0 {
                boosted.insert(chunk_id, tier);
            }
        }
    }
}

fn boost_embedded_symbols<S: BuildHasher>(
    boosted: &mut HashMap<usize, f32, S>,
    query: &str,
    max_score: f32,
    chunks: &ChunkStore,
    allowed: Option<&HashSet<usize>>,
) {
    let names: HashSet<String> = EMBEDDED_SYMBOL_RE
        .find_iter(query)
        .map(|m| m.as_str().to_owned())
        .collect();
    if names.is_empty() {
        return;
    }
    let matchers = definition_matchers(&names);
    let boost_unit = max_score * 3.0 * 0.5;
    for chunk_id in boosted.keys().copied().collect::<Vec<_>>() {
        let tier = definition_tier(
            chunks.content(chunk_id),
            chunks.file_path(chunk_id),
            &names,
            &matchers,
            boost_unit,
        );
        if tier > 0.0 {
            *boosted.get_mut(&chunk_id).unwrap() += tier;
        }
    }
    let symbols_lower: Vec<String> = names.iter().map(|s| s.to_lowercase()).collect();
    for chunk_id in 0..chunks.len() {
        if boosted.contains_key(&chunk_id) {
            continue;
        }
        if !selector_allows(allowed, chunk_id) {
            continue;
        }
        let file_path = chunks.file_path(chunk_id);
        let stem = path_stem_lower(file_path);
        let stem_norm = stem.replace('_', "");
        let stem_ok = symbols_lower.iter().any(|symbol| {
            stem == *symbol
                || stem_norm == *symbol
                || (stem.len() >= 4 && symbol.starts_with(&stem))
                || (stem_norm.len() >= 4 && symbol.starts_with(&stem_norm))
        });
        if stem_ok {
            let tier = definition_tier(
                chunks.content(chunk_id),
                file_path,
                &names,
                &matchers,
                boost_unit,
            );
            if tier > 0.0 {
                boosted.insert(chunk_id, tier);
            }
        }
    }
}

fn boost_stem_matches<S: BuildHasher>(
    boosted: &mut HashMap<usize, f32, S>,
    query: &str,
    max_score: f32,
    chunks: &ChunkStore,
) {
    let query_words: Vec<String> = super::tokens::tokenize(query)
        .into_iter()
        .filter(|w| w.len() > 2 && !STOPWORDS.contains(w.as_str()))
        .collect();
    if query_words.is_empty() {
        return;
    }
    let keywords = unique_words(query_words);
    let public_api = public_api_query(&keywords);
    let mut path_matches: HashMap<String, (usize, bool)> = HashMap::new();
    for (chunk_id, score) in boosted.iter_mut() {
        let file_path = chunks.file_path(*chunk_id);
        let entry = path_matches.entry(file_path.to_owned()).or_insert_with(|| {
            (
                count_keyword_path_matches(&keywords, file_path),
                public_api_file(file_path),
            )
        });
        let (matches, is_public_api_file) = *entry;
        if matches > 0 {
            *score += max_score * matches as f32 / keywords.len() as f32;
        }
        if public_api && is_public_api_file {
            *score += max_score * 0.8;
        }
    }
}

fn boost_path_intent<S: BuildHasher>(
    boosted: &mut HashMap<usize, f32, S>,
    intent: &QueryIntent,
    max_score: f32,
    chunks: &ChunkStore,
) {
    if !intent.has_path_intent() {
        return;
    }
    for (chunk_id, score) in boosted.iter_mut() {
        let path_score = path_intent_score(intent, chunks.file_path(*chunk_id));
        if path_score > 0.0 {
            *score += max_score * path_score.min(2.5) * 0.45;
        }
    }
}

fn boost_named_non_candidates<S: BuildHasher>(
    boosted: &mut HashMap<usize, f32, S>,
    intent: &QueryIntent,
    max_score: f32,
    chunks: &ChunkStore,
    file_mapping: Option<&BTreeMap<String, Vec<usize>>>,
    allowed: Option<&HashSet<usize>>,
) {
    if !intent.has_path_intent() {
        return;
    }
    let Some(file_mapping) = file_mapping else {
        return;
    };
    // `definition_matchers` is regex-heavy. Don't pay the build cost up
    // front (most queries never trigger an insertion here); lazy-init on
    // the first chunk that gets close enough to qualify, then reuse for
    // every subsequent chunk in the loop.
    let mut identifier_matchers: Option<Vec<DefinitionMatcher>> = None;
    let mut inserted = 0usize;
    let max_insertions = 24usize;
    for (path, chunk_ids) in file_mapping {
        if inserted >= max_insertions {
            break;
        }
        if chunk_ids.iter().any(|id| boosted.contains_key(id)) {
            continue;
        }
        let path_score = path_intent_score(intent, path);
        if path_score < 0.9 {
            continue;
        }
        let Some(chunk_id) = chunk_ids
            .iter()
            .copied()
            .find(|id| selector_allows(allowed, *id))
        else {
            continue;
        };
        let matchers = identifier_matchers.get_or_insert_with(|| {
            if intent.identifiers.is_empty() {
                Vec::new()
            } else {
                let names: HashSet<String> = intent.identifiers.iter().cloned().collect();
                definition_matchers(&names)
            }
        });
        let source_boost =
            if chunk_defines_any_identifier(chunks.content(chunk_id), matchers) {
                1.35
            } else {
                0.85
            };
        boosted.insert(chunk_id, max_score * source_boost * path_score.min(2.0));
        inserted += 1;
    }
}

fn chunk_defines_any_identifier(content: &str, matchers: &[DefinitionMatcher]) -> bool {
    if matchers.is_empty() {
        return false;
    }
    chunk_defines_symbol(content, matchers)
}

fn selector_allows(allowed: Option<&HashSet<usize>>, chunk_id: usize) -> bool {
    allowed.is_none_or(|ids| ids.contains(&chunk_id))
}

fn path_intent_score(intent: &QueryIntent, file_path: &str) -> f32 {
    let normalized = file_path.replace('\\', "/").to_lowercase();
    let mut score = 0.0f32;
    if !intent.keywords.is_empty() {
        let matches = count_keyword_path_matches(&intent.keywords, file_path);
        score += matches as f32 / intent.keywords.len() as f32;
    }
    for identifier in &intent.identifiers {
        let identifier_parts = split_identifier(identifier);
        let matches = count_keyword_path_matches(&identifier_parts, file_path);
        if matches > 0 {
            score += 0.9 + matches as f32 / identifier_parts.len().max(1) as f32;
        }
    }
    if intent.wants_component && normalized.contains("/components/") {
        score += 0.9;
    }
    if intent.wants_hook
        && (normalized.contains("/hooks/") || path_stem_lower(file_path).starts_with("use"))
    {
        score += 0.9;
    }
    if intent.wants_route && normalized.contains("/routes/") {
        score += 0.9;
    }
    if intent.wants_schema && normalized.contains("schema") {
        score += 0.8;
    }
    if intent.wants_config && (normalized.contains("config") || normalized.contains("settings")) {
        score += 0.8;
    }
    if intent.wants_model && normalized.contains("model") {
        score += 0.6;
    }
    if intent.wants_cli && (normalized.contains("/cli") || normalized.ends_with("main.rs")) {
        score += 0.7;
    }
    if intent.wants_cache && normalized.contains("cache") {
        score += 0.8;
    }
    if intent.wants_public_api && public_api_file(file_path) {
        score += 1.0;
    }
    if intent.wants_type_defs && TYPE_DEFS_RE.is_match(&normalized) {
        score += 1.0;
    }
    if intent.wants_test && is_test_path(&normalized) {
        score += 1.0;
    }
    if intent.wants_docs && is_docs_path(&normalized) {
        score += 1.0;
    }
    if intent.wants_vendor && is_vendor_path(&normalized) {
        score += 1.0;
    }
    if intent.wants_generated && is_generated_path(&normalized) {
        score += 1.0;
    }
    score
}

fn unique_words(words: Vec<String>) -> Vec<String> {
    let mut unique = Vec::with_capacity(words.len());
    for word in words {
        if !unique.contains(&word) {
            unique.push(word);
        }
    }
    unique
}

fn count_keyword_path_matches(keywords: &[String], file_path: &str) -> usize {
    let parts = path_words(file_path);
    let mut matches = 0;
    for keyword in keywords {
        if parts.iter().any(|part| path_word_matches(keyword, part)) {
            matches += 1;
        }
    }
    matches
}

fn path_words(file_path: &str) -> Vec<String> {
    let mut words = super::tokens::tokenize(file_path);
    let path = Path::new(file_path);
    if let Some(stem) = path.file_stem() {
        words.extend(split_identifier(&stem.to_string_lossy().to_lowercase()));
    }
    unique_words(
        words
            .into_iter()
            .filter(|word| word.len() > 1 && !STOPWORDS.contains(word.as_str()))
            .collect(),
    )
}

fn path_word_matches(keyword: &str, path_word: &str) -> bool {
    if keyword == path_word {
        return true;
    }
    let keyword = keyword.trim_end_matches('s');
    let path_word = path_word.trim_end_matches('s');
    if keyword == path_word {
        return true;
    }
    let (shorter, longer) = if keyword.len() <= path_word.len() {
        (keyword, path_word)
    } else {
        (path_word, keyword)
    };
    shorter.len() >= 3 && longer.starts_with(shorter)
}

fn public_api_query(keywords: &[String]) -> bool {
    keywords.iter().any(|keyword| {
        matches!(
            keyword.as_str(),
            "api" | "public" | "function" | "functions" | "builder" | "builders"
        )
    })
}

fn public_api_file(file_path: &str) -> bool {
    matches!(
        path_stem_lower(file_path).as_str(),
        "api" | "public" | "index" | "mod" | "lib"
    )
}

fn extract_symbol_name(query: &str) -> String {
    for sep in ["::", "\\", "->", "."] {
        if let Some((_, leaf)) = query.rsplit_once(sep) {
            return leaf.to_owned();
        }
    }
    query.trim().to_owned()
}

struct DefinitionMatcher {
    general: Regex,
    sql: Regex,
}

fn definition_matchers(names: &HashSet<String>) -> Vec<DefinitionMatcher> {
    names
        .iter()
        .filter_map(|name| {
            let escaped = regex::escape(name);
            let general_pattern = definition_pattern(DEFINITION_KEYWORDS, &escaped, "");
            let sql_pattern = definition_pattern(SQL_DEFINITION_KEYWORDS, &escaped, "i");
            Some(DefinitionMatcher {
                general: Regex::new(&general_pattern).ok()?,
                sql: Regex::new(&sql_pattern).ok()?,
            })
        })
        .collect()
}

fn definition_pattern(keywords: &[&str], escaped_name: &str, flags: &str) -> String {
    let prefix = if flags.is_empty() { "(?m)" } else { "(?im)" };
    format!(
        r"{prefix}(?:^|\s)(?:{})\s+(?:[A-Za-z_][A-Za-z0-9_]*(?:\.|::))*\$?{}(?:\s|[<({{\:\[;]|$)",
        keywords
            .iter()
            .map(|k| regex::escape(k))
            .collect::<Vec<_>>()
            .join("|"),
        escaped_name
    )
}

fn chunk_defines_symbol(content: &str, matchers: &[DefinitionMatcher]) -> bool {
    matchers
        .iter()
        .any(|matcher| matcher.general.is_match(content) || matcher.sql.is_match(content))
}

fn definition_tier(
    content: &str,
    file_path: &str,
    names: &HashSet<String>,
    matchers: &[DefinitionMatcher],
    boost_unit: f32,
) -> f32 {
    if !chunk_defines_symbol(content, matchers) {
        return 0.0;
    }
    let stem = path_stem_lower(file_path);
    if names
        .iter()
        .any(|name| stem_matches(&stem, &name.to_lowercase()))
    {
        boost_unit * 1.5
    } else {
        boost_unit
    }
}

fn stem_matches(stem: &str, name: &str) -> bool {
    let stem_norm = stem.replace('_', "");
    stem == name
        || stem_norm == name
        || stem.trim_end_matches('s') == name
        || stem_norm.trim_end_matches('s') == name
}

fn path_stem_lower(file_path: &str) -> String {
    Path::new(file_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Final top-k selection. Applies a test/compat/examples path penalty and
/// per-file diversity (each additional chunk from the same file gets a 50%
/// hit) before sorting and truncating.
pub fn rerank_topk<S: BuildHasher>(
    scores: &HashMap<usize, f32, S>,
    chunks: &ChunkStore,
    top_k: usize,
    intent: &QueryIntent,
    penalise_paths: bool,
) -> Vec<(usize, f32)> {
    if scores.is_empty() || top_k == 0 {
        return Vec::new();
    }
    let mut penalized: Vec<(usize, f32)> = scores
        .iter()
        .map(|(&id, &score)| {
            let penalty = if penalise_paths {
                file_path_penalty(chunks.file_path(id), intent)
            } else {
                1.0
            };
            (id, score * penalty)
        })
        .collect();
    penalized.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    let mut file_selected: HashMap<&str, usize> = HashMap::new();
    let mut selected: Vec<(usize, f32)> = Vec::new();
    let mut min_selected = f32::INFINITY;
    for (chunk_id, score) in penalized {
        if selected.len() >= top_k && score <= min_selected {
            break;
        }
        let file = chunks.file_path(chunk_id);
        let already = *file_selected.get(file).unwrap_or(&0);
        let mut eff_score = score;
        if already >= 1 {
            let excess = already;
            eff_score *= 0.5f32.powi(excess as i32);
        }
        selected.push((chunk_id, eff_score));
        file_selected.insert(file, already + 1);
        if selected.len() >= top_k {
            min_selected = selected
                .iter()
                .map(|(_, s)| *s)
                .fold(f32::INFINITY, f32::min);
        }
    }
    selected.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    selected.truncate(top_k.min(selected.len()));
    selected
}

fn file_path_penalty(file_path: &str, intent: &QueryIntent) -> f32 {
    let normalized = file_path.replace('\\', "/");
    let lowered = normalized.to_lowercase();
    let mut penalty = 1.0f32;
    if !intent.wants_test && is_test_path(&lowered) {
        penalty *= 0.3;
    }
    let name = Path::new(file_path)
        .file_name()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    if name == "__init__.py" || name == "package-info.java" {
        penalty *= 0.5;
    }
    if !intent.wants_docs && is_docs_path(&lowered) {
        penalty *= 0.55;
    }
    if !intent.wants_vendor && is_vendor_path(&lowered) {
        penalty *= 0.35;
    }
    if !intent.wants_generated && is_generated_path(&lowered) {
        penalty *= 0.45;
    }
    if COMPAT_DIR_RE.is_match(&lowered) {
        penalty *= 0.3;
    }
    if !intent.wants_docs && EXAMPLES_DIR_RE.is_match(&lowered) {
        penalty *= 0.3;
    }
    if !intent.wants_type_defs && TYPE_DEFS_RE.is_match(&lowered) {
        penalty *= 0.7;
    }
    penalty
}

fn is_test_path(normalized: &str) -> bool {
    TEST_FILE_RE.is_match(normalized) || TEST_DIR_RE.is_match(normalized)
}

fn is_docs_path(normalized: &str) -> bool {
    normalized.starts_with("docs/")
        || normalized.contains("/docs/")
        || normalized.starts_with("doc/")
        || normalized.contains("/doc/")
        || normalized.ends_with("readme.md")
        || normalized.ends_with("changelog.md")
        || normalized.ends_with("contributing.md")
        || normalized.ends_with("license")
}

fn is_vendor_path(normalized: &str) -> bool {
    normalized.starts_with("vendor/")
        || normalized.contains("/vendor/")
        || normalized.starts_with("third_party/")
        || normalized.contains("/third_party/")
}

fn is_generated_path(normalized: &str) -> bool {
    normalized.contains("generated")
        || normalized.contains("/gen/")
        || normalized.ends_with(".generated.ts")
        || normalized.ends_with(".generated.tsx")
        || normalized.ends_with(".pb.go")
}

#[cfg(test)]
mod tests {
    use super::{apply_query_boost_in_place, rerank_topk, resolve_alpha, QueryIntent};
    use crate::search::chunk_store::ChunkStore;
    use crate::search::types::IndexedChunk;
    use std::collections::{BTreeMap, HashMap};

    fn chunk(content: &str, file_path: &str) -> IndexedChunk {
        IndexedChunk {
            content: content.to_owned(),
            file_path: file_path.to_owned(),
            start_line: 1,
            end_line: 1,
            language: Some("python".to_owned()),
        }
    }

    fn store(chunks: Vec<IndexedChunk>) -> ChunkStore {
        ChunkStore::from_indexed(chunks)
    }

    #[test]
    fn boosts_definitions_and_penalizes_tests() {
        let defining = chunk("class MyService:\n    pass", "src/my_service.py");
        let other = chunk("x = MyService()", "src/utils.py");
        let chunks = store(vec![defining, other]);
        let mut scores = HashMap::new();
        scores.insert(1usize, 0.4);
        let intent = QueryIntent::new("MyService");
        let boosted = apply_query_boost_in_place(scores, &intent, "MyService", &chunks, None, None);
        assert!(boosted[&0] > boosted[&1]);

        let defining = chunk("pub fn parse_json(input: &str) {}", "src/parser.rs");
        let other = chunk("parse json input", "src/readme.md");
        let chunks = store(vec![defining, other]);
        let mut scores = HashMap::new();
        scores.insert(0usize, 0.2);
        scores.insert(1usize, 0.4);
        let intent = QueryIntent::new("where is parse_json handled");
        let boosted = apply_query_boost_in_place(
            scores,
            &intent,
            "where is parse_json handled",
            &chunks,
            None,
            None,
        );
        assert!(boosted[&0] > boosted[&1]);

        let regular = chunk("def impl(): pass", "src/regular.py");
        let test = chunk("def impl(): pass", "tests/test_auth.py");
        let chunks = store(vec![regular, test]);
        let scores = HashMap::from([(0usize, 1.0), (1usize, 1.0)]);
        let intent = QueryIntent::new("impl");
        let ranked = rerank_topk(&scores, &chunks, 2, &intent, true);
        assert_eq!(ranked[0].0, 0);

        let scores = HashMap::from([(0usize, 1.0), (1usize, 1.1)]);
        let intent = QueryIntent::new("test impl");
        let ranked = rerank_topk(&scores, &chunks, 2, &intent, true);
        assert_eq!(ranked[0].0, 1);
    }

    #[test]
    fn named_path_insertions_respect_selectors() {
        let other = chunk("const unrelated = true;", "src/other.ts");
        let component = chunk(
            "export function UserCard() { return null; }",
            "src/components/UserCard.tsx",
        );
        let chunks = store(vec![other, component]);
        let mapping = BTreeMap::from([
            ("src/components/UserCard.tsx".to_owned(), vec![1usize]),
            ("src/other.ts".to_owned(), vec![0usize]),
        ]);
        let scores = HashMap::from([(0usize, 1.0)]);
        let intent = QueryIntent::new("UserCard React component");

        let excluded = apply_query_boost_in_place(
            scores.clone(),
            &intent,
            "UserCard React component",
            &chunks,
            Some(&mapping),
            Some(&[0usize]),
        );
        assert!(!excluded.contains_key(&1));

        let included = apply_query_boost_in_place(
            scores,
            &intent,
            "UserCard React component",
            &chunks,
            Some(&mapping),
            Some(&[0usize, 1usize]),
        );
        assert!(included.contains_key(&1));
    }

    #[test]
    fn alpha_detection_matches_symbol_vs_natural_language() {
        assert_eq!(resolve_alpha("MyService", None), 0.25);
        assert_eq!(resolve_alpha("parse_json", None), 0.25);
        assert_eq!(resolve_alpha("parse hunk header", None), 0.35);
        assert_eq!(resolve_alpha("how does routing work", None), 0.65);
        assert_eq!(resolve_alpha("request lifecycle architecture", None), 0.65);
        assert_eq!(
            resolve_alpha("where is request validation handled?", None),
            0.65
        );
        assert_eq!(resolve_alpha("useSession hook", None), 0.45);
        assert_eq!(
            resolve_alpha("request validation and error handling", None),
            0.55
        );
        assert_eq!(resolve_alpha("MyService", Some(0.7)), 0.7);
    }
}
