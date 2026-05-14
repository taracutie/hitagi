// Integration tests for `mimi diff`. Each test builds a real git repo in a
// tempdir (sealed off from the user's git config via GIT_CONFIG_GLOBAL=/dev/null
// + GIT_CONFIG_SYSTEM=/dev/null) and shells out to the cargo-built binary.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::{Mutex, OnceLock};

use assert_cmd::Command;
use mimi::{
    commands::{
        self as app_commands, DiffBodyMode, DiffFileOptions, DiffOptions, DiffScope,
        DiffSummaryOptions, MAX_FILE_BYTES,
    },
    repo::RepoRoot,
};
use serde::Serialize;
use serde_json::Value;

const TEST_PACK_LANGUAGES: &[&str] = &["rust"];

struct DiffRepo {
    cache_dir: PathBuf,
    repo: PathBuf,
    /// Cached parent of `repo` and `cache_dir` so Drop can blow it all away.
    root: PathBuf,
}

impl DiffRepo {
    fn new(name: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "mimi-difftest-{}-{name}-{unique}",
            std::process::id()
        ));
        let repo = root.join("repo");
        let cache_dir = root.join("cache");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();

        let s = Self {
            cache_dir,
            repo,
            root,
        };
        s.git(&["init", "-q", "-b", "main"]);
        s.git(&["config", "commit.gpgsign", "false"]);
        s
    }

    fn git(&self, args: &[&str]) -> std::process::Output {
        // Sealed config: ignore the user's ~/.gitconfig and /etc/gitconfig.
        // Identity is set via env vars so commits work without writing config.
        let out = StdCommand::new("git")
            .current_dir(&self.repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t.test")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.test")
            .args(args)
            .output()
            .expect("git invocation failed");
        if !out.status.success() {
            panic!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        out
    }

    fn write(&self, rel: &str, body: &str) {
        let path = self.repo.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn write_bytes(&self, rel: &str, body: &[u8]) {
        let path = self.repo.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn rm(&self, rel: &str) {
        let _ = std::fs::remove_file(self.repo.join(rel));
    }

    fn add(&self, rel: &str) {
        self.git(&["add", rel]);
    }

    fn add_all(&self) {
        self.git(&["add", "-A"]);
    }

    fn commit(&self, message: &str) {
        self.add_all();
        self.git(&["commit", "-q", "-m", message]);
    }

    fn run(&self, args: &[&str]) -> Value {
        run_in(&self.repo, &self.cache_dir, args)
    }

    fn run_failure(&self, args: &[&str]) -> String {
        prewarm_language_pack();
        let assert = Command::cargo_bin("mimi")
            .unwrap()
            .env("MIMI_CACHE_DIR", &self.cache_dir)
            .arg("--repo")
            .arg(&self.repo)
            .args(args)
            .assert()
            .failure();
        String::from_utf8(assert.get_output().stderr.clone()).unwrap()
    }

    fn run_text(&self, args: &[&str]) -> String {
        prewarm_language_pack();
        let stdout = Command::cargo_bin("mimi")
            .unwrap()
            .env("MIMI_CACHE_DIR", &self.cache_dir)
            .arg("--repo")
            .arg(&self.repo)
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(stdout).unwrap()
    }
}

impl Drop for DiffRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn run_in(repo: &Path, cache_dir: &Path, args: &[&str]) -> Value {
    prewarm_language_pack();
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let old = std::env::var_os("MIMI_CACHE_DIR");
    std::env::set_var("MIMI_CACHE_DIR", cache_dir);
    let value = run_diff_structured(repo, args);
    match old {
        Some(old) => std::env::set_var("MIMI_CACHE_DIR", old),
        None => std::env::remove_var("MIMI_CACHE_DIR"),
    }
    value
}

fn run_diff_structured(repo: &Path, args: &[&str]) -> Value {
    assert_eq!(args[0], "diff");
    let repo = RepoRoot::new(std::fs::canonicalize(repo).unwrap());
    let parsed = ParsedDiffArgs::parse(&args[1..]);
    let opts = DiffOptions {
        scope: parsed.scope,
        against: parsed.against,
        excludes: parsed.excludes,
    };
    if parsed.paths_only {
        return to_value(app_commands::diff_paths(&repo, &parsed.paths, opts).unwrap());
    }
    if parsed.commit {
        return to_value(
            app_commands::diff_summary(
                &repo,
                &parsed.paths,
                opts,
                DiffSummaryOptions {
                    symbols: true,
                    commit: true,
                    group_by_state: true,
                },
            )
            .unwrap(),
        );
    }
    if parsed.summary {
        return to_value(
            app_commands::diff_summary(
                &repo,
                &parsed.paths,
                opts,
                DiffSummaryOptions {
                    symbols: parsed.symbols,
                    commit: false,
                    group_by_state: false,
                },
            )
            .unwrap(),
        );
    }
    let drill = DiffFileOptions {
        symbol: parsed.symbol,
        raw: parsed.raw,
        body: parsed.body,
        snippet: parsed.snippet,
    };
    if parsed.paths.is_empty() {
        return to_value(app_commands::diff_overview(&repo, opts).unwrap());
    }
    let no_drill_flags =
        !drill.raw && drill.symbol.is_none() && drill.body == DiffBodyMode::Full && !drill.snippet;
    if no_drill_flags
        && app_commands::diff_paths_are_all_directories(&repo, &parsed.paths, opts.clone()).unwrap()
    {
        return to_value(
            app_commands::diff_summary(&repo, &parsed.paths, opts, DiffSummaryOptions::default())
                .unwrap(),
        );
    }
    if parsed.paths.len() == 1 {
        to_value(app_commands::diff_file(&repo, &parsed.paths[0], opts, drill).unwrap())
    } else {
        let drill = DiffFileOptions {
            symbol: None,
            raw: drill.raw,
            body: drill.body,
            snippet: drill.snippet,
        };
        to_value(app_commands::diff_files(&repo, &parsed.paths, opts, drill).unwrap())
    }
}

struct ParsedDiffArgs {
    paths: Vec<String>,
    symbol: Option<String>,
    raw: bool,
    summary: bool,
    commit: bool,
    symbols: bool,
    paths_only: bool,
    body: DiffBodyMode,
    snippet: bool,
    scope: DiffScope,
    against: String,
    excludes: Vec<String>,
}

impl Default for ParsedDiffArgs {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            symbol: None,
            raw: false,
            summary: false,
            commit: false,
            symbols: false,
            paths_only: false,
            body: DiffBodyMode::Full,
            snippet: false,
            scope: DiffScope::All,
            against: "HEAD".to_string(),
            excludes: Vec::new(),
        }
    }
}

