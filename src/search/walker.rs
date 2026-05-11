//! Adapter over `RepoRoot::collect_search_files` ~ produces the file list
//! that the index builder will chunk + tokenize + embed.
//!
//! Reuses hitagi's gitignore-aware walker (`repo.rs`), exclude-pattern
//! infrastructure (`commands::build_exclude_set`/`apply_excludes`), and
//! language detection. Files whose extension we don't recognise still get
//! included with `language = None`; the pack-only chunker will skip them.

use std::fs::Metadata;

use globset::GlobSet;
use rayon::prelude::*;

use crate::error::AppResult;
use crate::lang::Language;
use crate::repo::{RepoRoot, ResolvedPath};

/// Hard cap on per-file size for indexing. Matches `MAX_FILE_BYTES`
/// (`commands.rs:30`) to keep search and outline/symbol/find consistent.
const MAX_INDEX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct WalkedFile {
    pub resolved: ResolvedPath,
    pub metadata: Metadata,
    pub language: Option<Language>,
}

#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    pub paths: Vec<String>,
}

pub fn walk_for_index(
    repo: &RepoRoot,
    opts: &WalkOptions,
    exclude_set: Option<&GlobSet>,
) -> AppResult<Vec<WalkedFile>> {
    // Single-pass: the walk worker captures metadata alongside the
    // resolved path so we don't fan out a second rayon par-iter just to
    // re-stat each entry. The previous double-stat path is preserved on
    // `collect_search_files` for callers that don't need metadata
    // (`find`, `langs`).
    //
    // Language detection is deferred ~ the warm-cache hit path
    // (signatures match, return persisted payload) never consults
    // `WalkedFile.language`, so the per-file extension lookup is pure
    // waste there. The rebuild path materializes them on demand via
    // `ensure_languages_resolved` before it actually chunks.
    let raw = repo.collect_search_files_with_metadata(&opts.paths)?;
    let walked: Vec<WalkedFile> = raw
        .into_iter()
        .filter_map(|(resolved, metadata)| {
            if let Some(set) = exclude_set {
                if set.is_match(&resolved.relative_path) {
                    return None;
                }
            }
            if !metadata.is_file() {
                return None;
            }
            if metadata.len() > MAX_INDEX_FILE_BYTES {
                return None;
            }
            Some(WalkedFile {
                resolved,
                metadata,
                language: None,
            })
        })
        .collect();
    Ok(walked)
}

/// Fill in `WalkedFile.language` for every file in-place. Used by the
/// rebuild path right before chunking ~ runs the same extension+filename
/// match the walker used to do, but only when we actually need it. The
/// warm-cache hit path skips this entirely.
pub fn ensure_languages_resolved(files: &mut [WalkedFile]) {
    files.par_iter_mut().for_each(|file| {
        if file.language.is_none() {
            file.language = Language::detect(&file.resolved.full_path).ok();
        }
    });
}
