use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// Internal types shared by parser, queries, and cache. RangeInfo and SymbolInfo
// are wire-format-stable: the on-disk parse cache (cache.rs) bincodes them, so
// changing their shape requires bumping CACHE_VERSION_KEY in cache.rs (or relying
// on the crate-version suffix it already includes).

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeInfo {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolInfo {
    pub kind: String,
    pub name: String,
    pub qualname: String,
    pub range: RangeInfo,
    pub parent: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SymbolDetail {
    pub kind: String,
    pub name: String,
    pub qualname: String,
    pub content: String,
    pub range: RangeInfo,
}

// External output: the JSON shape the CLI prints.

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Serialize)]
pub struct OutputSymbol {
    pub kind: String,
    pub name: String,
    pub qualname: String,
    pub lines: [usize; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<[usize; 2]>,
}

#[derive(Debug, Serialize)]
pub struct OutputSymbolDetail {
    pub kind: String,
    pub name: String,
    pub qualname: String,
    pub content: String,
    pub lines: [usize; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<[usize; 2]>,
}

#[derive(Debug, Serialize)]
pub struct OutlineResponse {
    pub language: String,
    pub symbols: Vec<OutputSymbol>,
    /// Set when --kind was passed but matched zero symbols ~ lists what was actually available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_kinds: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct SymbolResponse {
    pub language: String,
    pub symbol: OutputSymbolDetail,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    pub results: BTreeMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct ReadFileResponse {
    pub language: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<[usize; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<usize>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum FindMatches {
    /// Default: structured per-match objects.
    Full(Vec<FindMatch>),
    /// `--terse`: compact "path:line qualname(kind)" strings.
    Terse(Vec<String>),
}

#[derive(Debug, Serialize)]
pub struct FindResponse {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    pub matches: FindMatches,
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
    pub searched_files: usize,
    /// Set when --kind was passed but matched zero symbols ~ lists what was actually available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_kinds: Option<Vec<String>>,
    /// Hint shown when matches is empty and we couldn't parse anything (e.g. searched a vendor dir).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FindMatch {
    pub path: String,
    pub kind: String,
    pub name: String,
    pub qualname: String,
    pub lines: [usize; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<[usize; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FilesResponse {
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Hint emitted alongside `truncated: true` ~ tells the agent how to refine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LangsResponse {
    pub languages: Vec<LangSummary>,
}

#[derive(Debug, Serialize)]
pub struct LangSummary {
    pub language: String,
    pub files: usize,
    pub lines: usize,
    pub parseable: bool,
}

#[derive(Debug, Serialize)]
pub struct CacheStatusResponse {
    pub enabled: bool,
    pub disabled_via_env: bool,
    pub current_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_file: Option<String>,
    pub exists: bool,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_unix_secs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stored_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stored_repo_root: Option<String>,
    pub version_match: bool,
    pub repo_root_match: bool,
    pub entry_count: usize,
    pub languages: Vec<CacheLangCount>,
}

#[derive(Debug, Serialize)]
pub struct CacheLangCount {
    pub language: String,
    pub files: usize,
}

#[derive(Debug, Serialize)]
pub struct CachePathResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CacheClearResponse {
    /// "repo" (default) or "all" (with --all).
    pub scope: String,
    pub path: String,
    /// True when something was actually removed.
    pub cleared: bool,
    /// For scope=all only: number of repo subdirs that existed before delete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repos_removed: Option<usize>,
}