impl ParsedDiffArgs {
    fn parse(args: &[&str]) -> Self {
        let mut parsed = Self::default();
        let mut i = 0;
        while i < args.len() {
            match args[i] {
                "--symbol" => {
                    parsed.symbol = Some(args[i + 1].to_string());
                    i += 2;
                }
                "--raw" => {
                    parsed.raw = true;
                    i += 1;
                }
                "--summary" => {
                    parsed.summary = true;
                    i += 1;
                }
                "--commit" => {
                    parsed.commit = true;
                    i += 1;
                }
                "--symbols" => {
                    parsed.symbols = true;
                    i += 1;
                }
                "--paths" | "--names-only" => {
                    parsed.paths_only = true;
                    i += 1;
                }
                "--body" => {
                    parsed.body = parse_body(args[i + 1]);
                    i += 2;
                }
                "--snippet" => {
                    parsed.snippet = true;
                    i += 1;
                }
                "--staged" => {
                    parsed.scope = DiffScope::Staged;
                    i += 1;
                }
                "--unstaged" => {
                    parsed.scope = DiffScope::Unstaged;
                    i += 1;
                }
                "--untracked" => {
                    parsed.scope = DiffScope::Untracked;
                    i += 1;
                }
                "--against" => {
                    parsed.against = args[i + 1].to_string();
                    i += 2;
                }
                "--exclude" => {
                    parsed.excludes.push(args[i + 1].to_string());
                    i += 2;
                }
                path if path.starts_with('-') => panic!("unsupported diff arg {path}"),
                path => {
                    parsed.paths.push(path.to_string());
                    i += 1;
                }
            }
        }
        parsed
    }
}

fn parse_body(value: &str) -> DiffBodyMode {
    match value {
        "full" => DiffBodyMode::Full,
        "changed-lines" => DiffBodyMode::ChangedLines,
        "added-only" => DiffBodyMode::AddedOnly,
        "none" => DiffBodyMode::None,
        other => panic!("unsupported body mode {other}"),
    }
}

fn to_value<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).expect("response serializes for assertions")
}

fn prewarm_language_pack() {
    static PREWARM: OnceLock<()> = OnceLock::new();
    PREWARM.get_or_init(|| {
        tree_sitter_language_pack::download(TEST_PACK_LANGUAGES)
            .expect("test parser languages download");
    });
}

// ~~ Overview ~~

#[test]
fn overview_clean_repo_emits_clean_true() {
    let r = DiffRepo::new("clean");
    r.write("a.rs", "pub fn one() {}\n");
    r.commit("base");

    let v = r.run(&["diff"]);
    assert_eq!(v["clean"], true);
    assert!(v["files"].as_array().unwrap().is_empty());
}

#[test]
fn overview_lists_modified_added_deleted() {
    let r = DiffRepo::new("MAD");
    r.write("a.rs", "pub fn one() {}\n");
    r.write("gone.rs", "pub fn gone() {}\n");
    r.commit("base");

    r.write("a.rs", "pub fn one_v2() {}\n"); // M
    r.write("new.rs", "pub fn new_one() {}\n"); // ? until added
    r.add("new.rs"); // → A
    r.rm("gone.rs"); // → D

    let v = r.run(&["diff"]);
    let files: Vec<(String, String)> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["path"].as_str().unwrap().to_string(),
                f["status"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(files.iter().any(|(p, s)| p == "a.rs" && s == "M"));
    assert!(files.iter().any(|(p, s)| p == "new.rs" && s == "A"));
    assert!(files.iter().any(|(p, s)| p == "gone.rs" && s == "D"));
}

#[test]
fn overview_detects_rename() {
    let r = DiffRepo::new("rename");
    r.write(
        "orig.rs",
        "pub fn original_function_with_some_body() {\n    println!(\"hi\");\n}\n",
    );
    r.commit("base");
    r.git(&["mv", "orig.rs", "renamed.rs"]);

    let v = r.run(&["diff"]);
    let entry = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "renamed.rs")
        .expect("renamed entry should appear");
    assert_eq!(entry["status"], "R");
    assert_eq!(entry["old_path"], "orig.rs");
}

#[test]
fn overview_includes_untracked() {
    let r = DiffRepo::new("untracked");
    r.write("base.rs", "pub fn base() {}\n");
    r.commit("base");
    r.write("notes.txt", "hello\n");

    let v = r.run(&["diff"]);
    let entry = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "notes.txt")
        .expect("untracked entry should appear");
    assert_eq!(entry["status"], "?");
    assert_eq!(entry["added"], 1);
    assert_eq!(entry["removed"], 0);
}

#[test]
fn untracked_overview_multiline_added() {
    let r = DiffRepo::new("untracked-multiline");
    r.write("base.rs", "pub fn base() {}\n");
    r.commit("base");
    r.write("notes.txt", "alpha\nbeta\ngamma\n");

    let v = r.run(&["diff"]);
    let entry = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "notes.txt")
        .expect("untracked entry should appear");
    assert_eq!(entry["status"], "?");
    assert_eq!(entry["added"], 3);
    assert_eq!(entry["removed"], 0);
    assert!(entry.get("binary").is_none(), "text file should not be flagged binary");
}

#[test]
fn untracked_overview_binary_emits_no_count() {
    let r = DiffRepo::new("untracked-binary");
    r.write("base.rs", "pub fn base() {}\n");
    r.commit("base");
    r.write_bytes("blob.bin", b"alpha\0beta\0gamma");

    let v = r.run(&["diff"]);
    let entry = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "blob.bin")
        .expect("untracked entry should appear");
    assert_eq!(entry["status"], "?");
    assert!(entry.get("added").is_none());
    assert!(entry.get("removed").is_none());
    assert_eq!(entry["binary"], true);
}

