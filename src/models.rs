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
    pub qualname: String,
    pub content: String,
    pub range: RangeInfo,
}

// Structured response models shared by the command layer and test assertions.

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Serialize)]
pub struct OutputSymbol {
    pub kind: String,
    pub qualname: String,
    pub lines: [usize; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<[usize; 2]>,
}

#[derive(Debug, Serialize)]
pub struct OutputSymbolDetail {
    pub kind: String,
    pub qualname: String,
    pub content: String,
    pub lines: [usize; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<[usize; 2]>,
}

#[derive(Debug, Serialize)]
pub struct OutlineResponse {
    pub language: String,
    /// Total parsed symbols in the file BEFORE --depth/--kind filtering.
    /// Lets the caller decide whether to drill in further on the next call.
    pub total_symbols: usize,
    /// Per-kind counts BEFORE --depth/--kind filtering. Same orientation use
    /// as `available_kinds` but with the actual breakdown attached.
    pub kind_counts: BTreeMap<String, usize>,
    pub symbols: Vec<OutputSymbol>,
    /// Set when --kind was passed but matched zero symbols ~ lists what was actually available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_kinds: Option<Vec<String>>,
    /// True when the response was auto-summarized to --depth 1 because the
    /// file had more symbols than the soft cap and the caller didn't pass
    /// --depth/--kind/--bytes. Re-run with explicit --depth N to override.
    #[serde(skip_serializing_if = "is_false", default)]
    pub auto_summarized: bool,
    /// Human-readable hint accompanying `auto_summarized`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SymbolResponse {
    pub language: String,
    pub symbol: OutputSymbolDetail,
}

/// Ranked-search response. Replaces the old literal-substring shape ~ each
/// `result` is a chunk (path + start/end + score + source mode), not a list
/// of `scope(kind) @L<n>` strings. Hits are pre-sorted by score desc.
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub mode: String,
    /// Resolved alpha (0.0=BM25, 1.0=semantic). Always emitted so the agent
    /// can see whether the auto-tuner kicked in or `--alpha` overrode.
    pub alpha: f32,
    pub limit: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub languages: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub paths: Vec<String>,
    pub elapsed_ms: u128,
    pub indexed_files: usize,
    pub indexed_chunks: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
    pub results: Vec<SearchHit>,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub path: String,
    pub lines: [usize; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub score: f32,
    /// Which side of the hybrid actually surfaced this hit: `bm25`,
    /// `semantic`, or `hybrid` (when the fusion contributed).
    pub source: String,
    /// First non-blank line of the chunk. Only emitted with `--snippet`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// `find-related` response. Contains the resolved source chunk so the
/// caller can verify they pointed at what they meant, plus the related
/// hits (excluding the source).
#[derive(Debug, Serialize)]
pub struct FindRelatedResponse {
    pub path: String,
    pub line: usize,
    pub limit: usize,
    pub elapsed_ms: u128,
    pub indexed_files: usize,
    pub indexed_chunks: usize,
    pub source_chunk: SearchHit,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
    pub results: Vec<SearchHit>,
}

/// `mimi index status` response. Reports what's persisted in the search
/// portion of the SQLite cache without forcing a load.
#[derive(Debug, Serialize)]
pub struct IndexStatusResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_file: Option<String>,
    pub sparse_present: bool,
    pub dense_present: bool,
    pub indexed_files: usize,
    pub indexed_chunks: usize,
    pub languages: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dim: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sparse_built_at_unix_secs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dense_built_at_unix_secs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sparse_size_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dense_size_bytes: Option<usize>,
}

/// `mimi index build` response.
#[derive(Debug, Serialize)]
pub struct IndexBuildResponse {
    pub mode: String,
    pub indexed_files: usize,
    pub indexed_chunks: usize,
    pub languages: BTreeMap<String, usize>,
    pub elapsed_ms: u128,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

/// `mimi index clean` response.
#[derive(Debug, Serialize)]
pub struct IndexCleanResponse {
    /// True when at least one search row was deleted.
    pub cleared: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_file: Option<String>,
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
pub struct ReadSummaryResponse {
    pub language: String,
    pub lines: usize,
    pub bytes: usize,
    pub blank: usize,
    pub comment: usize,
    pub code: usize,
    pub parseable: bool,
    pub total_symbols: usize,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub kind_counts: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub symbols: Vec<OutputSymbol>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
    /// Always present. Empty (`[]`) when grouping is in effect or when zero
    /// matches were found ~ keeps the field stable for consumers.
    pub matches: FindMatches,
    /// Per-prefix groups, populated when matches span multiple top-level dirs
    /// and no global LCP exists. Each group carries its own `prefix`,
    /// `matches`, and (if `--per-file` is set) `more_in_file`. When populated,
    /// the top-level `prefix`/`matches`/`more_in_file` are empty/omitted.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub groups: Vec<FindGroup>,
    /// Per-file overflow when `--per-file N` capped output. Keys match the
    /// stripped paths in `matches` (top-level prefix applied). Omitted when
    /// empty or when grouping moved overflow into per-group containers.
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub more_in_file: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
    pub searched_files: usize,
    /// Top-level directories the walk never visited because the limit was
    /// hit first. Only present when `truncated == true` AND at least one
    /// subtree was skipped. Same intent as the field on SearchResponse.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub unsampled_dirs: Vec<String>,
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
    pub qualname: String,
    pub lines: [usize; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<[usize; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FindGroup {
    pub prefix: String,
    pub matches: FindMatches,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub more_in_file: BTreeMap<String, usize>,
}

#[derive(Debug, Serialize)]
pub struct LocSymbolsResponse {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub languages: Vec<String>,
    pub kinds: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<usize>,
    pub limit: usize,
    pub sort: String,
    pub scanned_files: usize,
    pub total_matches: usize,
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
    pub results: Vec<LocSymbolResult>,
}

#[derive(Debug, Serialize)]
pub struct LocSymbolResult {
    pub path: String,
    pub language: String,
    pub kind: String,
    pub qualname: String,
    pub lines: [usize; 2],
    pub code: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<[usize; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LocFilesResponse {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub globs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub languages: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<usize>,
    pub limit: usize,
    pub sort: String,
    pub scanned_files: usize,
    pub total_matches: usize,
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
    pub results: Vec<LocFileResult>,
}

#[derive(Debug, Serialize)]
pub struct LocFileResult {
    pub path: String,
    pub language: String,
    pub lines: usize,
    pub code: usize,
    pub blank: usize,
    pub comment: usize,
}

#[derive(Debug, Serialize)]
pub struct FilesResponse {
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub groups: Vec<FilesGroup>,
    /// Hint emitted alongside `truncated: true` ~ tells the agent how to refine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FilesGroup {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    pub total: usize,
    pub shown: usize,
    pub first: Vec<String>,
    pub last: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct LangsResponse {
    pub languages: Vec<LangSummary>,
}

#[derive(Debug, Serialize)]
pub struct LangSummary {
    pub language: String,
    pub files: usize,
    /// Total physical lines (cloc-style: counts a final non-newline line).
    pub lines: usize,
    pub blank: usize,
    pub comment: usize,
    /// `lines - blank - comment`. Pre-computed so callers do not have to.
    pub code: usize,
    pub parseable: bool,
}

// ~~ Framework support (Next.js, etc.) ~~

#[derive(Debug, Serialize)]
pub struct NextInfoResponse {
    pub framework: &'static str,
    pub detected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub router: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub src_layout: bool,
    pub root: String,
}

#[derive(Debug, Serialize)]
pub struct NextRoutesResponse {
    pub framework: &'static str,
    pub root: String,
    pub routes: Vec<NextRoute>,
}

#[derive(Debug, Serialize)]
pub struct NextRoute {
    pub pattern: String,
    pub file: String,
    pub kind: &'static str,
    pub router: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub methods: Option<Vec<String>>,
    #[serde(skip_serializing_if = "is_false")]
    pub advanced: bool,
}

#[derive(Debug, Serialize)]
pub struct NextLayoutsResponse {
    pub framework: &'static str,
    pub root: String,
    pub layouts: Vec<NextLayout>,
}

#[derive(Debug, Serialize)]
pub struct NextLayout {
    pub kind: &'static str,
    pub file: String,
    pub scope: String,
}

#[derive(Debug, Serialize)]
pub struct NextServerActionsResponse {
    pub framework: &'static str,
    pub root: String,
    pub actions: Vec<NextServerAction>,
}

#[derive(Debug, Serialize)]
pub struct NextServerAction {
    pub file: String,
    pub name: String,
    pub scope: &'static str,
}

#[derive(Debug, Serialize)]
pub struct AgentPromptResponse {
    pub action: String,
    pub agent: String,
    pub changed: bool,
    pub status: String,
    pub paths: Vec<String>,
}

// ~~ Diff (uncommitted-change review) ~~

#[derive(Debug, Serialize)]
pub struct DiffOverviewResponse {
    /// Common path prefix stripped from `files[].path` (matches find/search).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    pub files: Vec<DiffFileSummary>,
    /// Comparing against this ref. Omitted when default `HEAD`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub against: Option<String>,
    /// "staged" or "unstaged" when `--staged`/`--unstaged` was passed.
    /// Omitted (empty) for the default combined view.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub scope: String,
    /// True when no diff entries and no untracked files were found ~ lets the
    /// consumer short-circuit a pre-commit review.
    #[serde(skip_serializing_if = "is_false")]
    pub clean: bool,
    /// Hint emitted when the mimi repo root is a subdir of a larger git
    /// toplevel and changes outside it were silently filtered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DiffFileSummary {
    pub path: String,
    /// One of: "M", "A", "D", "R", "C", "T", "?" (untracked).
    pub status: String,
    /// Pre-rename path when `status == "R"` or `status == "C"` AND both
    /// endpoints fall inside the mimi repo subtree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// Text-rendering hint: true when `old_path` had the overview `prefix`
    /// stripped and needs it restored for display.
    #[serde(skip_serializing)]
    pub old_path_needs_prefix: bool,
    /// Lines added (numstat for tracked files, line count for untracked text
    /// files). Omitted for binary files (numstat returns `-`), untracked
    /// files that are binary / non-UTF-8 / oversize / unreadable, and
    /// cross-subtree-rename synthesized entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<usize>,
    /// Set in the default combined scope when this file has staged content.
    #[serde(skip_serializing_if = "is_false")]
    pub staged: bool,
    /// Set in the default combined scope when this file has unstaged content.
    #[serde(skip_serializing_if = "is_false")]
    pub unstaged: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub binary: bool,
    /// Per-file context. Currently used to flag cross-subtree renames that
    /// were synthesized into A/D from this subtree's perspective ~ the note
    /// names the toplevel-relative path of the other endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DiffFileResponse {
    pub path: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// Aggregate counts for the file (mirrors numstat).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<usize>,
    /// Language label for the file's working-tree side (or HEAD-side for
    /// deletions). Useful for the consumer to know whether `symbol`/`spans`
    /// will be populated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Set when `--raw` was passed: the unified diff text verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// Set when `--raw` was NOT passed: structured per-hunk data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<DiffHunk>>,
    /// Hint emitted when the diff was suppressed (size cap, submodule, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub binary: bool,
}

#[derive(Debug, Serialize)]
pub struct DiffMultiFileResponse {
    pub files: Vec<DiffFileResponse>,
}

#[derive(Debug, Serialize, Clone)]
pub struct DiffSummaryResponse {
    pub files: Vec<DiffSummaryFile>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub groups: Vec<DiffSummaryGroup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub against: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub scope: String,
    #[serde(skip_serializing_if = "is_false")]
    pub commit: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub clean: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct DiffSummaryFile {
    pub path: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub symbols: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub staged: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub unstaged: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub binary: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub more_symbols: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct DiffSummaryGroup {
    pub path: String,
    pub file_count: usize,
    pub added: usize,
    pub removed: usize,
    pub files: Vec<DiffSummaryFile>,
}

#[derive(Debug, Serialize)]
pub struct DiffPathsResponse {
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub against: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub scope: String,
    #[serde(skip_serializing_if = "is_false")]
    pub clean: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DiffHunk {
    /// Old-side line range [start, end_inclusive].
    pub old_lines: [usize; 2],
    /// New-side line range [start, end_inclusive]. For pure deletions this is
    /// a 1-line anchor at the deletion point.
    pub new_lines: [usize; 2],
    pub added: usize,
    pub removed: usize,
    /// Innermost enclosing symbol's qualname. Only set for parseable files
    /// where the symbol parser found an overlap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Innermost enclosing symbol's kind. Same emission rules as `symbol`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Other overlapping symbols (qualnames). Empty/absent in the typical
    /// single-symbol case.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub spans: Vec<String>,
    /// Short first changed line. Emitted only when `diff --snippet` is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Hunk body (line-prefixed `+`/`-`/space). Suppressed only via the
    /// per-file size-cap fallback; in that case `note` on the file response
    /// explains.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
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
