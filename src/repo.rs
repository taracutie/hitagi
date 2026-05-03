use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
};

use ignore::WalkBuilder;

use crate::error::{AppError, AppResult};

const AMBIGUOUS_DISPLAY_CAP: usize = 10;
const AMBIGUOUS_COLLECT_CAP: usize = 50;

#[derive(Debug, Clone)]
pub struct ResolvedPath {
    pub repo_root: PathBuf,
    pub relative_path: String,
    pub full_path: PathBuf,
}

pub struct RepoRoot {
    root: PathBuf,
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
        Self { root }
    }

    pub fn resolve_file(&self, relative_path: &str) -> AppResult<ResolvedPath> {
        resolve_path(&self.root, relative_path, PathKind::FileOnly)
    }

    fn resolve_search_path(&self, relative_path: &str) -> AppResult<ResolvedPath> {
        resolve_path(&self.root, relative_path, PathKind::FileOrDir)
    }

    pub fn collect_search_files(&self, paths: &[String]) -> AppResult<Vec<ResolvedPath>> {
        let mut files = Vec::new();
        let mut seen = HashSet::new();

        let start_paths: Vec<PathBuf> = if paths.is_empty() {
            vec![self.root.clone()]
        } else {
            paths
                .iter()
                .map(|p| self.resolve_search_path(p).map(|r| r.full_path))
                .collect::<AppResult<Vec<_>>>()?
        };

        for start in &start_paths {
            let walker = WalkBuilder::new(start)
                .hidden(true)
                .git_ignore(true)
                .git_global(false)
                .git_exclude(true)
                .follow_links(false)
                .build();

            for entry in walker.flatten() {
                if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                    continue;
                }

                let path = entry.into_path();
                let relative_path = match relative_path_string(&self.root, &path) {
                    Ok(rp) => rp,
                    Err(_) => continue,
                };

                if seen.insert(relative_path.clone()) {
                    files.push(ResolvedPath {
                        repo_root: self.root.clone(),
                        relative_path,
                        full_path: path,
                    });
                }
            }
        }

        Ok(files)
    }
}

fn resolve_path(repo_root: &Path, relative_path: &str, kind: PathKind) -> AppResult<ResolvedPath> {
    validate_requested_path(relative_path)?;

    match resolve_exact_path(repo_root, relative_path) {
        Ok(resolved) => Ok(resolved),
        Err(AppError::NotFound(_)) => resolve_path_by_suffix(repo_root, relative_path, kind),
        Err(error) => Err(error),
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

fn resolve_path_by_suffix(
    repo_root: &Path,
    relative_path: &str,
    kind: PathKind,
) -> AppResult<ResolvedPath> {
    let requested_components = requested_components(relative_path);
    let mut matches = Vec::new();
    let mut matches_truncated = false;

    let walker = WalkBuilder::new(repo_root)
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false)
        .build();

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

        if path_ends_with_components(&candidate_relative, &requested_components) {
            if matches.len() == AMBIGUOUS_COLLECT_CAP {
                matches_truncated = true;
                break;
            }
            matches.push(candidate_relative);
        }
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

    resolved_path(repo_root, &repo_root.join(&matches[0]), relative_path)
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
        repo_root: repo_root.to_path_buf(),
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
    fn rejects_ambiguous_suffix_matches_in_ignored_paths() {
        let root = std::env::temp_dir().join(format!(
            "hitagi-ignored-ambiguous-paths-{}",
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
        let error = registry.resolve_file("src/config.txt").unwrap_err();
        let message = error.to_string();

        assert!(message.contains("path is ambiguous: src/config.txt"));
        assert!(message.contains("ignored/src/config.txt"));
        assert!(message.contains("visible/src/config.txt"));

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