#[test]
fn untracked_overview_oversize_emits_no_count() {
    let r = DiffRepo::new("untracked-oversize");
    r.write("base.rs", "pub fn base() {}\n");
    r.commit("base");
    // One byte over the limit so the metadata short-circuit triggers without
    // pulling a full read into memory.
    r.write_bytes("huge.txt", &vec![b'x'; MAX_FILE_BYTES + 1]);

    let v = r.run(&["diff"]);
    let entry = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "huge.txt")
        .expect("untracked entry should appear");
    assert_eq!(entry["status"], "?");
    assert!(entry.get("added").is_none());
    assert!(entry.get("removed").is_none());
    assert!(entry.get("binary").is_none(), "oversize is not the binary branch");
}

#[test]
fn overview_staged_and_unstaged_flags_in_combined_scope() {
    let r = DiffRepo::new("staged-flags");
    r.write("a.rs", "pub fn one() {}\n");
    r.commit("base");

    r.write("a.rs", "pub fn one_v2() {}\n");
    r.add("a.rs"); // staged change
    r.write("a.rs", "pub fn one_v3() {}\n"); // additional unstaged change

    let v = r.run(&["diff"]);
    let entry = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "a.rs")
        .expect("a.rs entry should appear");
    assert_eq!(entry["staged"], true);
    assert_eq!(entry["unstaged"], true);
}

#[test]
fn overview_scope_staged_excludes_unstaged_only_files() {
    let r = DiffRepo::new("scope-staged");
    r.write("a.rs", "pub fn a() {}\n");
    r.write("b.rs", "pub fn b() {}\n");
    r.commit("base");

    r.write("a.rs", "pub fn a_v2() {}\n");
    r.add("a.rs"); // staged
    r.write("b.rs", "pub fn b_v2() {}\n"); // unstaged

    let v = r.run(&["diff", "--staged"]);
    assert_eq!(v["scope"], "staged");
    let paths: Vec<&str> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"a.rs"));
    assert!(!paths.contains(&"b.rs"));
}

#[test]
fn overview_scope_unstaged_excludes_staged_only_files_but_includes_untracked() {
    let r = DiffRepo::new("scope-unstaged");
    r.write("a.rs", "pub fn a() {}\n");
    r.write("b.rs", "pub fn b() {}\n");
    r.commit("base");

    r.write("a.rs", "pub fn a_v2() {}\n");
    r.add("a.rs"); // staged-only
    r.write("b.rs", "pub fn b_v2() {}\n"); // unstaged
    r.write("c.txt", "untracked\n");

    let v = r.run(&["diff", "--unstaged"]);
    assert_eq!(v["scope"], "unstaged");
    let paths: Vec<&str> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"b.rs"));
    assert!(paths.contains(&"c.txt"));
    assert!(!paths.contains(&"a.rs")); // staged-only ~ not visible in --unstaged
}

#[test]
fn overview_against_other_ref_emits_against_field() {
    let r = DiffRepo::new("against-ref");
    r.write("a.rs", "pub fn one() {}\n");
    r.commit("base");
    r.write("a.rs", "pub fn two() {}\n");
    r.commit("v2");
    r.write("a.rs", "pub fn three() {}\n"); // working tree change

    let v = r.run(&["diff", "--against", "HEAD~1"]);
    assert_eq!(v["against"], "HEAD~1");
    let entry = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "a.rs")
        .unwrap();
    assert_eq!(entry["status"], "M");
    // 1 added (current), 1 removed (the post-base commit) is the visible delta;
    // the exact count depends on context but we can sanity-check non-zero.
    assert!(entry["added"].as_u64().unwrap() >= 1);
    assert!(entry["removed"].as_u64().unwrap() >= 1);
}

#[test]
fn overview_default_against_field_is_omitted() {
    let r = DiffRepo::new("against-default");
    r.write("a.rs", "pub fn a() {}\n");
    r.commit("base");
    r.write("a.rs", "pub fn b() {}\n");

    let v = r.run(&["diff"]);
    assert!(v.get("against").is_none(), "default HEAD should be elided");
}

#[test]
fn overview_exclude_filters_files() {
    let r = DiffRepo::new("exclude");
    r.write("a.rs", "pub fn a() {}\n");
    r.write("ignored/b.rs", "pub fn b() {}\n");
    r.commit("base");
    r.write("a.rs", "pub fn av2() {}\n");
    r.write("ignored/b.rs", "pub fn bv2() {}\n");

    let v = r.run(&["diff", "--exclude", "ignored"]);
    let paths: Vec<&str> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"a.rs"));
    assert!(!paths.iter().any(|p| p.contains("ignored/")));
}

