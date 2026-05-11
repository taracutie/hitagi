use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use ignore::{WalkBuilder, WalkState};

use crate::error::{AppError, AppResult};

const AMBIGUOUS_DISPLAY_CAP: usize = 10;
const AMBIGUOUS_COLLECT_CAP: usize = 50;

#[derive(Debug, Clone)]
pub struct ResolvedPath {
    pub relative_path: String,
    pub full_path: PathBuf,
}

pub struct RepoRoot {
    root: PathBuf,
    /// Lazy gitignore-respected file/dir index used by suffix-resolution.
    /// Built on first miss and reused for the rest of the command.
    /// `OnceLock` adds an atomic load on read vs `OnceCell`'s plain pointer
    /// check; in exchange `RepoRoot` is `Sync`, so the search command can
    /// hand `&RepoRoot` to a rayon worker that runs the sparse-index walk
    /// in parallel with encoder loading.
    file_index: OnceLock<RepoFileIndex>,
}

/// Visible (gitignore-respected) repo paths held in memory so suffix-resolution
/// can scan them instead of walking the FS. `dirs` covers the `FileOrDir`
/// lookup case (e.g. `find foo src-tauri/src`) and is built lazily ~ FileOnly
/// resolves (the common case) skip the dir-derivation pass entirely. The
/// ignored-walk fallback only runs when the visible index is empty (a tmpfs
/// fixture or a repo where gitignore filtered everything).
struct RepoFileIndex {
    files: Vec<String>,
    dirs: OnceLock<HashSet<String>>,
}

impl RepoFileIndex {
    fn from_files(files: Vec<String>) -> Self {
        Self {
            files,
            dirs: OnceLock::new(),
        }
    }

    fn dirs(&self) -> &HashSet<String> {
        self.dirs.get_or_init(|| {
            let mut dirs: HashSet<String> = HashSet::new();
            for path in &self.files {
                let parts: Vec<&str> = path.split('/').collect();
                for end in 1..parts.len() {
                    dirs.insert(parts[..end].join("/"));
                }
            }
            dirs
        })
    }

    fn scan_for_suffix(
        &self,
        kind: PathKind,
        requested_components: &[String],
    ) -> (Vec<String>, bool) {
        let mut matches: Vec<String> = Vec::new();
        let mut truncated = false;
        for path in &self.files {
            if path_ends_with_components(path, requested_components) {
                if matches.len() == AMBIGUOUS_COLLECT_CAP {
                    truncated = true;
                    break;
                }
                matches.push(path.clone());
            }
        }
        if !truncated && matches!(kind, PathKind::FileOrDir) {
            for path in self.dirs() {
                if path_ends_with_components(path, requested_components) {
                    if matches.len() == AMBIGUOUS_COLLECT_CAP {
                        truncated = true;
                        break;
                    }
                    matches.push(path.clone());
                }
            }
        }
        (matches, truncated)
    }

