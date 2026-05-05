//! Adapter over `RepoRoot::collect_search_files` ~ produces the file list
//! that the index builder will chunk + tokenize + embed.
//!
//! Reuses hitagi's gitignore-aware walker (`repo.rs`), exclude-pattern
//! infrastructure (`commands::build_exclude_set`/`apply_excludes`), and
//! language detection. Files whose extension we don't recognise still get
//! included with `language = None`; the pack-only chunker will skip them.

use std::fs::Metadata;

use globset::GlobSet;

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
    let raw = repo.collect_search_files(&opts.paths)?;
    let filtered = match exclude_set {
        Some(set) => raw
            .into_iter()
            .filter(|file| !set.is_match(&file.relative_path))
            .collect::<Vec<_>>(),
        None => raw,
    };

    let mut out = Vec::with_capacity(filtered.len());
    for resolved in filtered {
        let metadata = match std::fs::metadata(&resolved.full_path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }
        if metadata.len() > MAX_INDEX_FILE_BYTES {
            continue;
        }
        let language = Language::detect(&resolved.full_path).ok();
        out.push(WalkedFile {
            resolved,
            metadata,
            language,
        });
    }
    Ok(out)
}