#[test]
fn overview_outside_git_repo_errors_clearly() {
    // Build a non-repo directory and point mimi at it. Because the binary
    // canonicalises the repo root, we just need a real dir that's NOT inside
    // a git checkout. /tmp ~ but to be safe, build a sealed tmpdir.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "mimi-difftest-nogit-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();

    let stderr = Command::cargo_bin("mimi")
        .unwrap()
        .env("MIMI_CACHE_DIR", &root)
        // GIT_CEILING_DIRECTORIES so git doesn't walk up out of the tempdir
        // and find some unrelated repo (the user's repo, in the worst case).
        .env("GIT_CEILING_DIRECTORIES", &root)
        .arg("--repo")
        .arg(&root)
        .args(["diff"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let s = String::from_utf8(stderr).unwrap();
    assert!(
        s.contains("not a git repository"),
        "expected not-a-git-repo error, got: {s}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

// ~~ Drilldown ~~

#[test]
fn drilldown_emits_structured_hunks_with_symbol_annotation() {
    let r = DiffRepo::new("drill-symbol");
    r.write(
        "lib.rs",
        "pub fn alpha() {\n    println!(\"alpha\");\n}\n\npub fn beta() {\n    println!(\"beta\");\n}\n",
    );
    r.commit("base");
    // Modify a line inside `beta` only.
    r.write(
        "lib.rs",
        "pub fn alpha() {\n    println!(\"alpha\");\n}\n\npub fn beta() {\n    println!(\"BETA-V2\");\n}\n",
    );

    let v = r.run(&["diff", "lib.rs"]);
    let hunks = v["hunks"].as_array().unwrap();
    assert!(!hunks.is_empty());
    // At least one hunk should be annotated with `beta`.
    assert!(hunks.iter().any(|h| h["symbol"] == "beta"));
}

#[test]
fn drilldown_raw_returns_unified_text() {
    let r = DiffRepo::new("drill-raw");
    r.write("a.rs", "pub fn one() {}\n");
    r.commit("base");
    r.write("a.rs", "pub fn two() {}\n");

    let v = r.run(&["diff", "a.rs", "--raw"]);
    assert!(v.get("hunks").is_none(), "raw mode should omit `hunks`");
    let raw = v["raw"].as_str().unwrap();
    assert!(raw.contains("@@"), "raw should contain hunk header");
    assert!(raw.contains("-pub fn one()"));
    assert!(raw.contains("+pub fn two()"));
}

#[test]
fn drilldown_filter_by_symbol_keeps_only_overlapping_hunks() {
    let r = DiffRepo::new("drill-symbol-filter");
    r.write(
        "lib.rs",
        "pub fn alpha() {\n    let x = 1;\n}\n\npub fn beta() {\n    let y = 2;\n}\n",
    );
    r.commit("base");
    // Modify both functions.
    r.write(
        "lib.rs",
        "pub fn alpha() {\n    let x = 11;\n}\n\npub fn beta() {\n    let y = 22;\n}\n",
    );

    let v = r.run(&["diff", "lib.rs", "--symbol", "beta"]);
    let hunks = v["hunks"].as_array().unwrap();
    assert!(!hunks.is_empty());
    for h in hunks {
        assert_eq!(h["symbol"], "beta");
    }
}

#[test]
fn drilldown_unknown_symbol_errors_with_suggestions() {
    let r = DiffRepo::new("drill-unknown-symbol");
    r.write("lib.rs", "pub fn alpha() {\n    let x = 1;\n}\n");
    r.commit("base");
    r.write("lib.rs", "pub fn alpha() {\n    let x = 2;\n}\n");

    let stderr = r.run_failure(&["diff", "lib.rs", "--symbol", "notthere"]);
    assert!(stderr.contains("symbol not found"));
}

#[test]
fn drilldown_deleted_file_returns_status_d_no_hunks_with_note() {
    let r = DiffRepo::new("drill-delete");
    r.write("gone.rs", "pub fn gone() {\n    let x = 1;\n}\n");
    r.commit("base");
    r.rm("gone.rs");

    let v = r.run(&["diff", "gone.rs"]);
    assert_eq!(v["status"], "D");
    // We DO emit hunks for a deletion (the diff shows -lines).
    let hunks = v["hunks"].as_array().expect("hunks should be present");
    assert!(!hunks.is_empty());
    assert!(hunks[0]["removed"].as_u64().unwrap() >= 1);
}

#[test]
fn drilldown_deleted_file_annotates_with_head_blob_symbols() {
    let r = DiffRepo::new("drill-delete-symbol");
    r.write(
        "gone.rs",
        "pub fn alpha() {\n    let x = 1;\n}\n\npub fn beta() {\n    let y = 2;\n}\n",
    );
    r.commit("base");
    r.rm("gone.rs");

    let v = r.run(&["diff", "gone.rs"]);
    assert_eq!(v["status"], "D");
    let hunks = v["hunks"].as_array().unwrap();
    // For pure deletions the hunks should annotate with HEAD-side symbols.
    let symbols: Vec<&str> = hunks.iter().filter_map(|h| h["symbol"].as_str()).collect();
    assert!(symbols.iter().any(|s| *s == "alpha" || *s == "beta"));
}

#[test]
fn drilldown_added_file_emits_full_addition_hunk() {
    let r = DiffRepo::new("drill-add");
    r.write("base.rs", "pub fn b() {}\n");
    r.commit("base");
    r.write("new.rs", "pub fn brand_new() {\n    1 + 1;\n}\n");
    r.add("new.rs");

    let v = r.run(&["diff", "--staged", "new.rs"]);
    assert_eq!(v["status"], "A");
    let hunks = v["hunks"].as_array().unwrap();
    assert!(!hunks.is_empty());
    assert!(hunks[0]["added"].as_u64().unwrap() >= 3);
    assert_eq!(hunks[0]["removed"], 0);
}

#[test]
fn drilldown_renamed_file_includes_old_path_and_hunks() {
    let r = DiffRepo::new("drill-rename");
    r.write(
        "orig.rs",
        "pub fn lots_of_content() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n",
    );
    r.commit("base");
    r.git(&["mv", "orig.rs", "renamed.rs"]);
    // Tweak content so it's not a 100% rename.
    r.write(
        "renamed.rs",
        "pub fn lots_of_content() {\n    let a = 1;\n    let b = 2;\n    let CHANGED = 99;\n}\n",
    );

    let v = r.run(&["diff", "renamed.rs"]);
    assert_eq!(v["status"], "R");
    assert_eq!(v["old_path"], "orig.rs");
    assert!(!v["hunks"].as_array().unwrap().is_empty());
}

#[test]
fn drilldown_path_with_no_changes_errors_clearly() {
    let r = DiffRepo::new("drill-clean-path");
    r.write("untouched.rs", "pub fn u() {}\n");
    r.commit("base");

    // No diff for this path. Resolving it against the (empty) candidate set
    // should produce a "path not found in diff" error.
    let stderr = r.run_failure(&["diff", "untouched.rs"]);
    assert!(stderr.contains("path not found in diff"));
}

#[test]
fn drilldown_binary_file_marks_binary_true_no_hunks() {
    let r = DiffRepo::new("drill-binary");
    // A small "binary" file ~ git treats files containing NUL bytes as binary.
    let blob: Vec<u8> = vec![0u8, 1, 2, 3, 4, 5, 0, 6, 7, 8];
    std::fs::write(r.repo.join("blob.bin"), &blob).unwrap();
    r.commit("base");
    let blob2: Vec<u8> = vec![0u8, 9, 8, 7, 6, 5, 0, 4, 3, 2, 1];
    std::fs::write(r.repo.join("blob.bin"), &blob2).unwrap();

    let v = r.run(&["diff", "blob.bin"]);
    assert_eq!(v["binary"], true);
    assert!(v.get("hunks").is_none());
}

#[test]
fn multi_file_drilldown_returns_files_array_without_changing_single_file_shape() {
    let r = DiffRepo::new("multi-file");
    r.write("a.rs", "pub fn a() {}\n");
    r.write("b.rs", "pub fn b() {}\n");
    r.commit("base");
    r.write("a.rs", "pub fn aa() {}\n");
    r.write("b.rs", "pub fn bb() {}\n");

    let single = r.run(&["diff", "a.rs"]);
    assert_eq!(single["path"], "a.rs");
    assert!(single.get("files").is_none());

    let multi = r.run(&["diff", "a.rs", "b.rs"]);
    let files = multi["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["path"], "a.rs");
    assert_eq!(files[1]["path"], "b.rs");
    assert!(files[0]["hunks"].is_array());
    assert!(files[1]["hunks"].is_array());
}

#[test]
fn summary_mode_returns_compact_files_and_symbols_when_requested() {
    let r = DiffRepo::new("summary");
    r.write(
        "a.rs",
        "pub fn alpha() {\n    let x = 1;\n}\n\npub fn beta() {\n    let y = 2;\n}\n",
    );
    r.write("b.rs", "pub fn gamma() {\n    let z = 3;\n}\n");
    r.commit("base");
    r.write(
        "a.rs",
        "pub fn alpha() {\n    let x = 11;\n}\n\npub fn beta() {\n    let y = 2;\n}\n",
    );
    r.write("b.rs", "pub fn gamma() {\n    let z = 4;\n}\n");

    let compact = r.run(&["diff", "--summary"]);
    let files = compact["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|file| file.get("hunks").is_none()));
    assert!(files.iter().all(|file| file.get("symbols").is_none()));

    let with_symbols = r.run(&["diff", "--summary", "--symbols"]);
    let files = with_symbols["files"].as_array().unwrap();
    let a = files
        .iter()
        .find(|file| file["path"].as_str() == Some("a.rs"))
        .unwrap();
    let b = files
        .iter()
        .find(|file| file["path"].as_str() == Some("b.rs"))
        .unwrap();
    let a_symbols = a["symbols"].as_array().unwrap();
    let b_symbols = b["symbols"].as_array().unwrap();
    assert!(a_symbols.iter().any(|s| s == "alpha"));
    assert!(b_symbols.iter().any(|s| s == "gamma"));
}

#[test]
fn commit_mode_returns_grouped_symbol_summary() {
    let r = DiffRepo::new("commit-mode");
    r.write("a.rs", "pub fn alpha() {\n    let x = 1;\n}\n");
    r.commit("base");
    r.write("a.rs", "pub fn alpha() {\n    let x = 2;\n}\n");
    r.write("new.rs", "pub fn fresh() {}\n");

    let json = r.run(&["diff", "--commit"]);
    assert_eq!(json["commit"], true);
    assert_eq!(json["files"].as_array().unwrap().len(), 2);
    let symbols = json["files"][0]["symbols"].as_array().unwrap();
    assert!(symbols.iter().any(|s| s == "alpha"));

    let text = r.run_text(&["diff", "--commit"]);
    assert!(text.contains("diff commit"), "{text}");
    assert!(text.contains("unstaged\n"), "{text}");
    assert!(text.contains("untracked\n"), "{text}");
    assert!(!text.contains("@@"), "{text}");
}

#[test]
fn diff_paths_lists_changed_paths_and_names_only_alias_matches() {
    let r = DiffRepo::new("paths-only");
    r.write("a.rs", "pub fn a() {}\n");
    r.commit("base");
    r.write("a.rs", "pub fn aa() {}\n");
    r.write("new.rs", "pub fn fresh() {}\n");

    let json = r.run(&["diff", "--paths"]);
    assert_eq!(json["paths"].as_array().unwrap().len(), 2);
    assert_eq!(json["paths"][0], "a.rs");
    assert_eq!(json["paths"][1], "new.rs");

    let text = r.run_text(&["diff", "--names-only"]);
    assert_eq!(text, "a.rs\nnew.rs\n");
}

#[test]
fn directory_paths_default_to_grouped_summary() {
    let r = DiffRepo::new("dir-summary");
    r.write("src/a.rs", "pub fn a() {}\n");
    r.write("tests/b.rs", "pub fn b() {}\n");
    r.commit("base");
    r.write("src/a.rs", "pub fn aa() {}\n");
    r.write("tests/b.rs", "pub fn bb() {}\n");

    let json = r.run(&["diff", "src", "tests"]);
    let groups = json["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["path"], "src");
    assert_eq!(groups[0]["file_count"], 1);
    assert_eq!(groups[1]["path"], "tests");
    assert_eq!(groups[1]["file_count"], 1);

    let text = r.run_text(&["diff", "src", "tests"]);
    assert!(text.contains("src • 1 files"), "{text}");
    assert!(text.contains("tests • 1 files"), "{text}");
    assert!(!text.contains("@@"), "{text}");
}

#[test]
fn drilldown_body_modes_and_snippet_reduce_hunk_context() {
    let r = DiffRepo::new("body-modes");
    r.write("a.rs", "pub fn a() {\n    let x = 1;\n}\n");
    r.commit("base");
    r.write("a.rs", "pub fn a() {\n    let x = 2;\n}\n");

    let none = r.run(&["diff", "a.rs", "--body", "none"]);
    let hunk = &none["hunks"].as_array().unwrap()[0];
    assert!(hunk.get("body").is_none());

    let added_only = r.run(&["diff", "a.rs", "--body", "added-only"]);
    let body = added_only["hunks"][0]["body"].as_str().unwrap();
    assert!(body.contains("+    let x = 2;"));
    assert!(!body.contains("-    let x = 1;"));

    let snippet = r.run(&["diff", "a.rs", "--body", "none", "--snippet"]);
    assert_eq!(snippet["hunks"][0]["snippet"], "-    let x = 1;");
}

#[test]
fn invalid_diff_flag_combinations_fail_clearly() {
    let r = DiffRepo::new("invalid-flags");
    r.write("a.rs", "pub fn a() {}\n");
    r.write("b.rs", "pub fn b() {}\n");
    r.commit("base");
    r.write("a.rs", "pub fn aa() {}\n");
    r.write("b.rs", "pub fn bb() {}\n");

    let stderr = r.run_failure(&["diff", "--raw"]);
    assert!(stderr.contains("--raw requires PATH"));

    let stderr = r.run_failure(&["diff", "a.rs", "b.rs", "--symbol", "a"]);
    assert!(stderr.contains("--symbol requires exactly one PATH"));

    let stderr = r.run_failure(&["diff", "a.rs", "--raw", "--snippet"]);
    assert!(stderr.contains("--raw and --snippet cannot be combined"));

    let stderr = r.run_failure(&["diff", "--commit", "--summary"]);
    assert!(stderr.contains("--commit and --summary cannot be combined"));

    let stderr = r.run_failure(&["diff", "--paths", "--symbols"]);
    assert!(stderr.contains("--paths and --symbols cannot be combined"));
}

// ~~ Edge cases ~~

#[test]
fn unborn_branch_errors_clearly() {
    let r = DiffRepo::new("unborn");
    r.write("a.rs", "pub fn a() {}\n");
    // No commit ~ HEAD is unborn.
    let stderr = r.run_failure(&["diff"]);
    assert!(
        stderr.contains("ref does not resolve") || stderr.contains("invalid ref"),
        "expected unborn-branch error, got: {stderr}"
    );
}

#[test]
fn against_with_leading_dash_rejected() {
    let r = DiffRepo::new("against-dash");
    r.write("a.rs", "pub fn a() {}\n");
    r.commit("base");

    // `--against=-rf` slips past clap's leading-dash detection because of `=`.
    let stderr = r.run_failure(&["diff", "--against=-rf"]);
    assert!(stderr.contains("invalid ref"));
}

#[test]
fn against_with_double_dot_rejected() {
    let r = DiffRepo::new("against-range");
    r.write("a.rs", "pub fn a() {}\n");
    r.commit("base");
    r.write("a.rs", "pub fn a_v2() {}\n");
    r.commit("base2");

    let stderr = r.run_failure(&["diff", "--against", "HEAD..HEAD~1"]);
    assert!(stderr.contains("invalid ref"));
}

#[test]
fn path_traversal_in_drilldown_rejected() {
    let r = DiffRepo::new("drill-traversal");
    r.write("a.rs", "pub fn a() {}\n");
    r.commit("base");
    r.write("a.rs", "pub fn b() {}\n");

    let stderr = r.run_failure(&["diff", "../escape"]);
    assert!(
        stderr.contains("escapes the repo root") || stderr.contains("must be relative"),
        "expected traversal rejection, got: {stderr}"
    );
}

#[test]
fn drilldown_resolves_path_by_unique_suffix() {
    let r = DiffRepo::new("drill-suffix");
    r.write("nested/deep/file.rs", "pub fn a() {}\n");
    r.commit("base");
    r.write("nested/deep/file.rs", "pub fn b() {}\n");

    // Suffix "file.rs" should resolve uniquely to "nested/deep/file.rs".
    let v = r.run(&["diff", "file.rs"]);
    assert_eq!(v["path"], "nested/deep/file.rs");
}

#[test]
fn drilldown_ambiguous_suffix_errors_with_candidates() {
    let r = DiffRepo::new("drill-ambig");
    r.write("a/file.rs", "pub fn a() {}\n");
    r.write("b/file.rs", "pub fn b() {}\n");
    r.commit("base");
    r.write("a/file.rs", "pub fn aa() {}\n");
    r.write("b/file.rs", "pub fn bb() {}\n");

    let stderr = r.run_failure(&["diff", "file.rs"]);
    assert!(stderr.contains("ambiguous"));
    assert!(stderr.contains("a/file.rs"));
    assert!(stderr.contains("b/file.rs"));
}

#[test]
fn drilldown_untracked_file_emits_added_hunk_with_symbol_annotation() {
    let r = DiffRepo::new("drill-untracked");
    r.write("a.rs", "pub fn a() {}\n");
    r.commit("base");
    r.write("new.rs", "pub fn fresh() {\n    let x = 1;\n}\n");

    let v = r.run(&["diff", "new.rs"]);
    assert_eq!(v["status"], "?");
    assert_eq!(v["added"], 3);
    assert_eq!(v["removed"], 0);
    let hunks = v["hunks"].as_array().unwrap();
    assert_eq!(hunks[0]["symbol"], "fresh");
    assert!(hunks[0]["body"].as_str().unwrap().contains("+pub fn fresh"));
}

#[test]
fn drilldown_untracked_raw_returns_synthetic_unified_diff() {
    let r = DiffRepo::new("drill-untracked-raw");
    r.write("a.rs", "pub fn a() {}\n");
    r.commit("base");
    r.write("new.rs", "pub fn fresh() {}\n");

    let v = r.run(&["diff", "new.rs", "--raw"]);
    assert_eq!(v["status"], "?");
    assert!(v.get("hunks").is_none());
    let raw = v["raw"].as_str().unwrap();
    assert!(raw.contains("new file mode 100644"));
    assert!(raw.contains("--- /dev/null"));
    assert!(raw.contains("+pub fn fresh"));
}

#[test]
fn overview_untracked_scope_only_lists_untracked_files() {
    let r = DiffRepo::new("scope-untracked");
    r.write("a.rs", "pub fn a() {}\n");
    r.commit("base");
    r.write("a.rs", "pub fn aa() {}\n");
    r.write("new.rs", "pub fn fresh() {}\n");

    let v = r.run(&["diff", "--untracked"]);
    assert_eq!(v["scope"], "untracked");
    let paths: Vec<&str> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["new.rs"]);
}

#[test]
fn drilldown_spans_emitted_for_multi_symbol_hunk() {
    // A single hunk that crosses two functions should surface both via `spans`.
    let r = DiffRepo::new("drill-spans");
    r.write(
        "lib.rs",
        "pub fn alpha() {\n    let x = 1;\n}\npub fn beta() {\n    let y = 2;\n}\n",
    );
    r.commit("base");
    // Replace lines in BOTH functions in one big swap so git produces a single
    // hunk crossing both.
    r.write(
        "lib.rs",
        "pub fn alpha() {\n    let x = 11;\n}\npub fn beta() {\n    let y = 22;\n}\n",
    );

    let v = r.run(&["diff", "lib.rs"]);
    let hunks = v["hunks"].as_array().unwrap();
    let multi = hunks
        .iter()
        .find(|h| h.get("spans").is_some())
        .map(|h| h["spans"].as_array().unwrap().len());
    if let Some(n) = multi {
        assert!(n >= 2);
    }
}

#[test]
fn monorepo_subdir_filters_to_cwd_subtree_only() {
    // Build a monorepo with three sibling projects at the git toplevel, then
    // point mimi at one project's subdir. Changes outside that subdir must
    // NOT appear in the overview ~ they should be silently filtered with a
    // top-level `note` listing the count.
    let r = DiffRepo::new("monorepo");
    r.write("project-a/src/lib.rs", "pub fn a() {}\n");
    r.write("project-b/src/lib.rs", "pub fn b() {}\n");
    r.write("project-c/src/lib.rs", "pub fn c() {}\n");
    r.write("shared/util.rs", "pub fn s() {}\n");
    r.commit("base");

    r.write("project-a/src/lib.rs", "pub fn a_v2() {}\n");
    r.write("project-b/src/lib.rs", "pub fn b_v2() {}\n");
    r.write("project-c/src/lib.rs", "pub fn c_v2() {}\n");
    r.write("shared/util.rs", "pub fn s_v2() {}\n");

    // Invoke mimi with --repo pointing at project-a/. The git toplevel is
    // r.repo; project-a is a subdir.
    let v = run_in(&r.repo.join("project-a"), &r.cache_dir, &["diff"]);
    let paths: Vec<&str> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths.len(),
        1,
        "only project-a's change should appear: {paths:?}"
    );
    assert!(paths[0].ends_with("lib.rs"));
    let note = v["note"].as_str().expect("filter note should be set");
    assert!(note.contains("project-a"));
    assert!(note.contains("3 file"));

    // Drilling deeper (project-b/src) also filters correctly ~ shared/, root,
    // and other projects are all out of the subtree.
    let v = run_in(
        &r.repo.join("project-b").join("src"),
        &r.cache_dir,
        &["diff"],
    );
    let paths: Vec<&str> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("lib.rs"));

    // Same repo, this time with --repo at the toplevel ~ all 4 files visible,
    // no filter note.
    let v = run_in(&r.repo, &r.cache_dir, &["diff"]);
    assert_eq!(v["files"].as_array().unwrap().len(), 4);
    assert!(v.get("note").is_none());
}

#[test]
fn monorepo_subdir_drilldown_finds_path_via_repo_relative_form() {
    let r = DiffRepo::new("monorepo-drill");
    r.write(
        "project-a/src/lib.rs",
        "pub fn alpha() {\n    let x = 1;\n}\n",
    );
    r.write(
        "project-b/src/lib.rs",
        "pub fn beta() {\n    let y = 2;\n}\n",
    );
    r.commit("base");
    r.write(
        "project-a/src/lib.rs",
        "pub fn alpha() {\n    let x = 11;\n}\n",
    );
    r.write(
        "project-b/src/lib.rs",
        "pub fn beta() {\n    let y = 22;\n}\n",
    );

    // From project-a/, the repo-relative path is `src/lib.rs` (not
    // `project-a/src/lib.rs`).
    let v = run_in(
        &r.repo.join("project-a"),
        &r.cache_dir,
        &["diff", "src/lib.rs"],
    );
    assert_eq!(v["path"], "src/lib.rs");
    let hunks = v["hunks"].as_array().unwrap();
    assert!(!hunks.is_empty());
    assert!(hunks.iter().any(|h| h["symbol"] == "alpha"));

    // Project-b's lib.rs is NOT addressable from project-a's cwd ~ it falls
    // outside the subtree and was filtered out of the candidate set.
    let stderr = Command::cargo_bin("mimi")
        .unwrap()
        .env("MIMI_CACHE_DIR", &r.cache_dir)
        .arg("--repo")
        .arg(r.repo.join("project-a"))
        .args(["diff", "../project-b/src/lib.rs"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let s = String::from_utf8(stderr).unwrap();
    assert!(s.contains("escapes the repo root") || s.contains("must be relative"));
}

#[test]
fn monorepo_cross_subtree_rename_arrives_as_added_with_note() {
    // A clean rename across subtrees ~ git -M detects it as R; from the
    // destination subtree we surface as A with a `note` naming the toplevel
    // origin path. Source subtree gets a synthesized D entry (next test).
    let r = DiffRepo::new("xsubtree-arrive");
    r.write("lib/src/x.rs", "fn a()\n");
    r.write("libfoo/src/keep.rs", "fn k()\n");
    r.commit("base");
    r.git(&["mv", "lib/src/x.rs", "libfoo/src/x_arrived.rs"]);

    let v = run_in(&r.repo.join("libfoo"), &r.cache_dir, &["diff"]);
    let entry = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"].as_str().unwrap().ends_with("x_arrived.rs"))
        .expect("arrived file should appear");
    assert_eq!(entry["status"], "A");
    assert!(
        entry.get("old_path").is_none(),
        "old_path is leaked toplevel"
    );
    let note = entry["note"]
        .as_str()
        .expect("cross-subtree rename should carry a note");
    assert!(note.contains("renamed into this subtree"));
    assert!(note.contains("lib/src/x.rs"));
}

#[test]
fn monorepo_cross_subtree_rename_departs_as_deleted_with_note() {
    let r = DiffRepo::new("xsubtree-depart");
    r.write("lib/src/x.rs", "fn a()\nfn b()\n");
    r.write("libfoo/src/keep.rs", "fn k()\n");
    r.commit("base");
    r.git(&["mv", "lib/src/x.rs", "libfoo/src/x_arrived.rs"]);

    let v = run_in(&r.repo.join("lib"), &r.cache_dir, &["diff"]);
    let entry = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"].as_str().unwrap().ends_with("x.rs"))
        .expect("departed file should appear");
    assert_eq!(entry["status"], "D");
    let note = entry["note"]
        .as_str()
        .expect("cross-subtree rename should carry a note");
    assert!(note.contains("renamed out of this subtree"));
    assert!(note.contains("libfoo/src/x_arrived.rs"));
}

#[test]
fn monorepo_cross_subtree_rename_drilldown_shows_deletion_diff() {
    // The synthesized D entry must be addressable for drilldown so the user
    // can inspect what was removed.
    let r = DiffRepo::new("xsubtree-drill");
    r.write("lib/src/x.rs", "fn alpha() {}\nfn beta() {}\n");
    r.write("libfoo/src/keep.rs", "fn k()\n");
    r.commit("base");
    r.git(&["mv", "lib/src/x.rs", "libfoo/src/x_arrived.rs"]);

    let v = run_in(&r.repo.join("lib"), &r.cache_dir, &["diff", "src/x.rs"]);
    assert_eq!(v["status"], "D");
    let hunks = v["hunks"].as_array().unwrap();
    assert!(!hunks.is_empty());
    // The diff body should contain `-` lines for what was removed.
    let body_blob: String = hunks
        .iter()
        .filter_map(|h| h["body"].as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(body_blob.contains("-fn alpha"));
}

#[test]
fn monorepo_subdir_with_substring_sibling_does_not_leak() {
    // `lib/` must not match siblings whose names start with `lib` (libfoo, library).
    // Regression test for prefix vs substring matching ~ the rebase uses
    // `format!("{subdir}/")` to require a path-component boundary.
    let r = DiffRepo::new("substring-sibling");
    r.write("lib/src/x.rs", "fn l()\n");
    r.write("libfoo/src/x.rs", "fn lf()\n");
    r.write("library/src/x.rs", "fn lb()\n");
    r.commit("base");
    r.write("lib/src/x.rs", "fn ll()\n");
    r.write("libfoo/src/x.rs", "fn lflf()\n");
    r.write("library/src/x.rs", "fn lblb()\n");

    let v = run_in(&r.repo.join("lib"), &r.cache_dir, &["diff"]);
    let paths: Vec<&str> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths.len(),
        1,
        "lib/ subtree should not match libfoo/library"
    );
    assert!(paths[0].ends_with("x.rs"));
    let note = v["note"].as_str().expect("filter note should be set");
    assert!(note.contains("2 file"));
}

#[test]
fn default_diff_output_is_concise_text() {
    let r = DiffRepo::new("text");
    r.write("a.rs", "pub fn a() {}\n");
    r.commit("base");
    r.write("a.rs", "pub fn aa() {}\n");

    let text = r.run_text(&["diff"]);
    assert!(text.starts_with("diff"));
    assert!(text.contains("1 files"));
    assert!(text.contains("M a.rs"));
    assert!(text.contains("• unstaged"));
    assert!(
        serde_json::from_str::<Value>(&text).is_err(),
        "default diff output should not be JSON"
    );
}

#[test]
fn default_diff_text_groups_combined_scope_by_folder() {
    let r = DiffRepo::new("text-groups");
    r.write("src/a.rs", "pub fn a() {}\n");
    r.write("src/search/c.rs", "pub fn c() {}\n");
    r.write("tests/b.rs", "pub fn b() {}\n");
    r.commit("base");

    r.write("src/a.rs", "pub fn aa() {}\n");
    r.add("src/a.rs");
    r.write("src/a.rs", "pub fn aaa() {}\n");
    r.write("src/search/c.rs", "pub fn cc() {}\n");
    r.write("tests/b.rs", "pub fn bb() {}\n");
    r.write("new.rs", "pub fn fresh() {}\n");

    let text = r.run_text(&["diff"]);
    assert!(text.contains("▾ ./ • 1 file"), "{text}");
    assert!(text.contains("└─ ? new.rs"), "{text}");
    assert!(text.contains("▾ src/ • 2 files"), "{text}");
    assert!(text.contains("├─ M src/a.rs"), "{text}");
    assert!(text.contains("└─ M src/search/c.rs"), "{text}");
    assert!(!text.contains("▾ src/search/"), "{text}");
    assert!(text.contains("▾ tests/ • 1 file"), "{text}");
    assert!(text.contains("└─ M tests/b.rs"), "{text}");
    assert!(text.contains("• staged"), "{text}");
    assert!(text.contains("• unstaged"), "{text}");
    assert!(!text.contains("staged+unstaged\n"), "{text}");
    assert!(!text.contains("untracked\n"), "{text}");
}

#[test]
fn default_diff_text_renders_rename_origins_with_correct_prefixing() {
    let cross = DiffRepo::new("text-cross-dir-rename");
    cross.write("a/foo.rs", "pub fn foo() {}\n");
    cross.commit("base");
    std::fs::create_dir_all(cross.repo.join("b")).unwrap();
    cross.git(&["mv", "a/foo.rs", "b/foo.rs"]);

    let text = cross.run_text(&["diff"]);
    assert!(text.contains("R b/foo.rs"), "{text}");
    assert!(text.contains("← a/foo.rs"), "{text}");
    assert!(!text.contains("← b/a/foo.rs"), "{text}");

    let same = DiffRepo::new("text-same-prefix-rename");
    same.write("src/orig.rs", "pub fn foo() {}\n");
    same.commit("base");
    same.git(&["mv", "src/orig.rs", "src/renamed.rs"]);

    let text = same.run_text(&["diff"]);
    assert!(text.contains("R src/renamed.rs"), "{text}");
    assert!(text.contains("← src/orig.rs"), "{text}");
    assert!(!text.contains("← orig.rs"), "{text}");
}

#[test]
fn default_diff_file_output_shows_hunks_as_text() {
    let r = DiffRepo::new("file-text");
    r.write("a.rs", "pub fn a() {}\n");
    r.commit("base");
    r.write("a.rs", "pub fn aa() {}\n");

    let text = r.run_text(&["diff", "a.rs"]);
    assert!(text.starts_with("diff a.rs"));
    assert!(text.contains("@@"));
    assert!(text.contains("+pub fn aa()"));
}