    /// Skip the ignored-walk fallback whenever the visible index has any
    /// content. The fallback only ever surfaced ignored files via partial-
    /// suffix lookup; the README-documented behavior is "unique repo-internal
    /// suffix" which is a visible concern. The empty-visible case (test
    /// fixtures, gitignore-everything repos) still walks because we have no
    /// other way to find anything.
    fn skip_ignored_fallback(&self) -> bool {
        !self.files.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PathKind {
    FileOnly,
    FileOrDir,
}

impl PathKind {
    fn matches(self, file_type: std::fs::FileType) -> bool {
        match self {
            Self::FileOnly => file_type.is_file(),
            Self::FileOrDir => file_type.is_file() || file_type.is_dir(),
        }
    }
}

impl RepoRoot {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            file_index: OnceLock::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn resolve_file(&self, relative_path: &str) -> AppResult<ResolvedPath> {
        self.resolve_path(relative_path, PathKind::FileOnly)
    }

    pub fn resolve_file_or_dir(&self, relative_path: &str) -> AppResult<ResolvedPath> {
        self.resolve_path(relative_path, PathKind::FileOrDir)
    }

    fn file_index(&self) -> &RepoFileIndex {
        self.file_index
            .get_or_init(|| RepoFileIndex::from_files(walk_visible_files(&self.root)))
    }

    fn resolve_path(&self, relative_path: &str, kind: PathKind) -> AppResult<ResolvedPath> {
        validate_requested_path(relative_path)?;
        match resolve_exact_path(&self.root, relative_path) {
            Ok(resolved) => Ok(resolved),
            Err(AppError::NotFound(_)) => self.resolve_path_by_suffix(relative_path, kind),
            Err(error) => Err(error),
        }
    }

    fn resolve_path_by_suffix(
        &self,
        relative_path: &str,
        kind: PathKind,
    ) -> AppResult<ResolvedPath> {
        let requested_components = requested_components(relative_path);
        let index = self.file_index();

        // Pass 1: in-memory scan over the gitignore-respected file/dir index.
        // Was a full FS walk via `ignore::WalkBuilder`. Constant-time after
        // the index build amortizes on the first call.
        let (mut matches, mut matches_truncated) =
            index.scan_for_suffix(kind, &requested_components);

        // Pass 2: ignored fallback. Skipped whenever the visible index has
        // anything in it ~ a non-empty repo with no visible suffix match
        // means the user typo'd. The empty-visible edge case still walks so
        // legit ignored content remains addressable there.
        if matches.is_empty() && !index.skip_ignored_fallback() {
            let (ignored_matches, ignored_truncated) =
                walk_for_suffix(&self.root, kind, &requested_components, false);
            matches = ignored_matches;
            matches_truncated = ignored_truncated;
        }

        if matches.is_empty() {
            return Err(AppError::not_found(format!(
                "path not found: {relative_path}"
            )));
        }

        matches.sort();

        if matches.len() > 1 {
            return Err(AppError::bad_request(ambiguous_path_message(
                relative_path,
                &matches,
                matches_truncated,
            )));
        }

        resolved_path(&self.root, &self.root.join(&matches[0]), relative_path)
    }

    /// Validate `relative_path` for traversal/escape and return a normalized
    /// repo-relative form. Unlike `resolve_file`, this does NOT require the
    /// file to exist on disk ~ used by `hitagi diff` so deleted files (and
    /// rename old paths) can still be addressed.
    pub fn validate_diff_path(&self, relative_path: &str) -> AppResult<String> {
        validate_requested_path(relative_path)?;
        let normalized = normalize_repo_relative(relative_path);
        if normalized.is_empty() {
            return Err(AppError::bad_request(format!(
                "path must point inside the repo: {relative_path}"
            )));
        }
        Ok(normalized)
    }

    fn resolve_search_path(&self, relative_path: &str) -> AppResult<ResolvedPath> {
        self.resolve_path(relative_path, PathKind::FileOrDir)
    }

    /// Like `collect_search_files` but also captures the entry's `Metadata`
    /// in the same walk-worker callback. Callers that need both the path
    /// list and per-file size/mtime (the `search` index walk) save a full
    /// second pass of stat syscalls ~ on the web repo that's ~5 ms shaved
    /// off the warm `walk_for_index` head, where the stat phase used to
    /// be a separate rayon par-iter immediately after the walk completed.
    pub fn collect_search_files_with_metadata(
        &self,
        paths: &[String],
    ) -> AppResult<Vec<(ResolvedPath, std::fs::Metadata)>> {
        let start_paths: Vec<PathBuf> = if paths.is_empty() {
            vec![self.root.clone()]
        } else {
            paths
                .iter()
                .map(|p| self.resolve_search_path(p).map(|r| r.full_path))
                .collect::<AppResult<Vec<_>>>()?
        };

        let buckets: Mutex<Vec<(ResolvedPath, std::fs::Metadata)>> = Mutex::new(Vec::new());

        struct WorkerSink<'a> {
            local: Vec<(ResolvedPath, std::fs::Metadata)>,
            shared: &'a Mutex<Vec<(ResolvedPath, std::fs::Metadata)>>,
        }
        impl<'a> WorkerSink<'a> {
            fn flush(&mut self) {
                if self.local.is_empty() {
                    return;
                }
                if let Ok(mut shared) = self.shared.lock() {
                    if shared.is_empty() {
                        std::mem::swap(&mut self.local, &mut *shared);
                    } else {
                        shared.append(&mut self.local);
                    }
                }
            }
        }
        impl<'a> Drop for WorkerSink<'a> {
            fn drop(&mut self) {
                self.flush();
            }
        }

        for start in &start_paths {
            let mut builder = WalkBuilder::new(start);
            builder
                .hidden(true)
                .git_ignore(true)
                .git_global(false)
                .git_exclude(true)
                .follow_links(false);
            let walker = builder.build_parallel();
            walker.run(|| {
                let root = self.root.clone();
                let mut sink = WorkerSink {
                    local: Vec::new(),
                    shared: &buckets,
                };
                Box::new(move |entry| {
                    if let Ok(entry) = entry {
                        if entry.file_type().is_some_and(|ft| ft.is_file()) {
                            // `entry.metadata()` on the `ignore` crate's
                            // `DirEntry` issues a single stat (the dirent's
                            // d_type covers is_file but not size/mtime, so
                            // a stat is unavoidable; doing it here folds
                            // into the walk-worker pool instead of paying
                            // for a rayon-par fan-out afterward).
                            if let Ok(metadata) = entry.metadata() {
                                let path = entry.into_path();
                                if let Ok(relative_path) = relative_path_string(&root, &path) {
                                    sink.local.push((
                                        ResolvedPath {
                                            relative_path,
                                            full_path: path,
                                        },
                                        metadata,
                                    ));
                                    if sink.local.len() >= 256 {
                                        sink.flush();
                                    }
                                }
                            }
                        }
                    }
                    WalkState::Continue
                })
            });
        }

        let mut files = buckets.into_inner().unwrap_or_default();
        if start_paths.len() > 1 {
            let mut seen: HashSet<String> = HashSet::with_capacity(files.len());
            files.retain(|(rp, _)| seen.insert(rp.relative_path.clone()));
        }
        Ok(files)
    }

    pub fn collect_search_files(&self, paths: &[String]) -> AppResult<Vec<ResolvedPath>> {
        let start_paths: Vec<PathBuf> = if paths.is_empty() {
            vec![self.root.clone()]
        } else {
            paths
                .iter()
                .map(|p| self.resolve_search_path(p).map(|r| r.full_path))
                .collect::<AppResult<Vec<_>>>()?
        };

        // Parallel directory walk: WalkBuilder's single-threaded `.build()`
        // reads directories sequentially. On a 2k-file repo this dominates
        // the warm `ensure_sparse` head (~30 ms of 40 ms). build_parallel
        // shards traversal across N worker threads; each one fills a local
        // buffer that's flushed on Drop so we only acquire the shared Mutex
        // a handful of times per worker, not once per file.
        let buckets: Mutex<Vec<ResolvedPath>> = Mutex::new(Vec::new());

        struct WorkerSink<'a> {
            local: Vec<ResolvedPath>,
            shared: &'a Mutex<Vec<ResolvedPath>>,
        }
        impl<'a> WorkerSink<'a> {
            fn flush(&mut self) {
                if self.local.is_empty() {
                    return;
                }
                if let Ok(mut shared) = self.shared.lock() {
                    if shared.is_empty() {
                        std::mem::swap(&mut self.local, &mut *shared);
                    } else {
                        shared.append(&mut self.local);
                    }
                }
            }
        }
        impl<'a> Drop for WorkerSink<'a> {
            fn drop(&mut self) {
                self.flush();
            }
        }

        for start in &start_paths {
            let mut builder = WalkBuilder::new(start);
            builder
                .hidden(true)
                .git_ignore(true)
                .git_global(false)
                .git_exclude(true)
                .follow_links(false);
            let walker = builder.build_parallel();
            walker.run(|| {
                let root = self.root.clone();
                let mut sink = WorkerSink {
                    local: Vec::new(),
                    shared: &buckets,
                };
                Box::new(move |entry| {
                    if let Ok(entry) = entry {
                        if entry.file_type().is_some_and(|ft| ft.is_file()) {
                            let path = entry.into_path();
                            if let Ok(relative_path) = relative_path_string(&root, &path) {
                                sink.local.push(ResolvedPath {
                                    relative_path,
                                    full_path: path,
                                });
                                if sink.local.len() >= 256 {
                                    sink.flush();
                                }
                            }
                        }
                    }
                    WalkState::Continue
                })
            });
        }

        let mut files = buckets.into_inner().unwrap_or_default();

        // Multiple start paths can yield duplicates (overlapping subtrees);
        // dedup while preserving discovery order. A single start path
        // (the no-`paths` default for `search` / `find` / `langs`) can't
        // emit duplicates from one walk, so skip the 2.5k-clone HashSet
        // populate in that case.
        if start_paths.len() > 1 {
            let mut seen: HashSet<String> = HashSet::with_capacity(files.len());
            files.retain(|rp| seen.insert(rp.relative_path.clone()));
        }
        Ok(files)
    }
}

