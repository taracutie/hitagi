// Git subprocess wrappers + pure parsers for `--name-status -z`, `--numstat -z`,
// and unified diff text. This is the only place in hitagi that shells out, and
// the only place that calls `std::process::Command` ~ keep it that way so the
// command-layer code stays pure-Rust and easy to unit-test.
//
// Error mapping: any non-zero `git` exit gets its stderr inspected and turned
// into a typed AppError. "not a git repository", "unknown revision", and
// "ambiguous argument" are user-facing and map to BadRequest; everything else
// is Internal so unexpected git behavior doesn't masquerade as user input.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone)]
pub struct GitRoot {
    /// Canonical absolute path to the git toplevel (parent of .git/).
    pub toplevel: PathBuf,
    /// Hitagi repo root expressed relative to `toplevel`. Empty when the two
    /// coincide. Always uses '/' separators so it composes cleanly with git's
    /// (also '/'-separated) path output.
    pub repo_subdir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameStatusEntry {
    /// First letter of the status field. M, A, D, T, R, C, U, X.
    pub status: char,
    /// Similarity score for R/C entries (e.g. R100 -> 100). None otherwise.
    pub score: Option<u32>,
    /// Post-rename (or sole) path, repo-toplevel-relative.
    pub path: String,
    /// Pre-rename path for R/C entries.
    pub old_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumstatEntry {
    /// `None` for binary files (git emits `-`).
    pub added: Option<usize>,
    pub removed: Option<usize>,
    pub path: String,
    pub old_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawHunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedHunk {
    pub raw: RawHunk,
    /// The hunk body (line-prefixed `+`, `-`, or ` ` lines). Excludes the `@@`
    /// header line and any "\ No newline at end of file" markers.
    pub body: String,
    pub added: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedFileDiff {
    /// Pre-rename / pre-edit path. For pure additions this is None.
    pub old_path: Option<String>,
    /// Post-rename / post-edit path. For pure deletions this is None.
    pub new_path: Option<String>,
    pub binary: bool,
    pub renamed: bool,
    pub new_file: bool,
    pub deleted_file: bool,
    pub hunks: Vec<ParsedHunk>,
    /// Verbatim slice of the unified diff for this file (everything from one
    /// `diff --git` line up to but not including the next).
    pub raw_text: String,
}

// ~~ Subprocess wrappers ~~

enum GitError {
    NotInstalled,
    Io(std::io::Error),
    NonZero { stderr: String },
}

fn raw_git(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd)
        .arg("-c")
        .arg("color.ui=never")
        .arg("--no-pager")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(GitError::NotInstalled),
        Err(e) => return Err(GitError::Io(e)),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(GitError::NonZero { stderr });
    }
    Ok(output.stdout)
}

fn run_git(cwd: &Path, args: &[&str]) -> AppResult<Vec<u8>> {
    match raw_git(cwd, args) {
        Ok(bytes) => Ok(bytes),
        Err(GitError::NotInstalled) => {
            Err(AppError::bad_request("git executable not found on PATH"))
        }
        Err(GitError::Io(e)) => Err(AppError::Io(e)),
        Err(GitError::NonZero { stderr }) => Err(map_git_stderr(&stderr)),
    }
}

fn map_git_stderr(stderr: &str) -> AppError {
    let trimmed = stderr.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("not a git repository") {
        return AppError::bad_request(trimmed.to_string());
    }
    if lower.contains("unknown revision")
        || lower.contains("bad revision")
        || lower.contains("ambiguous argument")
    {
        return AppError::bad_request(format!("invalid ref: {trimmed}"));
    }
    AppError::internal(format!("git failed: {trimmed}"))
}

// ~~ High-level wrappers used by `commands::diff` ~~

pub fn resolve_git_root(repo: &Path) -> AppResult<GitRoot> {
    let bytes = run_git(repo, &["rev-parse", "--show-toplevel"])?;
    let raw = std::str::from_utf8(&bytes)
        .map_err(|_| AppError::internal("git rev-parse --show-toplevel returned non-UTF-8"))?
        .trim();
    if raw.is_empty() {
        return Err(AppError::bad_request(format!(
            "not a git repository: {}",
            repo.display()
        )));
    }
    let toplevel = std::fs::canonicalize(PathBuf::from(raw))?;
    let canonical_repo = std::fs::canonicalize(repo)?;
    let repo_subdir = canonical_repo
        .strip_prefix(&toplevel)
        .ok()
        .map(|p| {
            // ignore::WalkBuilder normalizes to '/' elsewhere; do the same
            // here so subdir-prefix comparisons work regardless of platform.
            p.to_string_lossy().replace('\\', "/")
        })
        .unwrap_or_default();
    Ok(GitRoot {
        toplevel,
        repo_subdir,
    })
}

pub fn validate_ref(reference: &str) -> AppResult<()> {
    if reference.is_empty() {
        return Err(AppError::bad_request("ref must not be empty"));
    }
    if reference.starts_with('-') {
        return Err(AppError::bad_request(format!(
            "invalid ref: {reference} (must not start with `-`)"
        )));
    }
    for ch in reference.chars() {
        if ch == '\0' || ch.is_ascii_control() || ch.is_whitespace() {
            return Err(AppError::bad_request(format!("invalid ref: {reference}")));
        }
    }
    if reference.contains("..") {
        return Err(AppError::bad_request(format!(
            "invalid ref: {reference} (range syntax not allowed)"
        )));
    }
    Ok(())
}

pub fn ref_exists(toplevel: &Path, ref_name: &str) -> bool {
    matches!(
        raw_git(
            toplevel,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{ref_name}^{{commit}}")
            ],
        ),
        Ok(_)
    )
}

pub fn list_untracked(toplevel: &Path) -> AppResult<Vec<String>> {
    let bytes = run_git(
        toplevel,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    Ok(parse_z_paths(&bytes))
}

pub fn name_status(
    toplevel: &Path,
    base_ref: Option<&str>,
    cached: bool,
) -> AppResult<Vec<NameStatusEntry>> {
    let mut args: Vec<String> = vec![
        "diff".into(),
        "--name-status".into(),
        "-z".into(),
        "-M".into(),
    ];
    if cached {
        args.push("--cached".into());
    }
    if let Some(r) = base_ref {
        args.push(r.into());
    }
    args.push("--".into());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let bytes = run_git(toplevel, &arg_refs)?;
    parse_name_status_z(&bytes)
}

pub fn numstat(
    toplevel: &Path,
    base_ref: Option<&str>,
    cached: bool,
) -> AppResult<Vec<NumstatEntry>> {
    let mut args: Vec<String> = vec!["diff".into(), "--numstat".into(), "-z".into(), "-M".into()];
    if cached {
        args.push("--cached".into());
    }
    if let Some(r) = base_ref {
        args.push(r.into());
    }
    args.push("--".into());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let bytes = run_git(toplevel, &arg_refs)?;
    parse_numstat_z(&bytes)
}

pub fn diff_one_file(
    toplevel: &Path,
    base_ref: Option<&str>,
    cached: bool,
    rel_path: &str,
    unified_zero: bool,
) -> AppResult<ParsedFileDiff> {
    let mut args: Vec<String> = vec!["diff".into()];
    if unified_zero {
        args.push("-U0".into());
    }
    args.push("--no-color".into());
    args.push("-M".into());
    // Don't pass --text: it forces text treatment of binary files, which we
    // explicitly want to detect via `Binary files ... differ` in the parser.
    if cached {
        args.push("--cached".into());
    }
    if let Some(r) = base_ref {
        args.push(r.into());
    }
    args.push("--".into());
    args.push(rel_path.into());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let bytes = run_git(toplevel, &arg_refs)?;
    let text = String::from_utf8(bytes).map_err(|_| {
        AppError::InvalidUtf8(format!("git diff produced non-UTF-8 output for {rel_path}"))
    })?;
    let mut diffs = parse_unified_diff(&text)?;
    if diffs.is_empty() {
        return Ok(ParsedFileDiff::default());
    }
    Ok(diffs.remove(0))
}

pub fn diff_many_files(
    toplevel: &Path,
    base_ref: Option<&str>,
    cached: bool,
    rel_paths: &[String],
    unified_zero: bool,
) -> AppResult<Vec<ParsedFileDiff>> {
    if rel_paths.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for chunk in rel_paths.chunks(128) {
        let mut args: Vec<String> = vec!["diff".into()];
        if unified_zero {
            args.push("-U0".into());
        }
        args.push("--no-color".into());
        args.push("-M".into());
        // Don't pass --text: it forces text treatment of binary files, which we
        // explicitly want to detect via `Binary files ... differ` in the parser.
        if cached {
            args.push("--cached".into());
        }
        if let Some(r) = base_ref {
            args.push(r.into());
        }
        args.push("--".into());
        args.extend(chunk.iter().cloned());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let bytes = run_git(toplevel, &arg_refs)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| AppError::InvalidUtf8("git diff produced non-UTF-8 output".into()))?;
        out.extend(parse_unified_diff(&text)?);
    }

    Ok(out)
}

pub fn show_blob(toplevel: &Path, base_ref: &str, rel_path: &str) -> AppResult<Vec<u8>> {
    let pathspec = format!("{base_ref}:{rel_path}");
    run_git(toplevel, &["show", &pathspec])
}

// ~~ Pure parsers (unit-tested without a git binary) ~~

fn parse_z_paths(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

pub fn parse_name_status_z(bytes: &[u8]) -> AppResult<Vec<NameStatusEntry>> {
    // With `-z`, git separates EVERY field with NUL ~ status, then path[s]:
    //   non-rename:    "M\0path\0"
    //   rename/copy:   "R<score>\0old\0new\0"
    let mut entries = Vec::new();
    let mut chunks = bytes.split(|b| *b == 0).filter(|s| !s.is_empty());
    while let Some(chunk) = chunks.next() {
        let status_str = std::str::from_utf8(chunk)
            .map_err(|_| AppError::internal("non-UTF-8 in git --name-status status field"))?;
        let first_char = status_str
            .chars()
            .next()
            .ok_or_else(|| AppError::internal("empty status field in git --name-status output"))?;
        let path_chunk = chunks.next().ok_or_else(|| {
            AppError::internal(format!(
                "name-status record missing path after status `{status_str}`"
            ))
        })?;
        let path = std::str::from_utf8(path_chunk)
            .map_err(|_| AppError::internal("non-UTF-8 in git --name-status path"))?
            .to_string();
        if first_char == 'R' || first_char == 'C' {
            let score = status_str[first_char.len_utf8()..].parse::<u32>().ok();
            let new_chunk = chunks.next().ok_or_else(|| {
                AppError::internal(format!(
                    "rename/copy record `{status_str}` missing destination path"
                ))
            })?;
            let new_path = std::str::from_utf8(new_chunk)
                .map_err(|_| AppError::internal("non-UTF-8 in git --name-status rename path"))?
                .to_string();
            entries.push(NameStatusEntry {
                status: first_char,
                score,
                path: new_path,
                old_path: Some(path),
            });
        } else {
            entries.push(NameStatusEntry {
                status: first_char,
                score: None,
                path,
                old_path: None,
            });
        }
    }
    Ok(entries)
}

pub fn parse_numstat_z(bytes: &[u8]) -> AppResult<Vec<NumstatEntry>> {
    let mut entries = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Skip stray NULs between records (defensive ~ git doesn't emit
        // empty records but be robust to trailing terminators).
        while i < bytes.len() && bytes[i] == 0 {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        // <added>\t
        let added_start = i;
        while i < bytes.len() && bytes[i] != b'\t' {
            i += 1;
        }
        if i >= bytes.len() {
            return Err(AppError::internal(
                "truncated numstat entry (no tab after added)",
            ));
        }
        let added_str = std::str::from_utf8(&bytes[added_start..i])
            .map_err(|_| AppError::internal("non-UTF-8 in numstat added"))?;
        i += 1;

        // <removed>\t
        let removed_start = i;
        while i < bytes.len() && bytes[i] != b'\t' {
            i += 1;
        }
        if i >= bytes.len() {
            return Err(AppError::internal(
                "truncated numstat entry (no tab after removed)",
            ));
        }
        let removed_str = std::str::from_utf8(&bytes[removed_start..i])
            .map_err(|_| AppError::internal("non-UTF-8 in numstat removed"))?;
        i += 1;

        // path or empty (rename marker), terminated by NUL
        let path_start = i;
        while i < bytes.len() && bytes[i] != 0 {
            i += 1;
        }
        let path_field = std::str::from_utf8(&bytes[path_start..i])
            .map_err(|_| AppError::internal("non-UTF-8 in numstat path"))?
            .to_string();
        if i < bytes.len() {
            i += 1; // skip NUL
        }

        let added = parse_count(added_str);
        let removed = parse_count(removed_str);

        if path_field.is_empty() {
            // Rename: read old then new as separate NUL-terminated chunks.
            let old_start = i;
            while i < bytes.len() && bytes[i] != 0 {
                i += 1;
            }
            let old = std::str::from_utf8(&bytes[old_start..i])
                .map_err(|_| AppError::internal("non-UTF-8 in numstat rename old"))?
                .to_string();
            if i < bytes.len() {
                i += 1;
            }

            let new_start = i;
            while i < bytes.len() && bytes[i] != 0 {
                i += 1;
            }
            let new_path = std::str::from_utf8(&bytes[new_start..i])
                .map_err(|_| AppError::internal("non-UTF-8 in numstat rename new"))?
                .to_string();
            if i < bytes.len() {
                i += 1;
            }

            entries.push(NumstatEntry {
                added,
                removed,
                path: new_path,
                old_path: Some(old),
            });
        } else {
            entries.push(NumstatEntry {
                added,
                removed,
                path: path_field,
                old_path: None,
            });
        }
    }
    Ok(entries)
}

fn parse_count(s: &str) -> Option<usize> {
    if s == "-" {
        None
    } else {
        s.parse().ok()
    }
}

pub fn parse_unified_diff(text: &str) -> AppResult<Vec<ParsedFileDiff>> {
    let mut files: Vec<ParsedFileDiff> = Vec::new();
    let mut cur_file: Option<ParsedFileDiff> = None;
    let mut cur_hunk: Option<ParsedHunk> = None;

    for line in text.split_inclusive('\n') {
        let stripped = line.strip_suffix('\n').unwrap_or(line);

        if stripped.starts_with("diff --git ") {
            if let Some(h) = cur_hunk.take() {
                if let Some(f) = cur_file.as_mut() {
                    f.hunks.push(h);
                }
            }
            if let Some(f) = cur_file.take() {
                files.push(f);
            }
            let mut f = ParsedFileDiff::default();
            // Parse the "a/<old> b/<new>" trailer best-effort. Renames have
            // their own `rename from/to` lines that overwrite this anyway.
            let rest = &stripped["diff --git ".len()..];
            if let Some((a, b)) = parse_diff_git_paths(rest) {
                f.old_path = Some(a);
                f.new_path = Some(b);
            }
            f.raw_text.push_str(line);
            cur_file = Some(f);
            continue;
        }

        let Some(file) = cur_file.as_mut() else {
            continue;
        };
        file.raw_text.push_str(line);

        if let Some(rest) = stripped.strip_prefix("rename from ") {
            file.old_path = Some(rest.to_string());
            file.renamed = true;
            continue;
        }
        if let Some(rest) = stripped.strip_prefix("rename to ") {
            file.new_path = Some(rest.to_string());
            file.renamed = true;
            continue;
        }
        if stripped.starts_with("copy from ")
            || stripped.starts_with("copy to ")
            || stripped.starts_with("similarity index ")
            || stripped.starts_with("dissimilarity index ")
            || stripped.starts_with("index ")
            || stripped.starts_with("old mode ")
            || stripped.starts_with("new mode ")
        {
            continue;
        }
        if stripped.starts_with("new file mode ") {
            file.new_file = true;
            continue;
        }
        if stripped.starts_with("deleted file mode ") {
            file.deleted_file = true;
            continue;
        }
        if stripped.starts_with("Binary files ") || stripped.starts_with("GIT binary patch") {
            file.binary = true;
            continue;
        }
        if stripped.starts_with("--- ") || stripped.starts_with("+++ ") {
            continue;
        }
        if stripped.starts_with("@@ ") {
            if let Some(h) = cur_hunk.take() {
                file.hunks.push(h);
            }
            let raw = parse_hunk_header(stripped)?;
            cur_hunk = Some(ParsedHunk {
                raw,
                body: String::new(),
                added: 0,
                removed: 0,
            });
            continue;
        }
        if stripped.starts_with('\\') {
            // \ No newline at end of file ~ skip
            continue;
        }

        if let Some(h) = cur_hunk.as_mut() {
            h.body.push_str(line);
            match stripped.as_bytes().first() {
                Some(b'+') => h.added += 1,
                Some(b'-') => h.removed += 1,
                _ => {}
            }
        }
    }

    if let Some(h) = cur_hunk.take() {
        if let Some(f) = cur_file.as_mut() {
            f.hunks.push(h);
        }
    }
    if let Some(f) = cur_file.take() {
        files.push(f);
    }

    Ok(files)
}

fn parse_diff_git_paths(rest: &str) -> Option<(String, String)> {
    // Best-effort split of `a/<old> b/<new>`. Paths with embedded spaces (or
    // quoting) are handled authoritatively by the rename markers; this is
    // just a fallback so files without those markers still get a path.
    if !rest.starts_with("a/") {
        return None;
    }
    let idx = rest.find(" b/")?;
    let a = rest[2..idx].to_string();
    let b = rest[idx + " b/".len()..].to_string();
    Some((a, b))
}

pub fn parse_hunk_header(line: &str) -> AppResult<RawHunk> {
    let s = line
        .strip_prefix("@@ -")
        .ok_or_else(|| AppError::internal(format!("malformed hunk header: `{line}`")))?;

    let (old_start, mut rest) = read_uint(s).ok_or_else(|| {
        AppError::internal(format!("malformed hunk header (old_start): `{line}`"))
    })?;

    let old_len = if let Some(stripped) = rest.strip_prefix(',') {
        let (n, r) = read_uint(stripped).ok_or_else(|| {
            AppError::internal(format!("malformed hunk header (old_len): `{line}`"))
        })?;
        rest = r;
        n
    } else {
        1
    };

    rest = rest.strip_prefix(" +").ok_or_else(|| {
        AppError::internal(format!("malformed hunk header (missing ` +`): `{line}`"))
    })?;

    let (new_start, r2) = read_uint(rest).ok_or_else(|| {
        AppError::internal(format!("malformed hunk header (new_start): `{line}`"))
    })?;
    rest = r2;

    let new_len = if let Some(stripped) = rest.strip_prefix(',') {
        let (n, r) = read_uint(stripped).ok_or_else(|| {
            AppError::internal(format!("malformed hunk header (new_len): `{line}`"))
        })?;
        rest = r;
        n
    } else {
        1
    };

    if rest.strip_prefix(" @@").is_none() {
        return Err(AppError::internal(format!(
            "malformed hunk header (missing ` @@`): `{line}`"
        )));
    }

    Ok(RawHunk {
        old_start,
        old_len,
        new_start,
        new_len,
    })
}

fn read_uint(s: &str) -> Option<(usize, &str)> {
    let bytes = s.as_bytes();
    let mut end = 0;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == 0 {
        return None;
    }
    let n: usize = s[..end].parse().ok()?;
    Some((n, &s[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hunk_header_basic() {
        let h = parse_hunk_header("@@ -10,3 +12,5 @@").unwrap();
        assert_eq!(h.old_start, 10);
        assert_eq!(h.old_len, 3);
        assert_eq!(h.new_start, 12);
        assert_eq!(h.new_len, 5);
    }

    #[test]
    fn parse_hunk_header_omitted_count() {
        let h = parse_hunk_header("@@ -10 +12,5 @@").unwrap();
        assert_eq!(h.old_len, 1);
        let h = parse_hunk_header("@@ -10,3 +12 @@").unwrap();
        assert_eq!(h.new_len, 1);
        let h = parse_hunk_header("@@ -10 +12 @@").unwrap();
        assert_eq!(h.old_len, 1);
        assert_eq!(h.new_len, 1);
    }

    #[test]
    fn parse_hunk_header_with_section_text() {
        let h = parse_hunk_header("@@ -10,3 +12,5 @@ fn foo() {").unwrap();
        assert_eq!(h.new_start, 12);
        assert_eq!(h.new_len, 5);
    }

    #[test]
    fn parse_hunk_header_invalid_returns_error() {
        assert!(parse_hunk_header("not a hunk").is_err());
        assert!(parse_hunk_header("@@ -10").is_err());
        assert!(parse_hunk_header("@@ -10,3 +12,5").is_err());
        assert!(parse_hunk_header("@@ ").is_err());
    }

    #[test]
    fn parse_name_status_handles_renames() {
        // Real git format (verified empirically with `git diff --name-status -z -M`):
        // every field is NUL-separated, including the status->path separator.
        let bytes = b"R100\0old\0new\0M\0file.txt\0";
        let entries = parse_name_status_z(bytes).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].status, 'R');
        assert_eq!(entries[0].score, Some(100));
        assert_eq!(entries[0].old_path.as_deref(), Some("old"));
        assert_eq!(entries[0].path, "new");
        assert_eq!(entries[1].status, 'M');
        assert_eq!(entries[1].score, None);
        assert_eq!(entries[1].path, "file.txt");
        assert!(entries[1].old_path.is_none());
    }

    #[test]
    fn parse_name_status_handles_copy() {
        let bytes = b"C75\0src\0dst\0";
        let entries = parse_name_status_z(bytes).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, 'C');
        assert_eq!(entries[0].score, Some(75));
        assert_eq!(entries[0].old_path.as_deref(), Some("src"));
        assert_eq!(entries[0].path, "dst");
    }

    #[test]
    fn parse_name_status_handles_added_and_deleted() {
        let bytes = b"A\0new.rs\0D\0gone.rs\0";
        let entries = parse_name_status_z(bytes).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].status, 'A');
        assert_eq!(entries[0].path, "new.rs");
        assert_eq!(entries[1].status, 'D');
        assert_eq!(entries[1].path, "gone.rs");
    }

    #[test]
    fn parse_numstat_z_marks_binary_as_none() {
        let bytes = b"-\t-\tfoo.bin\0";
        let entries = parse_numstat_z(bytes).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].added.is_none());
        assert!(entries[0].removed.is_none());
        assert_eq!(entries[0].path, "foo.bin");
    }

    #[test]
    fn parse_numstat_z_handles_rename() {
        let bytes = b"3\t1\t\0old\0new\0";
        let entries = parse_numstat_z(bytes).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].added, Some(3));
        assert_eq!(entries[0].removed, Some(1));
        assert_eq!(entries[0].old_path.as_deref(), Some("old"));
        assert_eq!(entries[0].path, "new");
    }

    #[test]
    fn parse_numstat_z_handles_simple_records() {
        let bytes = b"5\t2\tsrc/cli.rs\0";
        let entries = parse_numstat_z(bytes).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].added, Some(5));
        assert_eq!(entries[0].removed, Some(2));
        assert_eq!(entries[0].path, "src/cli.rs");
        assert!(entries[0].old_path.is_none());
    }

    #[test]
    fn parse_numstat_z_handles_multiple_mixed_records() {
        let bytes = b"5\t2\ta.rs\0-\t-\timg.png\03\t1\t\0old\0new\0";
        let entries = parse_numstat_z(bytes).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "a.rs");
        assert!(entries[1].added.is_none());
        assert_eq!(entries[2].old_path.as_deref(), Some("old"));
        assert_eq!(entries[2].path, "new");
    }

    #[test]
    fn parse_unified_diff_splits_per_file_and_collects_hunks() {
        let text = concat!(
            "diff --git a/foo b/foo\n",
            "index aaa..bbb 100644\n",
            "--- a/foo\n",
            "+++ b/foo\n",
            "@@ -1,2 +1,3 @@\n",
            " line1\n",
            "+added\n",
            " line2\n",
            "diff --git a/bar b/bar\n",
            "index ccc..ddd 100644\n",
            "--- a/bar\n",
            "+++ b/bar\n",
            "@@ -1 +1 @@\n",
            "-old\n",
            "+new\n",
        );
        let files = parse_unified_diff(text).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path.as_deref(), Some("foo"));
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(files[0].hunks[0].added, 1);
        assert_eq!(files[0].hunks[0].removed, 0);
        assert_eq!(files[1].new_path.as_deref(), Some("bar"));
        assert_eq!(files[1].hunks.len(), 1);
        assert_eq!(files[1].hunks[0].added, 1);
        assert_eq!(files[1].hunks[0].removed, 1);
    }

    #[test]
    fn parse_unified_diff_handles_binary_files_differ_marker() {
        let text = concat!(
            "diff --git a/img.png b/img.png\n",
            "index aaa..bbb 100644\n",
            "Binary files a/img.png and b/img.png differ\n",
        );
        let files = parse_unified_diff(text).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].binary);
        assert!(files[0].hunks.is_empty());
    }

    #[test]
    fn parse_unified_diff_detects_new_file() {
        let text = concat!(
            "diff --git a/n.rs b/n.rs\n",
            "new file mode 100644\n",
            "index 000..aaa\n",
            "--- /dev/null\n",
            "+++ b/n.rs\n",
            "@@ -0,0 +1,2 @@\n",
            "+fn n() {}\n",
            "+\n",
        );
        let files = parse_unified_diff(text).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].new_file);
        assert_eq!(files[0].hunks[0].added, 2);
    }

    #[test]
    fn parse_unified_diff_detects_rename_via_markers() {
        let text = concat!(
            "diff --git a/orig b/renamed\n",
            "similarity index 90%\n",
            "rename from orig\n",
            "rename to renamed\n",
            "index aaa..bbb 100644\n",
            "--- a/orig\n",
            "+++ b/renamed\n",
            "@@ -1 +1 @@\n",
            "-old\n",
            "+new\n",
        );
        let files = parse_unified_diff(text).unwrap();
        assert!(files[0].renamed);
        assert_eq!(files[0].old_path.as_deref(), Some("orig"));
        assert_eq!(files[0].new_path.as_deref(), Some("renamed"));
    }

    #[test]
    fn parse_unified_diff_skips_no_newline_marker() {
        let text = concat!(
            "diff --git a/n b/n\n",
            "index aaa..bbb 100644\n",
            "--- a/n\n",
            "+++ b/n\n",
            "@@ -1 +1 @@\n",
            "-old\n",
            "\\ No newline at end of file\n",
            "+new\n",
            "\\ No newline at end of file\n",
        );
        let files = parse_unified_diff(text).unwrap();
        assert_eq!(files[0].hunks[0].added, 1);
        assert_eq!(files[0].hunks[0].removed, 1);
        assert!(!files[0].hunks[0].body.contains("No newline"));
    }

    #[test]
    fn parse_unified_diff_raw_text_contains_full_file_block() {
        let text = concat!(
            "diff --git a/foo b/foo\n",
            "--- a/foo\n",
            "+++ b/foo\n",
            "@@ -1 +1 @@\n",
            "-old\n",
            "+new\n",
        );
        let files = parse_unified_diff(text).unwrap();
        assert!(files[0].raw_text.contains("diff --git"));
        assert!(files[0].raw_text.contains("@@"));
        assert!(files[0].raw_text.contains("-old"));
        assert!(files[0].raw_text.contains("+new"));
    }

    #[test]
    fn parse_unified_diff_empty_input_returns_empty_vec() {
        let files = parse_unified_diff("").unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn validate_ref_rejects_dash_prefix_and_traversal() {
        assert!(validate_ref("-rf").is_err());
        assert!(validate_ref("--upload-pack=oops").is_err());
        assert!(validate_ref("foo..bar").is_err());
        assert!(validate_ref("foo bar").is_err());
        assert!(validate_ref("").is_err());
        assert!(validate_ref("HEAD\nfoo").is_err());
        assert!(validate_ref("HEAD").is_ok());
        assert!(validate_ref("HEAD~1").is_ok());
        assert!(validate_ref("HEAD^").is_ok());
        assert!(validate_ref("origin/main").is_ok());
        assert!(validate_ref("v1.2.3").is_ok());
        assert!(validate_ref("feature/foo").is_ok());
    }
}
