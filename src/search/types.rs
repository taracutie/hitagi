//! Cross-module data types for the ranked-search subsystem.
//!
//! `IndexedChunk` is the unit of retrieval ~ a slice of a source file with
//! enough metadata to render a `path:start-end` location and a language
//! label. `FileSignature` snapshots are written to the persisted index so a
//! later walk can decide "rebuild vs reuse" in one comparison instead of
//! re-stating every file individually.

use serde::{Deserialize, Serialize};

/// One indexed chunk of source. Lives in a `Vec<IndexedChunk>` whose index
/// position IS the chunk id; both BM25 postings and dense embeddings refer
/// back to that id, so chunk-vector ordering must stay stable across the
/// life of one index payload.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct IndexedChunk {
    pub content: String,
    /// Repo-relative, forward-slash-joined.
    pub file_path: String,
    /// 1-indexed inclusive line range.
    pub start_line: usize,
    /// 1-indexed inclusive line range.
    pub end_line: usize,
    /// Stable language label (`"rust"`, `"go"`, ...). `None` for files whose
    /// extension we don't recognise; chunker uses line-based fallback.
    pub language: Option<String>,
}

/// Per-file fingerprint for cache invalidation. Mismatch on any field for
/// any walked file → discard the persisted indexes and rebuild.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileSignature {
    pub rel_path: String,
    pub len: u64,
    pub mtime_ns: i128,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum SearchMode {
    Hybrid,
    Bm25,
    Semantic,
}

impl SearchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hybrid => "hybrid",
            Self::Bm25 => "bm25",
            Self::Semantic => "semantic",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RankedHit {
    pub chunk: IndexedChunk,
    pub score: f32,
    pub source: SearchMode,
}

#[derive(Clone, Debug, Default)]
pub struct SearchStats {
    pub elapsed_ms: u128,
}