fn validate_requested_path(relative_path: &str) -> AppResult<()> {
    if relative_path.trim().is_empty() {
        return Err(AppError::bad_request("path must not be empty"));
    }

    let requested_path = Path::new(relative_path);
    if requested_path.is_absolute() {
        return Err(AppError::bad_request(
            "path must be relative to the repo root",
        ));
    }

    if requested_path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(AppError::bad_request(format!(
            "path escapes the repo root: {relative_path}"
        )));
    }

    Ok(())
}

fn resolve_exact_path(repo_root: &Path, relative_path: &str) -> AppResult<ResolvedPath> {
    let joined = repo_root.join(Path::new(relative_path));
    resolved_path(repo_root, &joined, relative_path)
}

/// Collect every gitignore-respected file path in the repo (relative to
/// `repo_root`, `/`-joined). Same WalkBuilder flag set as
/// `collect_search_files`; lives here to back the `RepoFileIndex` cache.
fn walk_visible_files(repo_root: &Path) -> Vec<String> {
    let walker = WalkBuilder::new(repo_root)
        .hidden(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .follow_links(false)
        .build();

    let mut files = Vec::new();
    for entry in walker.flatten() {
        if !entry.file_type().map_or(false, |ft| ft.is_file()) {
            continue;
        }
        let full_path = entry.into_path();
        if let Ok(relative) = relative_path_string(repo_root, &full_path) {
            files.push(relative);
        }
    }
    files
}

/// Walk `repo_root` (un-respected: includes ignored content) and collect
/// repo-relative paths whose component suffix matches `requested_components`.
/// Used as the fallback when the in-memory visible index is empty. The bool
/// result is `true` when the AMBIGUOUS_COLLECT_CAP was hit.
///
/// The original two-pass `walk_for_suffix(..., true)` then `(..., false)`
/// path was retired in favor of the in-memory `RepoFileIndex` for the
/// `respect_ignores=true` half ~ this function now handles only the
/// fallback. The flag stays for symmetry/future use.
/// top-level commands' walker (collect_search_files). `false` walks
/// everything for fallback resolution. The bool result is true when the
/// AMBIGUOUS_COLLECT_CAP was hit.
fn walk_for_suffix(
    repo_root: &Path,
    kind: PathKind,
    requested_components: &[String],
    respect_ignores: bool,
) -> (Vec<String>, bool) {
    // .hidden(true) SKIPS dotfiles (default), .hidden(false) walks them.
    // Original suffix resolver walked hidden + ignored; respected mode now
    // mirrors collect_search_files (skip hidden, respect ignores).
    let walker = WalkBuilder::new(repo_root)
        .hidden(respect_ignores)
        .git_ignore(respect_ignores)
        .git_global(false)
        .git_exclude(respect_ignores)
        .follow_links(false)
        .build();

    let mut matches = Vec::new();
    let mut matches_truncated = false;

    for entry in walker.flatten() {
        let Some(file_type) = entry.file_type() else {
            continue;
        };

        if !kind.matches(file_type) {
            continue;
        }

        let full_path = entry.into_path();
        let candidate_relative = match relative_path_string(repo_root, &full_path) {
            Ok(value) => value,
            Err(_) => continue,
        };

        if path_ends_with_components(&candidate_relative, requested_components) {
            if matches.len() == AMBIGUOUS_COLLECT_CAP {
                matches_truncated = true;
                break;
            }
            matches.push(candidate_relative);
        }
    }

    (matches, matches_truncated)
}

fn resolved_path(
    repo_root: &Path,
    full_path: &Path,
    relative_path: &str,
) -> AppResult<ResolvedPath> {
    let canonical = std::fs::canonicalize(full_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AppError::not_found(format!("path not found: {relative_path}"))
        } else {
            AppError::from(error)
        }
    })?;

    if !canonical.starts_with(repo_root) {
        return Err(AppError::bad_request(format!(
            "path resolves outside the repo root: {relative_path}"
        )));
    }

    let relative = relative_path_string(repo_root, &canonical)?;

    Ok(ResolvedPath {
        relative_path: relative,
        full_path: canonical,
    })
}

