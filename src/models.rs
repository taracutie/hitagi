use std::collections::BTreeMap;

use serde::Serialize;

// Internal types: shared by parser and queries. Not serialized.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeInfo {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