fn requested_components(relative_path: &str) -> Vec<String> {
    Path::new(relative_path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

pub(crate) fn normalize_repo_relative(relative_path: &str) -> String {
    Path::new(relative_path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn path_components_match_suffix(
    candidate_relative: &str,
    requested_components: &[String],
) -> bool {
    path_ends_with_components(candidate_relative, requested_components)
}

pub(crate) fn parse_requested_components(relative_path: &str) -> Vec<String> {
    requested_components(relative_path)
}

fn path_ends_with_components(candidate_relative: &str, requested_components: &[String]) -> bool {
    let candidate_components: Vec<&str> = candidate_relative.split('/').collect();
    if candidate_components.len() < requested_components.len() {
        return false;
    }

    candidate_components[candidate_components.len() - requested_components.len()..]
        .iter()
        .copied()
        .eq(requested_components.iter().map(String::as_str))
}

fn ambiguous_path_message(
    relative_path: &str,
    candidates: &[String],
    candidates_truncated: bool,
) -> String {
    let shown: Vec<&str> = candidates
        .iter()
        .take(AMBIGUOUS_DISPLAY_CAP)
        .map(String::as_str)
        .collect();
    let remaining = candidates.len().saturating_sub(shown.len());

    if candidates_truncated {
        format!(
            "path is ambiguous: {relative_path} matched more than {AMBIGUOUS_COLLECT_CAP} repo paths; showing {} sampled paths: {}",
            shown.len(),
            shown.join(", ")
        )
    } else if remaining == 0 {
        format!(
            "path is ambiguous: {relative_path} matched multiple repo paths: {}",
            shown.join(", ")
        )
    } else {
        format!(
            "path is ambiguous: {relative_path} matched multiple repo paths: {} (+{remaining} more)",
            shown.join(", ")
        )
    }
}

fn relative_path_string(repo_root: &Path, full_path: &Path) -> AppResult<String> {
    let relative = full_path.strip_prefix(repo_root).map_err(|_| {
        AppError::bad_request(format!(
            "path resolves outside the repo root: {}",
            full_path.display()
        ))
    })?;

    let mut parts = Vec::new();
    for component in relative.components() {
        if let Component::Normal(value) = component {
            parts.push(value.to_string_lossy().into_owned());
        }
    }

    if parts.is_empty() {
        return Err(AppError::bad_request(
            "path must point to a file or directory inside the repo",
        ));
    }

    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::RepoRoot;

    fn fixture_root(name: &str) -> PathBuf {
        std::fs::canonicalize(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join(name),
        )
        .unwrap()
    }

    #[test]
    fn rejects_path_traversal() {
        let registry = RepoRoot::new(fixture_root("sample_repo"));
        let error = registry.resolve_file("../outside.txt").unwrap_err();
        assert_eq!(
            error.to_string(),
            "path escapes the repo root: ../outside.txt"
        );
    }

    #[test]
    fn rejects_absolute_paths() {
        let registry = RepoRoot::new(fixture_root("sample_repo"));
        let error = registry
            .resolve_file("/tmp/definitely-not-inside-the-repo")
            .unwrap_err();
        assert_eq!(error.to_string(), "path must be relative to the repo root");
    }

    #[test]
    fn exact_repo_relative_match_wins_before_suffix_lookup() {
        let registry = RepoRoot::new(fixture_root("sample_repo"));
        let resolved = registry.resolve_file("src/auth.ts").unwrap();
        assert_eq!(resolved.relative_path, "src/auth.ts");
    }

    #[test]
    fn resolves_unique_file_suffix_inside_repo_root() {
        let registry = RepoRoot::new(fixture_root("sample_repo"));
        let resolved = registry.resolve_file("src-tauri/src/main.rs").unwrap();
        assert_eq!(resolved.relative_path, "apps/desktop/src-tauri/src/main.rs");
    }

    #[test]
    fn resolves_unique_directory_suffix_for_search_paths() {
        let registry = RepoRoot::new(fixture_root("sample_repo"));
        let files = registry
            .collect_search_files(&["src-tauri/src".to_string()])
            .unwrap();
        let relative_paths: Vec<String> =
            files.into_iter().map(|file| file.relative_path).collect();

        assert_eq!(
            relative_paths,
            vec!["apps/desktop/src-tauri/src/main.rs".to_string()]
        );
    }

    #[test]
    fn rejects_ambiguous_suffix_matches() {
        let registry = RepoRoot::new(fixture_root("sample_repo"));
        let error = registry
            .resolve_file("src/components/Button.tsx")
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "path is ambiguous: src/components/Button.tsx matched multiple repo paths: apps/desktop/src/components/Button.tsx, packages/mobile/src/components/Button.tsx"
        );
    }

    #[test]
    fn suffix_resolution_prefers_gitignore_respected_match() {
        // Two files with the same basename ~ one in an ignored dir, one in a
        // visible dir. Old behavior surfaced both as ambiguity (which left
        // node_modules mirrors polluting the candidate list); new behavior
        // prefers the visible match and resolves uniquely.
        let root = std::env::temp_dir().join(format!(
            "hitagi-suffix-prefers-visible-{}",
            std::process::id()
        ));
        if root.exists() {
            std::fs::remove_dir_all(&root).unwrap();
        }
        std::fs::create_dir_all(root.join("ignored/src")).unwrap();
        std::fs::create_dir_all(root.join("visible/src")).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::write(root.join("ignored/src/config.txt"), "").unwrap();
        std::fs::write(root.join("visible/src/config.txt"), "").unwrap();

        let registry = RepoRoot::new(std::fs::canonicalize(&root).unwrap());
        let resolved = registry.resolve_file("src/config.txt").unwrap();
        assert_eq!(resolved.relative_path, "visible/src/config.txt");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn suffix_resolution_falls_back_to_ignored_when_no_visible_match() {
        // Only an ignored copy exists ~ resolution must still find it so
        // agents can address files that are gitignored on purpose (build
        // outputs, fixtures inside an ignored vendor dir, etc.).
        let root =
            std::env::temp_dir().join(format!("hitagi-suffix-falls-back-{}", std::process::id()));
        if root.exists() {
            std::fs::remove_dir_all(&root).unwrap();
        }
        std::fs::create_dir_all(root.join("ignored/src")).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::write(root.join("ignored/src/config.txt"), "body").unwrap();

        let registry = RepoRoot::new(std::fs::canonicalize(&root).unwrap());
        let resolved = registry.resolve_file("src/config.txt").unwrap();
        assert_eq!(resolved.relative_path, "ignored/src/config.txt");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn suffix_ambiguity_still_surfaces_among_visible_matches() {
        // Two visible matches ~ ambiguity is still real; both surface in the
        // error. (The fallback path doesn't hide real ambiguity.)
        let root = std::env::temp_dir().join(format!(
            "hitagi-suffix-visible-ambiguity-{}",
            std::process::id()
        ));
        if root.exists() {
            std::fs::remove_dir_all(&root).unwrap();
        }
        std::fs::create_dir_all(root.join("alpha/src")).unwrap();
        std::fs::create_dir_all(root.join("beta/src")).unwrap();
        std::fs::write(root.join("alpha/src/config.txt"), "").unwrap();
        std::fs::write(root.join("beta/src/config.txt"), "").unwrap();

        let registry = RepoRoot::new(std::fs::canonicalize(&root).unwrap());
        let error = registry.resolve_file("src/config.txt").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("path is ambiguous: src/config.txt"));
        assert!(message.contains("alpha/src/config.txt"));
        assert!(message.contains("beta/src/config.txt"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reports_truncated_ambiguous_suffix_matches_as_sampled() {
        let root =
            std::env::temp_dir().join(format!("hitagi-ambiguous-paths-{}", std::process::id()));
        if root.exists() {
            std::fs::remove_dir_all(&root).unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();

        for index in 0..=super::AMBIGUOUS_COLLECT_CAP {
            let path = root.join(format!("pkg-{index:03}")).join("index.ts");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }

        let registry = RepoRoot::new(std::fs::canonicalize(&root).unwrap());
        let error = registry.resolve_file("index.ts").unwrap_err();
        let message = error.to_string();

        assert!(message.contains("matched more than 50 repo paths"));
        assert!(message.contains("showing 10 sampled paths"));
        assert!(!message.contains("(+41 more)"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn keeps_not_found_for_missing_suffix_matches() {
        let registry = RepoRoot::new(fixture_root("sample_repo"));
        let error = registry
            .resolve_file("src-tauri/src/missing.rs")
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "path not found: src-tauri/src/missing.rs"
        );
    }

    #[test]
    fn suffix_matching_uses_path_components_not_raw_string_suffix() {
        let registry = RepoRoot::new(fixture_root("sample_repo"));
        let resolved = registry.resolve_file("src-tauri/src/main.rs").unwrap();
        assert_ne!(resolved.relative_path, "tools/foo-src-tauri/src/main.rs");
    }
}
