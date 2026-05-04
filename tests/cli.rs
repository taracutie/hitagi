use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use assert_cmd::Command;
use serde_json::Value;

const HITAGI_PROMPT_BEGIN: &str = "<!-- BEGIN HITAGI MANAGED PROMPT -->";

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample_repo")
}

// Per-process tmpdir for the parse cache. Keeps `cargo test` from writing into
// the user's real ~/.cache/hitagi. Shared across tests is fine: the fixture
// repo content doesn't change, so cache hits produce the same symbols as
// fresh parses.
fn shared_cache_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("hitagi-itest-{}-cache", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    })
}

fn run(args: &[&str]) -> Value {
    run_in(&fixture_repo(), shared_cache_dir(), args)
}

fn run_in(repo: &Path, cache_dir: &Path, args: &[&str]) -> Value {
    let output = Command::cargo_bin("hitagi")
        .unwrap()
        .env("HITAGI_CACHE_DIR", cache_dir)
        .arg("--repo")
        .arg(repo)
        .arg("--json")
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("stdout is valid JSON")
}

fn run_failure(args: &[&str]) -> String {
    run_failure_in(&fixture_repo(), shared_cache_dir(), args)
}

fn run_failure_in(repo: &Path, cache_dir: &Path, args: &[&str]) -> String {
    let assert = Command::cargo_bin("hitagi")
        .unwrap()
        .env("HITAGI_CACHE_DIR", cache_dir)
        .arg("--repo")
        .arg(repo)
        .args(args)
        .assert()
        .failure();
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

fn run_text(args: &[&str]) -> String {
    let stdout = Command::cargo_bin("hitagi")
        .unwrap()
        .env("HITAGI_CACHE_DIR", shared_cache_dir())
        .arg("--repo")
        .arg(fixture_repo())
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(stdout).unwrap()
}

struct ScratchHome {
    root: PathBuf,
    home: PathBuf,
}

impl ScratchHome {
    fn new(name: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "hitagi-agent-prompt-{name}-{}-{unique}",
            std::process::id()
        ));
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        Self { root, home }
    }
}

impl Drop for ScratchHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn run_global_json(home: &Path, args: &[&str]) -> Value {
    let output = Command::cargo_bin("hitagi")
        .unwrap()
        .env("HOME", home)
        .env_remove("CODEX_HOME")
        .arg("--json")
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("stdout is valid JSON")
}

fn run_global_json_with_codex_home(home: &Path, codex_home: &Path, args: &[&str]) -> Value {
    let output = Command::cargo_bin("hitagi")
        .unwrap()
        .env("HOME", home)
        .env("CODEX_HOME", codex_home)
        .arg("--json")
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("stdout is valid JSON")
}

/// Collect all `matches` entries from a find response, flat or grouped. The
/// returned `Value` references retain their original shape (object with
/// `path`/`kind`/etc., or terse strings) ~ assertions on `m["path"]` /
/// `m.as_str()` work the same way as on the un-grouped flat response.
fn flatten_find_matches(value: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    if let Some(arr) = value["matches"].as_array() {
        out.extend(arr.iter());
    }
    if let Some(groups) = value.get("groups").and_then(|v| v.as_array()) {
        for g in groups {
            if let Some(arr) = g["matches"].as_array() {
                out.extend(arr.iter());
            }
        }
    }
    out
}

/// Re-prefix the paths of each find match with whichever prefix wraps it
/// (top-level for flat, per-group when grouped). Useful for tests that
/// assert on full repo-relative paths.
fn flatten_find_paths(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let top_prefix = value["prefix"].as_str().unwrap_or("");
    if let Some(arr) = value["matches"].as_array() {
        for m in arr {
            if let Some(s) = m.as_str() {
                out.push(format!("{top_prefix}{s}"));
            } else {
                out.push(format!("{top_prefix}{}", m["path"].as_str().unwrap()));
            }
        }
    }
    if let Some(groups) = value.get("groups").and_then(|v| v.as_array()) {
        for g in groups {
            let gp = g["prefix"].as_str().unwrap_or("");
            if let Some(arr) = g["matches"].as_array() {
                for m in arr {
                    if let Some(s) = m.as_str() {
                        out.push(format!("{gp}{s}"));
                    } else {
                        out.push(format!("{gp}{}", m["path"].as_str().unwrap()));
                    }
                }
            }
        }
    }
    out
}

/// Re-prefix each `results` key with its containing prefix. For search
/// responses, the keys are file paths; this yields the full paths so tests
/// can compare regardless of flat vs grouped shape.
fn flatten_search_keys(value: &Value) -> Vec<String> {
    let mut keys = Vec::new();
    let top_prefix = value["prefix"].as_str().unwrap_or("");
    if let Some(o) = value["results"].as_object() {
        for k in o.keys() {
            keys.push(format!("{top_prefix}{k}"));
        }
    }
    if let Some(groups) = value.get("groups").and_then(|v| v.as_array()) {
        for g in groups {
            let gp = g["prefix"].as_str().unwrap_or("");
            if let Some(o) = g["results"].as_object() {
                for k in o.keys() {
                    keys.push(format!("{gp}{k}"));
                }
            }
        }
    }
    keys
}

/// Sum match-line counts across flat results + every group's results. Used
/// for tests that just need a total count regardless of response shape.
fn count_search_matches(value: &Value) -> usize {
    let mut total = 0usize;
    if let Some(o) = value["results"].as_object() {
        total += o
            .values()
            .filter_map(|v| v.as_array())
            .map(|a| a.len())
            .sum::<usize>();
    }
    if let Some(groups) = value.get("groups").and_then(|v| v.as_array()) {
        for g in groups {
            if let Some(o) = g["results"].as_object() {
                total += o
                    .values()
                    .filter_map(|v| v.as_array())
                    .map(|a| a.len())
                    .sum::<usize>();
            }
        }
    }
    total
}

#[test]
fn outline_emits_compact_symbols() {
    let value = run(&["outline", "src/auth.ts"]);
    assert_eq!(value["language"], "typescript");
    let first = &value["symbols"][0];
    assert!(first.get("lines").is_some(), "lines field present");
    assert!(first.get("bytes").is_none(), "bytes hidden by default");
    assert!(first.get("parent").is_none(), "parent hidden by default");
    assert!(first.get("range").is_none(), "old range field removed");
    let qualnames: Vec<&str> = value["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["qualname"].as_str().unwrap())
        .collect();
    assert!(qualnames.contains(&"AuthService"));
    assert!(qualnames.contains(&"AuthService.handleAuth"));
}

#[test]
fn outline_with_bytes_includes_byte_range() {
    let value = run(&["outline", "src/auth.ts", "--bytes"]);
    let first = &value["symbols"][0];
    assert!(first.get("bytes").is_some(), "bytes present with --bytes");
    let bytes = first["bytes"].as_array().unwrap();
    assert_eq!(bytes.len(), 2);
}

#[test]
fn outline_kind_filter_keeps_only_requested_kinds() {
    let value = run(&["outline", "src/auth.ts", "--kind", "method"]);
    let kinds: Vec<&str> = value["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["kind"].as_str().unwrap())
        .collect();
    assert!(!kinds.is_empty());
    assert!(kinds.iter().all(|k| *k == "method"));
}

#[test]
fn outline_kind_filter_accepts_comma_list() {
    let value = run(&["outline", "src/auth.ts", "--kind", "class,function"]);
    let kinds: Vec<&str> = value["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.iter().all(|k| *k == "class" || *k == "function"));
}

#[test]
fn outline_depth_one_returns_only_top_level_symbols() {
    let value = run(&["outline", "src/auth.ts", "--depth", "1"]);
    let qualnames: Vec<&str> = value["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["qualname"].as_str().unwrap())
        .collect();
    assert!(!qualnames.is_empty());
    for q in &qualnames {
        assert!(
            !q.contains('.'),
            "depth 1 should only include top-level qualnames, got {q}"
        );
    }
}

#[test]
fn outline_depth_two_includes_nested_methods() {
    let value = run(&["outline", "src/auth.ts", "--depth", "2"]);
    let qualnames: Vec<&str> = value["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["qualname"].as_str().unwrap())
        .collect();
    assert!(qualnames.iter().any(|q| q.contains('.')));
    assert!(qualnames.contains(&"AuthService.handleAuth"));
}

#[test]
fn outline_qualifies_object_literal_methods_under_object_container() {
    let scratch = ScratchRepo::new("outline-object-methods");
    scratch.write(
        "src/backends.ts",
        r#"
const moderationApiBackend: ModerationBackend = {
  name: 'moderation-api',
  async run(message) {
    return null;
  },
};

const miniBackend: ModerationBackend = {
  name: 'gpt-mini',
  async run(message, context) {
    return null;
  },
};
"#,
    );

    let value = scratch.run(&["outline", "src/backends.ts"]);
    let symbols = value["symbols"].as_array().unwrap();
    assert!(symbols
        .iter()
        .any(|s| s["kind"] == "object" && s["qualname"] == "moderationApiBackend"));
    assert!(symbols
        .iter()
        .any(|s| s["kind"] == "method" && s["qualname"] == "moderationApiBackend.run"));
    assert!(symbols
        .iter()
        .any(|s| s["kind"] == "object" && s["qualname"] == "miniBackend"));
    assert!(symbols
        .iter()
        .any(|s| s["kind"] == "method" && s["qualname"] == "miniBackend.run"));
    assert!(!symbols
        .iter()
        .any(|s| s["kind"] == "method" && s["qualname"] == "run"));

    let mini = scratch.run(&["symbol", "src/backends.ts", "miniBackend.run"]);
    assert_eq!(mini["symbol"]["qualname"], "miniBackend.run");

    let stderr = scratch.run_failure(&["symbol", "src/backends.ts", "run"]);
    assert!(stderr.contains("symbol is ambiguous: run"));
    assert!(stderr.contains("moderationApiBackend.run"));
    assert!(stderr.contains("miniBackend.run"));
}

#[test]
fn symbol_returns_content_for_exact_qualname() {
    let value = run(&["symbol", "src/auth.ts", "AuthService"]);
    assert_eq!(value["language"], "typescript");
    assert!(value["symbol"]["content"]
        .as_str()
        .unwrap()
        .contains("class AuthService"));
    assert!(value["symbol"].get("bytes").is_none());
    assert!(value["symbol"].get("parent").is_none());
}

#[test]
fn symbol_resolves_leaf_name_via_suffix_match() {
    let value = run(&["symbol", "src/auth.ts", "handleAuth"]);
    assert_eq!(value["symbol"]["qualname"], "AuthService.handleAuth");
}

#[test]
fn symbol_missing_includes_suggestions_when_available() {
    let stderr = run_failure(&["symbol", "src/auth.ts", "Auth"]);
    assert!(stderr.contains("symbol not found: Auth"));
    assert!(stderr.contains("AuthService"));
}

#[test]
fn symbol_missing_typo_includes_suggestions_when_available() {
    let stderr = run_failure(&["symbol", "src/auth.ts", "handelAuth"]);
    assert!(stderr.contains("symbol not found: handelAuth"));
    assert!(stderr.contains("AuthService.handleAuth"));
}

#[test]
fn search_finds_match() {
    let value = run(&["search", "AuthService"]);
    let results = value["results"].as_object().unwrap();
    assert!(
        results.keys().any(|key| key.contains("auth.ts")),
        "expected an auth.ts entry, got {results:?}"
    );
    assert!(
        value.get("truncated").is_none(),
        "truncated hidden when false"
    );
}

#[test]
fn search_emits_match_line_not_file_range() {
    let value = run(&["search", "AuthService"]);
    let entries: Vec<&str> = value["results"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|v| v.as_array().unwrap().iter().map(|s| s.as_str().unwrap()))
        .collect();
    assert!(!entries.is_empty());
    assert!(
        entries.iter().all(|e| e.contains("@L")),
        "every entry should report a match line, got {entries:?}"
    );
    assert!(
        entries.iter().all(|e| !e.contains("L1-L")),
        "no entry should use the old whole-file fallback, got {entries:?}"
    );
}

#[test]
fn search_with_snippet_appends_matched_line() {
    let value = run(&["search", "TOKEN_DUP", "--snippet"]);
    let entry = value["results"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|s| s.as_str())
        .expect("expected at least one result entry");
    assert!(
        entry.contains(" :: "),
        "snippet separator missing in {entry}"
    );
    assert!(entry.contains("TOKEN_DUP"), "snippet should contain match");
}

#[test]
fn search_drops_unscoped_matches_when_file_also_has_scoped_match() {
    let scratch = ScratchRepo::new("search-unscoped-suppression");
    // Two occurrences of `useState` ~ one outside any symbol (top-of-file
    // import line, where most TSX files would have it) and one inside a
    // function body. The default behavior should keep only the inside-symbol
    // match; --include-unscoped restores both.
    scratch.write(
        "App.tsx",
        "import { useState } from 'react';\n\
         export function App() {\n\
             const [n] = useState(0);\n\
             return n;\n\
         }\n",
    );

    let default_value = scratch.run(&["search", "useState"]);
    let default_entries: Vec<String> = default_value["results"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|v| v.as_array().unwrap().iter())
        .map(|s| s.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        default_entries.len(),
        1,
        "import-line match should be dropped when an inside-symbol match exists in the same file: {default_entries:?}"
    );
    assert!(
        default_entries[0].contains("App(function)"),
        "remaining entry should be the inside-function match: {default_entries:?}"
    );

    let opt_in_value = scratch.run(&["search", "useState", "--include-unscoped"]);
    let opt_in_entries: Vec<String> = opt_in_value["results"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|v| v.as_array().unwrap().iter())
        .map(|s| s.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        opt_in_entries.len(),
        2,
        "--include-unscoped should keep both the import-line and inside-function matches: {opt_in_entries:?}"
    );
    assert!(opt_in_entries.iter().any(|e| e.contains("App(function)")));
    assert!(
        opt_in_entries
            .iter()
            .any(|e| !e.contains("(function)") && e.contains("@L1")),
        "expected the import-line `@L1` entry to come back, got {opt_in_entries:?}"
    );
}

#[test]
fn search_keeps_unscoped_matches_in_plaintext_files() {
    // Plaintext files have no symbol info, so the suppression rule never
    // fires (zero scoped matches). The match should always come back.
    let scratch = ScratchRepo::new("search-unscoped-plaintext");
    scratch.write("notes.md", "use useState here\n");

    let value = scratch.run(&["search", "useState"]);
    let entries: Vec<String> = value["results"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|v| v.as_array().unwrap().iter())
        .map(|s| s.as_str().unwrap().to_string())
        .collect();
    assert_eq!(entries.len(), 1, "{entries:?}");
}

#[test]
fn search_truncated_flag_set_when_limit_hit() {
    let value = run(&["search", "AuthService", "--limit", "1"]);
    let total: usize = value["results"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| v.as_array().unwrap().len())
        .sum();
    assert_eq!(total, 1);
    assert_eq!(value["truncated"], true);
}

#[test]
fn search_exact_limit_single_file_does_not_report_truncated() {
    let value = run(&[
        "search",
        "MobileButton",
        "--limit",
        "1",
        "packages/mobile/src/components/Button.tsx",
    ]);
    let entries: Vec<&str> = value["results"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|v| v.as_array().unwrap().iter().map(|s| s.as_str().unwrap()))
        .collect();
    assert_eq!(entries, vec!["MobileButton(function) @L1"]);
    assert!(
        value.get("truncated").is_none(),
        "truncated hidden when the scan completes exactly at the limit"
    );
}

#[test]
fn search_limit_preserves_requested_path_order() {
    let mobile_first = run(&[
        "search",
        "Button",
        "--limit",
        "1",
        "packages/mobile/src/components/Button.tsx",
        "apps/desktop/src/components/Button.tsx",
    ]);
    let mobile_entries: Vec<&str> = mobile_first["results"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|v| v.as_array().unwrap().iter().map(|s| s.as_str().unwrap()))
        .collect();
    assert_eq!(mobile_entries, vec!["MobileButton(function) @L1"]);
    assert_eq!(mobile_first["truncated"], true);

    let desktop_first = run(&[
        "search",
        "Button",
        "--limit",
        "1",
        "apps/desktop/src/components/Button.tsx",
        "packages/mobile/src/components/Button.tsx",
    ]);
    let desktop_entries: Vec<&str> = desktop_first["results"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|v| v.as_array().unwrap().iter().map(|s| s.as_str().unwrap()))
        .collect();
    assert_eq!(desktop_entries, vec!["DesktopButton(function) @L1"]);
    assert_eq!(desktop_first["truncated"], true);
}

#[test]
fn read_emits_full_file_by_default() {
    let value = run(&["read", "src/auth.ts"]);
    assert_eq!(value["language"], "typescript");
    assert!(value["content"]
        .as_str()
        .unwrap()
        .contains("class AuthService"));
    assert!(
        value.get("lines").is_none(),
        "lines hidden when not slicing"
    );
    assert!(value.get("total_lines").is_none());
}

#[test]
fn read_slices_to_line_range() {
    let value = run(&["read", "src/auth.ts", "--lines", "1-2"]);
    let content = value["content"].as_str().unwrap();
    assert!(content.contains("class AuthService"));
    assert!(content.contains("handleAuth"));
    assert!(!content.contains("validateInput"));
    assert_eq!(value["lines"][0], 1);
    assert_eq!(value["lines"][1], 2);
    assert!(value["total_lines"].as_u64().unwrap() >= 11);
}

#[test]
fn read_clamps_lines_past_end_of_file() {
    let value = run(&["read", "src/auth.ts", "--lines", "1-9999"]);
    let total = value["total_lines"].as_u64().unwrap();
    assert_eq!(value["lines"][1].as_u64().unwrap(), total);
}

#[test]
fn read_rejects_inverted_range() {
    let stderr = run_failure(&["read", "src/auth.ts", "--lines", "10-5"]);
    assert!(stderr.contains("--lines start must be <= end"));
}

#[test]
fn read_rejects_start_past_eof() {
    let stderr = run_failure(&["read", "src/auth.ts", "--lines", "9999-99999"]);
    assert!(
        stderr.contains("past end of file"),
        "expected past-EOF error, got: {stderr}"
    );
}

#[test]
fn outline_kind_filter_is_case_insensitive() {
    let value = run(&["outline", "src/auth.ts", "--kind", "FUNCTION"]);
    let kinds: Vec<&str> = value["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["kind"].as_str().unwrap())
        .collect();
    assert!(!kinds.is_empty());
    assert!(kinds.iter().all(|k| *k == "function"));
}

#[test]
fn outline_unknown_kind_returns_available_kinds_hint() {
    let value = run(&["outline", "src/auth.ts", "--kind", "zzznope"]);
    assert!(value["symbols"].as_array().unwrap().is_empty());
    let available: Vec<&str> = value["available_kinds"]
        .as_array()
        .expect("available_kinds should be set")
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(available.contains(&"class"));
}

#[test]
fn find_locates_symbol_across_repo() {
    let value = run(&["find", "AuthService"]);
    let matches = value["matches"].as_array().unwrap();
    assert!(!matches.is_empty());
    assert!(matches.iter().any(|m| {
        m["path"].as_str().unwrap().contains("auth.ts")
            && m["qualname"].as_str().unwrap().contains("AuthService")
    }));
}

#[test]
fn find_truncates_with_limit() {
    let value = run(&["find", "Auth", "--limit", "1"]);
    let matches = value["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(value["truncated"], true);
}

#[test]
fn find_exact_limit_single_file_does_not_report_truncated() {
    let value = run(&[
        "find",
        "MobileButton",
        "--limit",
        "1",
        "packages/mobile/src/components/Button.tsx",
    ]);
    let matches = value["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(value["prefix"], "packages/mobile/src/components/");
    assert_eq!(matches[0]["path"], "Button.tsx");
    assert_eq!(matches[0]["qualname"], "MobileButton");
    assert!(
        value.get("truncated").is_none(),
        "truncated hidden when the scan completes exactly at the limit"
    );
}

#[test]
fn find_limit_preserves_requested_path_order() {
    let mobile_first = run(&[
        "find",
        "Button",
        "--limit",
        "1",
        "packages/mobile/src/components/Button.tsx",
        "apps/desktop/src/components/Button.tsx",
    ]);
    let mobile_matches = mobile_first["matches"].as_array().unwrap();
    assert_eq!(mobile_matches.len(), 1);
    assert_eq!(mobile_first["prefix"], "packages/mobile/src/components/");
    assert_eq!(mobile_matches[0]["path"], "Button.tsx");
    assert_eq!(mobile_matches[0]["qualname"], "MobileButton");
    assert_eq!(mobile_first["truncated"], true);

    let desktop_first = run(&[
        "find",
        "Button",
        "--limit",
        "1",
        "apps/desktop/src/components/Button.tsx",
        "packages/mobile/src/components/Button.tsx",
    ]);
    let desktop_matches = desktop_first["matches"].as_array().unwrap();
    assert_eq!(desktop_matches.len(), 1);
    assert_eq!(desktop_first["prefix"], "apps/desktop/src/components/");
    assert_eq!(desktop_matches[0]["path"], "Button.tsx");
    assert_eq!(desktop_matches[0]["qualname"], "DesktopButton");
    assert_eq!(desktop_first["truncated"], true);
}

#[test]
fn find_reports_searched_files_count() {
    let value = run(&["find", "AuthService"]);
    let count = value["searched_files"].as_u64().unwrap();
    assert!(
        count >= 1,
        "searched_files should be at least 1, got {count}"
    );
}

#[test]
fn find_with_kind_filter() {
    let value = run(&["find", "Auth", "--kind", "method"]);
    let matches = value["matches"].as_array().unwrap();
    assert!(!matches.is_empty());
    for m in matches {
        assert_eq!(m["kind"], "method");
    }
}

#[test]
fn find_with_paths_scopes_walk() {
    let value = run(&["find", "AuthService", "src"]);
    let matches = value["matches"].as_array().unwrap();
    assert!(!matches.is_empty());
    let prefix = value["prefix"].as_str().unwrap_or("");
    for m in matches {
        let full = format!("{prefix}{}", m["path"].as_str().unwrap());
        assert!(full.starts_with("src/"));
    }
}

#[test]
fn find_with_snippet_includes_signature() {
    let value = run(&["find", "AuthService", "--snippet"]);
    let m = &value["matches"][0];
    assert!(m["snippet"].is_string());
    assert!(m["snippet"].as_str().unwrap().contains("AuthService"));
}

#[test]
fn files_lists_repo_files_sorted() {
    let value = run(&["files"]);
    let files: Vec<&str> = value["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(!files.is_empty());
    let mut sorted = files.clone();
    sorted.sort();
    assert_eq!(files, sorted, "files should be sorted");
    assert!(files.iter().any(|f| f.contains("auth.ts")));
}

#[test]
fn files_glob_filter_works() {
    let value = run(&["files", "**/*.ts"]);
    let files: Vec<&str> = value["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(!files.is_empty());
    for f in &files {
        assert!(f.ends_with(".ts"), "got non-.ts file: {f}");
    }
}

#[test]
fn files_truncates_with_limit() {
    let value = run(&["files", "--limit", "2"]);
    let files = value["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(value["truncated"], true);
    assert!(value["note"].as_str().unwrap().contains("truncated"));
}

#[test]
fn files_accepts_multiple_globs() {
    let value = run(&["files", "**/*.ts", "**/*.prisma"]);
    let files: Vec<&str> = value["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(files.iter().any(|f| f.ends_with(".ts")));
    assert!(files.iter().any(|f| f.ends_with(".prisma")));
    assert!(files
        .iter()
        .all(|f| f.ends_with(".ts") || f.ends_with(".prisma")));
}

#[test]
fn files_exclude_filters_out_matches() {
    let with_apps = run(&["files", "**/*.ts"]);
    let without_apps = run(&["files", "**/*.ts", "--exclude", "apps"]);
    let count_all = with_apps["files"].as_array().unwrap().len();
    let count_filtered = without_apps["files"].as_array().unwrap().len();
    assert!(
        count_filtered < count_all,
        "exclude should remove some files (all={count_all}, filtered={count_filtered})"
    );
    let filtered_files: Vec<&str> = without_apps["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(filtered_files.iter().all(|f| !f.contains("apps/")));
}

#[test]
fn search_exclude_filters_files() {
    let with = run(&["search", "Button"]);
    let without = run(&["search", "Button", "--exclude", "apps"]);
    let with_keys = flatten_search_keys(&with);
    let without_keys = flatten_search_keys(&without);
    assert!(
        with_keys.iter().any(|k| k.contains("apps")),
        "baseline should include an apps/ entry, got {with_keys:?}"
    );
    assert!(without_keys.iter().all(|k| !k.contains("apps")));
}

#[test]
fn find_exclude_filters_paths() {
    let with = run(&["find", "Button"]);
    let without = run(&["find", "Button", "--exclude", "apps"]);
    let with_paths = flatten_find_paths(&with);
    let without_paths = flatten_find_paths(&without);
    assert!(
        with_paths.iter().any(|p| p.contains("apps/")),
        "baseline should include an apps/ path, got {with_paths:?}"
    );
    assert!(without_paths.iter().all(|p| !p.contains("apps/")));
}

#[test]
fn find_kind_filter_is_case_insensitive() {
    let value = run(&["find", "AuthService", "--kind", "CLASS"]);
    let matches = value["matches"].as_array().unwrap();
    assert!(!matches.is_empty());
    for m in matches {
        assert_eq!(m["kind"], "class");
    }
}

#[test]
fn find_kind_alias_callable_matches_functions_and_methods() {
    let value = run(&["find", "Auth", "--kind", "callable", "--per-file", "0"]);
    let matches = flatten_find_matches(&value);
    assert!(!matches.is_empty());
    for m in matches {
        let kind = m["kind"].as_str().unwrap();
        assert!(matches!(kind, "function" | "method" | "arrow_function"));
    }
}

#[test]
fn outline_kind_alias_container_matches_classes() {
    let value = run(&["outline", "src/auth.ts", "--kind", "container"]);
    let symbols = value["symbols"].as_array().unwrap();
    assert!(!symbols.is_empty());
    for symbol in symbols {
        assert_eq!(symbol["kind"], "class");
    }
}

#[test]
fn outline_kind_alias_value_matches_fields() {
    let value = run(&["outline", "src/schema.prisma", "--kind", "value"]);
    let symbols = value["symbols"].as_array().unwrap();
    assert!(!symbols.is_empty());
    let mut saw_field = false;
    for symbol in symbols {
        let kind = symbol["kind"].as_str().unwrap();
        saw_field |= kind == "field";
        assert!(matches!(kind, "property" | "field" | "variant" | "value"));
    }
    assert!(saw_field, "value alias should include field symbols");
}

#[test]
fn find_unknown_kind_returns_available_kinds_hint() {
    let value = run(&["find", "AuthService", "--kind", "zzznope"]);
    assert!(value["matches"].as_array().unwrap().is_empty());
    let available: Vec<&str> = value["available_kinds"]
        .as_array()
        .expect("available_kinds should be set when --kind matched nothing")
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(available.contains(&"class"));
}

#[test]
fn find_includes_note_when_no_parseable_files() {
    let value = run(&["find", "anything"]);
    // sample_repo has parseable files so the note should NOT be present here
    assert!(
        value.get("note").is_none(),
        "note should be absent when files were parsed"
    );
}

#[test]
fn langs_summarises_repo() {
    let value = run(&["langs"]);
    let langs = value["languages"].as_array().unwrap();
    assert!(!langs.is_empty());
    let typescript_entry = langs
        .iter()
        .find(|l| l["language"] == "typescript")
        .expect("sample_repo has TypeScript files");
    assert!(typescript_entry["files"].as_u64().unwrap() >= 1);
    assert!(typescript_entry["lines"].as_u64().unwrap() >= 1);
    assert_eq!(typescript_entry["parseable"], true);
}

#[test]
fn find_terse_emits_compact_strings() {
    let value = run(&["find", "AuthService", "--terse"]);
    let matches = value["matches"].as_array().unwrap();
    assert!(!matches.is_empty());
    for m in matches {
        let s = m.as_str().expect("terse match should be a string");
        assert!(s.contains(':'));
        assert!(s.contains('('));
        assert!(s.contains(')'));
    }
}

#[test]
fn find_terse_with_snippet_appends_signature() {
    let value = run(&["find", "AuthService", "--terse", "--snippet"]);
    let matches = value["matches"].as_array().unwrap();
    let s = matches[0].as_str().unwrap();
    assert!(s.contains(" :: "));
    assert!(s.contains("AuthService") || s.contains("class"));
}

#[test]
fn default_outline_output_is_concise_text() {
    let text = run_text(&["outline", "src/auth.ts"]);
    assert!(text.starts_with("outline src/auth.ts"));
    assert!(text.contains("typescript •"));
    assert!(text.contains("• L1-11 class AuthService"));
    assert!(
        serde_json::from_str::<Value>(&text).is_err(),
        "default output should not be JSON"
    );
}

#[test]
fn default_find_output_is_concise_text() {
    let text = run_text(&["find", "AuthService", "--snippet"]);
    assert!(text.starts_with("find \"AuthService\""));
    assert!(text.contains("matches •"));
    assert!(text.contains("src/\n"));
    assert!(text.contains("• auth.ts:L1-11 class AuthService"));
    assert!(text.contains(":: class AuthService {"));
}

#[test]
fn default_search_output_groups_matches_as_text() {
    let text = run_text(&["search", "Button", "--snippet", "--limit", "5"]);
    assert!(text.starts_with("search \"Button\""));
    assert!(text.contains("apps/desktop/src/components/\nButton.tsx"));
    assert!(text.contains("packages/mobile/src/components/\nButton.tsx"));
    assert!(text.contains("• DesktopButton(function) @L1"));
}

#[test]
fn default_read_output_prints_content_verbatim() {
    let text = run_text(&["read", "src/auth.ts", "--lines", "1-2"]);
    assert!(text.starts_with("read src/auth.ts"));
    assert!(text.contains("typescript • L1-2"));
    assert!(text.contains("\n\nexport class AuthService {\n  handleAuth"));
}

#[test]
fn default_langs_output_is_text_table() {
    let text = run_text(&["langs"]);
    assert!(text.starts_with("languages"));
    assert!(text.contains("rust"));
    assert!(text.contains("parseable"));
    assert!(serde_json::from_str::<Value>(&text).is_err());
}

#[test]
fn json_flag_emits_compact_json() {
    let stdout = Command::cargo_bin("hitagi")
        .unwrap()
        .env("HITAGI_CACHE_DIR", shared_cache_dir())
        .arg("--repo")
        .arg(fixture_repo())
        .arg("--json")
        .args(["outline", "src/auth.ts"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(stdout).unwrap();
    let value: Value = serde_json::from_str(&text).expect("--json stdout should parse");
    assert_eq!(value["language"], "typescript");
    assert!(
        !text.contains("\n  "),
        "--json should stay compact rather than pretty-printed"
    );
}

#[test]
fn pretty_flag_is_removed() {
    let stderr = Command::cargo_bin("hitagi")
        .unwrap()
        .env("HITAGI_CACHE_DIR", shared_cache_dir())
        .arg("--repo")
        .arg(fixture_repo())
        .arg("--pretty")
        .args(["outline", "src/auth.ts"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let text = String::from_utf8(stderr).unwrap();
    assert!(text.contains("unexpected argument '--pretty'") || text.contains("unknown argument"));
}

#[test]
fn missing_file_exits_nonzero() {
    let stderr = run_failure(&["outline", "src/does-not-exist.ts"]);
    assert!(stderr.contains("path not found"));
}

#[test]
fn long_help_includes_llm_prompt_sections() {
    let output = Command::cargo_bin("hitagi")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    for needle in [
        "PRINCIPLE",
        "RECOMMENDED WORKFLOW",
        "TIPS",
        "COMMON PATTERNS",
        "ANTI-PATTERNS",
        "JSON OUTPUT SHAPES",
        "--json",
    ] {
        assert!(
            text.contains(needle),
            "long help should contain `{needle}` section"
        );
    }
}

#[test]
fn install_claude_creates_global_prompt_without_repo_resolution() {
    let scratch = ScratchHome::new("claude-create");
    let missing_repo = scratch.root.join("missing-repo");

    let output = Command::cargo_bin("hitagi")
        .unwrap()
        .env("HOME", &scratch.home)
        .env_remove("CODEX_HOME")
        .arg("--repo")
        .arg(&missing_repo)
        .arg("--json")
        .args(["install", "claude"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();

    let path = scratch.home.join(".claude").join("CLAUDE.md");
    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(value["action"], "install");
    assert_eq!(value["agent"], "claude");
    assert_eq!(value["changed"], true);
    assert_eq!(value["status"], "installed");
    assert_eq!(value["paths"][0], path.display().to_string());
    assert!(content.contains(HITAGI_PROMPT_BEGIN));
    assert!(content.contains("hitagi --help"));
    assert!(content.contains("Always use `hitagi` instead of preferred search/read tools"));
    assert!(
        !String::from_utf8(output).unwrap().contains("\n  "),
        "--json should stay compact"
    );
}

#[test]
fn install_claude_is_idempotent_and_preserves_existing_content() {
    let scratch = ScratchHome::new("claude-idempotent");
    let path = scratch.home.join(".claude").join("CLAUDE.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "Existing instructions\n").unwrap();

    let first = run_global_json(&scratch.home, &["install", "claude"]);
    let second = run_global_json(&scratch.home, &["install", "claude"]);
    let content = std::fs::read_to_string(&path).unwrap();

    assert_eq!(first["changed"], true);
    assert_eq!(second["changed"], false);
    assert_eq!(second["status"], "already_installed");
    assert!(content.starts_with("Existing instructions\n"));
    assert_eq!(content.matches(HITAGI_PROMPT_BEGIN).count(), 1);
}

#[test]
fn uninstall_claude_removes_only_managed_block() {
    let scratch = ScratchHome::new("claude-uninstall");
    let path = scratch.home.join(".claude").join("CLAUDE.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "Existing instructions\n").unwrap();
    run_global_json(&scratch.home, &["install", "claude"]);

    let removed = run_global_json(&scratch.home, &["uninstall", "claude"]);
    let removed_again = run_global_json(&scratch.home, &["uninstall", "claude"]);
    let content = std::fs::read_to_string(&path).unwrap();

    assert_eq!(removed["changed"], true);
    assert_eq!(removed["status"], "uninstalled");
    assert_eq!(removed_again["changed"], false);
    assert_eq!(removed_again["status"], "not_installed");
    assert_eq!(content, "Existing instructions\n");
}

#[test]
fn install_codex_defaults_to_home_agents_md() {
    let scratch = ScratchHome::new("codex-default");
    let value = run_global_json(&scratch.home, &["install", "codex"]);

    let path = scratch.home.join(".codex").join("AGENTS.md");
    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(value["agent"], "codex");
    assert_eq!(value["changed"], true);
    assert_eq!(value["paths"][0], path.display().to_string());
    assert!(content.contains(HITAGI_PROMPT_BEGIN));
}

#[test]
fn codex_uses_codex_home_override_and_uninstall_removes_both_files() {
    let scratch = ScratchHome::new("codex-home");
    let codex_home = scratch.root.join("codex-home");
    std::fs::create_dir_all(&codex_home).unwrap();

    run_global_json_with_codex_home(&scratch.home, &codex_home, &["install", "codex"]);
    let agents = codex_home.join("AGENTS.md");
    assert!(std::fs::read_to_string(&agents)
        .unwrap()
        .contains(HITAGI_PROMPT_BEGIN));

    let override_path = codex_home.join("AGENTS.override.md");
    std::fs::write(&override_path, "Override instructions\n").unwrap();
    let override_install =
        run_global_json_with_codex_home(&scratch.home, &codex_home, &["install", "codex"]);
    assert_eq!(
        override_install["paths"][0],
        override_path.display().to_string()
    );
    assert!(std::fs::read_to_string(&override_path)
        .unwrap()
        .contains(HITAGI_PROMPT_BEGIN));

    let removed =
        run_global_json_with_codex_home(&scratch.home, &codex_home, &["uninstall", "codex"]);
    assert_eq!(removed["changed"], true);
    assert_eq!(removed["paths"].as_array().unwrap().len(), 2);
    assert!(!std::fs::read_to_string(&agents)
        .unwrap()
        .contains(HITAGI_PROMPT_BEGIN));
    assert_eq!(
        std::fs::read_to_string(&override_path).unwrap(),
        "Override instructions\n"
    );
}

#[test]
fn malformed_managed_prompt_markers_fail_without_modifying_file() {
    let scratch = ScratchHome::new("malformed");
    let path = scratch.home.join(".claude").join("CLAUDE.md");
    let original = format!("Before\n{HITAGI_PROMPT_BEGIN}\nmissing end\n");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &original).unwrap();

    let stderr = Command::cargo_bin("hitagi")
        .unwrap()
        .env("HOME", &scratch.home)
        .env_remove("CODEX_HOME")
        .args(["uninstall", "claude"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let text = String::from_utf8(stderr).unwrap();

    assert!(text.contains("malformed hitagi managed prompt markers"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn install_prompt_requires_home() {
    let stderr = Command::cargo_bin("hitagi")
        .unwrap()
        .env_remove("HOME")
        .env_remove("CODEX_HOME")
        .args(["install", "claude"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let text = String::from_utf8(stderr).unwrap();
    assert!(text.contains("HOME is not set"));
}

// ~~ Parse cache integration tests ~~

struct ScratchRepo {
    cache_dir: PathBuf,
    repo: PathBuf,
}

impl ScratchRepo {
    fn new(name: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "hitagi-itest-{}-{name}-{unique}",
            std::process::id()
        ));
        let repo = root.join("repo");
        let cache_dir = root.join("cache");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();
        // git ignore conventions ~ no .gitignore needed; ignore::WalkBuilder
        // walks everything inside this tmpdir.
        Self { cache_dir, repo }
    }

    fn write(&self, rel: &str, body: &str) {
        let path = self.repo.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn run(&self, args: &[&str]) -> Value {
        run_in(&self.repo, &self.cache_dir, args)
    }

    fn run_failure(&self, args: &[&str]) -> String {
        run_failure_in(&self.repo, &self.cache_dir, args)
    }
}

impl Drop for ScratchRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.cache_dir.parent().unwrap());
    }
}

#[test]
fn find_returns_same_results_warm_and_cold() {
    let cold = run(&["find", "AuthService"]);
    let warm = run(&["find", "AuthService"]);
    assert_eq!(cold, warm, "warm cache hit must match cold parse output");
}

#[test]
fn outline_returns_same_results_warm_and_cold() {
    let cold = run(&["outline", "src/auth.ts"]);
    let warm = run(&["outline", "src/auth.ts"]);
    assert_eq!(cold, warm);
}

#[test]
fn cache_invalidates_when_file_content_changes() {
    let scratch = ScratchRepo::new("invalidate");
    scratch.write("a.rs", "pub fn first() {}\n");

    let before = scratch.run(&["find", "first"]);
    let matches = before["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["qualname"], "first");

    // Sleep past mtime resolution then rewrite. Different size + different
    // content guarantees the (mtime, size) cache key changes.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    scratch.write("a.rs", "pub fn second_renamed() {}\n");

    let after_rename = scratch.run(&["find", "second_renamed"]);
    let after_matches = after_rename["matches"].as_array().unwrap();
    assert_eq!(after_matches.len(), 1);
    assert_eq!(after_matches[0]["qualname"], "second_renamed");

    let stale = scratch.run(&["find", "first"]);
    assert!(
        stale["matches"].as_array().unwrap().is_empty(),
        "post-edit find for old symbol must not return cached matches: {stale:?}"
    );
}

#[test]
fn scoped_walk_does_not_prune_full_repo_entries() {
    let scratch = ScratchRepo::new("scoped-prune");
    scratch.write("alpha/foo.rs", "pub fn alpha_only() {}\n");
    scratch.write("beta/bar.rs", "pub fn beta_only() {}\n");

    // Full-repo walk populates entries for both files.
    let full = scratch.run(&["find", "_only"]);
    assert_eq!(flatten_find_matches(&full).len(), 2);

    // Scoped walk visits only alpha/, but must NOT prune beta/ from the cache.
    let alpha = scratch.run(&["find", "alpha_only", "alpha"]);
    assert_eq!(flatten_find_matches(&alpha).len(), 1);

    // Subsequent full walk should still find beta_only ~ if scoped walk had
    // pruned, beta would just be re-parsed and we couldn't observe pruning,
    // but the cache file size shouldn't shrink. The behavioral invariant we
    // can check from the outside: the search terminates with both matches.
    let again = scratch.run(&["find", "_only"]);
    assert_eq!(flatten_find_matches(&again).len(), 2);
}

#[test]
fn truncated_find_does_not_prune_full_repo_entries() {
    let scratch = ScratchRepo::new("truncated-find-prune");
    scratch.write("alpha/foo.rs", "pub fn alpha_only() {}\n");
    scratch.write("beta/bar.rs", "pub fn beta_only() {}\n");

    let full = scratch.run(&["find", "_only"]);
    assert_eq!(flatten_find_matches(&full).len(), 2);
    let warm = scratch.run(&["cache", "status"]);
    assert!(warm["entry_count"].as_u64().unwrap() >= 2);

    let limited = scratch.run(&["find", "_only", "--limit", "1"]);
    assert_eq!(limited["truncated"], true);

    let status = scratch.run(&["cache", "status"]);
    assert!(
        status["entry_count"].as_u64().unwrap() >= 2,
        "truncated find must not prune warmed cache entries: {status:?}"
    );
}

#[test]
fn truncated_search_does_not_prune_full_repo_entries() {
    let scratch = ScratchRepo::new("truncated-search-prune");
    scratch.write("alpha/foo.rs", "pub fn alpha_only() {}\n");
    scratch.write("beta/bar.rs", "pub fn beta_only() {}\n");

    let full = scratch.run(&["search", "_only"]);
    assert_eq!(count_search_matches(&full), 2);
    let warm = scratch.run(&["cache", "status"]);
    assert!(warm["entry_count"].as_u64().unwrap() >= 2);

    let limited = scratch.run(&["search", "_only", "--limit", "1"]);
    assert_eq!(limited["truncated"], true);

    let status = scratch.run(&["cache", "status"]);
    assert!(
        status["entry_count"].as_u64().unwrap() >= 2,
        "truncated search must not prune warmed cache entries: {status:?}"
    );
}

#[test]
fn cache_status_when_empty_shows_no_file() {
    let scratch = ScratchRepo::new("status-empty");
    let value = scratch.run(&["cache", "status"]);
    assert_eq!(value["enabled"], true);
    assert_eq!(value["disabled_via_env"], false);
    assert_eq!(value["exists"], false);
    assert_eq!(value["entry_count"], 0);
    assert_eq!(value["languages"].as_array().unwrap().len(), 0);
    // Stored fields elided when the file doesn't exist.
    assert!(value.get("stored_version").is_none());
    assert!(value.get("stored_repo_root").is_none());
    let cache_dir = value["cache_dir"].as_str().unwrap();
    assert!(
        cache_dir.starts_with(scratch.cache_dir.to_str().unwrap()),
        "cache_dir should live under HITAGI_CACHE_DIR, got {cache_dir}"
    );
}

#[test]
fn cache_status_reports_disabled_when_no_cache_root_can_be_resolved() {
    let scratch = ScratchRepo::new("status-no-root");
    let output = Command::cargo_bin("hitagi")
        .unwrap()
        .env_remove("HITAGI_CACHE_DIR")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("HOME")
        .env_remove("HITAGI_NO_CACHE")
        .arg("--repo")
        .arg(&scratch.repo)
        .arg("--json")
        .args(["cache", "status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(value["enabled"], false);
    assert_eq!(value["disabled_via_env"], false);
    assert!(value.get("cache_dir").is_none());
    assert!(value.get("cache_file").is_none());
}

#[test]
fn cache_ignores_empty_xdg_cache_home_and_falls_back_to_home() {
    let scratch = ScratchRepo::new("xdg-empty");
    let home = scratch.repo.parent().unwrap().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let output = Command::cargo_bin("hitagi")
        .unwrap()
        .env_remove("HITAGI_CACHE_DIR")
        .env("XDG_CACHE_HOME", "")
        .env("HOME", &home)
        .env_remove("HITAGI_NO_CACHE")
        .arg("--repo")
        .arg(&scratch.repo)
        .arg("--json")
        .args(["cache", "path"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    let path = value["path"].as_str().unwrap();
    let expected_prefix = home.join(".cache").join("hitagi");

    assert!(
        path.starts_with(expected_prefix.to_str().unwrap()),
        "empty XDG_CACHE_HOME should fall back under HOME, got {path}"
    );
}

#[test]
fn cache_clear_all_ignores_relative_xdg_cache_home() {
    let scratch = ScratchRepo::new("xdg-relative-clear-all");
    let cwd = scratch.repo.parent().unwrap().join("cwd");
    let dangerous = cwd.join("hitagi");
    let home = scratch.repo.parent().unwrap().join("home");
    std::fs::create_dir_all(&dangerous).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(dangerous.join("sentinel.txt"), "do not delete").unwrap();

    let output = Command::cargo_bin("hitagi")
        .unwrap()
        .current_dir(&cwd)
        .env_remove("HITAGI_CACHE_DIR")
        .env("XDG_CACHE_HOME", "hitagi")
        .env("HOME", &home)
        .env_remove("HITAGI_NO_CACHE")
        .arg("--repo")
        .arg(&scratch.repo)
        .arg("--json")
        .args(["cache", "clear", "--all"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(value["scope"], "all");
    assert_eq!(value["cleared"], false);
    assert!(
        dangerous.join("sentinel.txt").exists(),
        "relative XDG_CACHE_HOME must not let --all delete ./hitagi"
    );
}

#[test]
fn relative_hitagi_cache_dir_disables_cache_resolution() {
    let scratch = ScratchRepo::new("custom-relative");
    let cwd = scratch.repo.parent().unwrap().join("cwd");
    let dangerous = cwd.join("hitagi");
    let home = scratch.repo.parent().unwrap().join("home");
    std::fs::create_dir_all(&dangerous).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(dangerous.join("sentinel.txt"), "do not delete").unwrap();

    let output = Command::cargo_bin("hitagi")
        .unwrap()
        .current_dir(&cwd)
        .env("HITAGI_CACHE_DIR", "hitagi")
        .env("XDG_CACHE_HOME", home.join(".xdg-cache"))
        .env("HOME", &home)
        .env_remove("HITAGI_NO_CACHE")
        .arg("--repo")
        .arg(&scratch.repo)
        .arg("--json")
        .args(["cache", "clear", "--all"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(value["scope"], "all");
    assert_eq!(value["cleared"], false);
    assert_eq!(value["repos_removed"], 0);
    assert!(value["path"].as_str().unwrap().is_empty());
    assert!(
        dangerous.join("sentinel.txt").exists(),
        "relative HITAGI_CACHE_DIR must not let --all delete ./hitagi"
    );
}

#[test]
fn cache_status_after_populate_reports_languages() {
    let scratch = ScratchRepo::new("status-warm");
    scratch.write("a.rs", "pub fn first() {}\n");
    scratch.write("b.ts", "export function second() {}\n");
    let _ = scratch.run(&["find", "first"]);

    let value = scratch.run(&["cache", "status"]);
    assert_eq!(value["exists"], true);
    assert!(value["size_bytes"].as_u64().unwrap() > 0);
    assert_eq!(value["version_match"], true);
    assert_eq!(value["repo_root_match"], true);
    assert!(value["entry_count"].as_u64().unwrap() >= 2);
    let langs: Vec<&str> = value["languages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["language"].as_str().unwrap())
        .collect();
    assert!(langs.contains(&"rust"));
    assert!(langs.contains(&"typescript"));
}

#[test]
fn cache_path_returns_repo_subdir() {
    let scratch = ScratchRepo::new("path");
    let value = scratch.run(&["cache", "path"]);
    let path = value["path"].as_str().unwrap();
    assert!(
        path.starts_with(scratch.cache_dir.to_str().unwrap()),
        "cache path should be under HITAGI_CACHE_DIR, got {path}"
    );
    // The subdir is the repo hash, not the bare cache dir.
    assert_ne!(path, scratch.cache_dir.to_str().unwrap());
}

#[test]
fn cache_default_subcommand_is_status() {
    let scratch = ScratchRepo::new("default");
    let with_status = scratch.run(&["cache", "status"]);
    let without = scratch.run(&["cache"]);
    assert_eq!(with_status, without);
}

#[test]
fn cache_clear_removes_repo_dir() {
    let scratch = ScratchRepo::new("clear-repo");
    scratch.write("a.rs", "pub fn keep() {}\n");
    let _ = scratch.run(&["find", "keep"]);

    let pre = scratch.run(&["cache", "status"]);
    assert_eq!(pre["exists"], true);

    let cleared = scratch.run(&["cache", "clear"]);
    assert_eq!(cleared["scope"], "repo");
    assert_eq!(cleared["cleared"], true);
    assert!(cleared.get("repos_removed").is_none());

    let post = scratch.run(&["cache", "status"]);
    assert_eq!(post["exists"], false);
    assert_eq!(post["entry_count"], 0);
}

#[test]
fn cache_clear_when_missing_reports_nothing_cleared() {
    let scratch = ScratchRepo::new("clear-missing");
    let cleared = scratch.run(&["cache", "clear"]);
    assert_eq!(cleared["scope"], "repo");
    assert_eq!(cleared["cleared"], false);
}

#[test]
fn cache_clear_all_removes_every_repo() {
    let scratch = ScratchRepo::new("clear-all");
    scratch.write("a.rs", "pub fn one() {}\n");
    let _ = scratch.run(&["find", "one"]);

    // Also point a second pseudo-repo at the same cache to populate a sibling
    // subdir under HITAGI_CACHE_DIR.
    let other = scratch.repo.parent().unwrap().join("other-repo");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("b.rs"), "pub fn two() {}\n").unwrap();
    let _ = run_in(&other, &scratch.cache_dir, &["find", "two"]);

    let entries_before: Vec<_> = std::fs::read_dir(&scratch.cache_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert!(
        entries_before.len() >= 2,
        "expected at least 2 repo subdirs, found {}",
        entries_before.len()
    );

    let cleared = scratch.run(&["cache", "clear", "--all"]);
    assert_eq!(cleared["scope"], "all");
    assert_eq!(cleared["cleared"], true);
    assert!(cleared["repos_removed"].as_u64().unwrap() >= 2);

    assert!(
        !scratch.cache_dir.exists(),
        "cache root should be gone after --all"
    );
}

#[test]
fn no_cache_env_disables_persistence() {
    let scratch = ScratchRepo::new("no-cache");
    scratch.write("a.rs", "pub fn keep_me() {}\n");

    // First run with cache disabled.
    let value = Command::cargo_bin("hitagi")
        .unwrap()
        .env("HITAGI_CACHE_DIR", &scratch.cache_dir)
        .env("HITAGI_NO_CACHE", "1")
        .arg("--repo")
        .arg(&scratch.repo)
        .arg("--json")
        .args(["find", "keep_me"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let parsed: Value = serde_json::from_slice(&value).unwrap();
    assert_eq!(parsed["matches"].as_array().unwrap().len(), 1);

    // Cache file must not have been written.
    let entries = std::fs::read_dir(&scratch.cache_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .count();
    assert_eq!(
        entries, 0,
        "HITAGI_NO_CACHE must skip persistence; got {entries} entries in cache dir"
    );
}

#[test]
fn outline_includes_total_symbols_and_kind_counts_always() {
    let value = run(&["outline", "src/auth.ts"]);
    assert!(
        value.get("total_symbols").is_some(),
        "outline response must include total_symbols"
    );
    assert!(
        value.get("kind_counts").is_some(),
        "outline response must include kind_counts"
    );
    let total = value["total_symbols"].as_u64().unwrap() as usize;
    let symbols_len = value["symbols"].as_array().unwrap().len();
    assert_eq!(
        total, symbols_len,
        "small file is not auto-summarized, so total_symbols == symbols.len()"
    );
    let kind_counts = value["kind_counts"].as_object().unwrap();
    assert!(!kind_counts.is_empty());
    let counted: u64 = kind_counts.values().map(|v| v.as_u64().unwrap()).sum();
    assert_eq!(counted as usize, total);
    assert!(
        value.get("auto_summarized").is_none(),
        "auto_summarized hidden when false"
    );
}

#[test]
fn outline_auto_summarizes_when_symbol_count_exceeds_threshold() {
    let scratch = ScratchRepo::new("outline-auto-summary");
    // Generate a Rust file with > 500 top-level functions ~ exceeds the
    // OUTLINE_AUTO_SUMMARY_THRESHOLD (currently 500).
    let mut body = String::new();
    for i in 0..600 {
        body.push_str(&format!("pub fn fn_{i:04}() {{}}\n"));
    }
    scratch.write("big.rs", &body);

    let value = scratch.run(&["outline", "big.rs"]);
    assert_eq!(value["language"], "rust");
    assert_eq!(value["total_symbols"], 600);
    assert_eq!(value["auto_summarized"], true);
    assert!(
        value["note"]
            .as_str()
            .unwrap()
            .contains("auto-applied --depth 1"),
        "expected auto-summary note, got {:?}",
        value["note"]
    );
    let symbols_len = value["symbols"].as_array().unwrap().len();
    assert_eq!(
        symbols_len, 600,
        "all 600 fns are top-level, so depth=1 keeps them all (auto-summary doesn't drop top-level entries)"
    );
}

#[test]
fn outline_auto_summary_collapses_nested_symbols() {
    let scratch = ScratchRepo::new("outline-auto-summary-nested");
    // 50 enums, each with 12 variants ~ 50 + 600 = 650 total symbols. Under
    // depth=1, only the 50 enum entries should remain.
    let mut body = String::new();
    for i in 0..50 {
        body.push_str(&format!("pub enum E{i:02} {{\n"));
        for j in 0..12 {
            body.push_str(&format!("    V{j:02},\n"));
        }
        body.push_str("}\n");
    }
    scratch.write("enums.rs", &body);

    let value = scratch.run(&["outline", "enums.rs"]);
    assert_eq!(value["total_symbols"], 650);
    assert_eq!(value["auto_summarized"], true);
    let symbols_len = value["symbols"].as_array().unwrap().len();
    assert_eq!(
        symbols_len, 50,
        "depth=1 should drop the variants under each enum"
    );
    let kinds = value["kind_counts"].as_object().unwrap();
    assert_eq!(kinds["enum"], 50);
    assert_eq!(kinds["variant"], 600);
}

#[test]
fn outline_respects_explicit_depth_even_when_large() {
    let scratch = ScratchRepo::new("outline-explicit-depth");
    let mut body = String::new();
    for i in 0..600 {
        body.push_str(&format!("pub fn fn_{i:04}() {{}}\n"));
    }
    scratch.write("big.rs", &body);

    let value = scratch.run(&["outline", "big.rs", "--depth", "5"]);
    assert_eq!(value["total_symbols"], 600);
    assert!(
        value.get("auto_summarized").is_none(),
        "explicit --depth opts out of auto-summary"
    );
    assert_eq!(value["symbols"].as_array().unwrap().len(), 600);
}

#[test]
fn find_truncated_lists_unsampled_top_level_dirs() {
    let scratch = ScratchRepo::new("find-unsampled-dirs");
    // Three sibling top-level dirs. Pass paths explicitly so the walk visits
    // them in the user-provided order regardless of OS readdir() ordering.
    scratch.write("aaa/one.rs", "pub fn target() {}\n");
    scratch.write("bbb/two.rs", "pub fn target() {}\n");
    scratch.write("ccc/three.rs", "pub fn target() {}\n");

    let value = scratch.run(&["find", "target", "--limit", "1", "aaa", "bbb", "ccc"]);
    assert_eq!(value["truncated"], true);

    let unsampled: Vec<&str> = value["unsampled_dirs"]
        .as_array()
        .expect("unsampled_dirs must be present when truncated")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(unsampled, vec!["bbb", "ccc"]);
}

#[test]
fn find_full_walk_omits_unsampled_dirs() {
    let scratch = ScratchRepo::new("find-no-unsampled");
    scratch.write("aaa/one.rs", "pub fn target() {}\n");
    scratch.write("bbb/two.rs", "pub fn target() {}\n");

    let value = scratch.run(&["find", "target", "--limit", "50"]);
    assert!(
        value.get("truncated").is_none(),
        "walk should complete with limit 50"
    );
    assert!(
        value.get("unsampled_dirs").is_none(),
        "unsampled_dirs must be omitted when not truncated"
    );
}

#[test]
fn search_truncated_lists_unsampled_top_level_dirs() {
    let scratch = ScratchRepo::new("search-unsampled-dirs");
    scratch.write("aaa/notes.txt", "needle here\n");
    scratch.write("bbb/notes.txt", "needle here\n");
    scratch.write("ccc/notes.txt", "needle here\n");

    let value = scratch.run(&["search", "needle", "--limit", "1", "aaa", "bbb", "ccc"]);
    assert_eq!(value["truncated"], true);
    let unsampled: Vec<&str> = value["unsampled_dirs"]
        .as_array()
        .expect("unsampled_dirs must be present when truncated")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(unsampled, vec!["bbb", "ccc"]);
}

#[test]
fn find_round_robin_samples_across_top_level_dirs() {
    // Without paths, find should fairly sample across top-level buckets.
    // aaa/ has THREE files that match; bbb/ and ccc/ have one each. With the
    // old alphabetical walk and --limit 3, all 3 matches would come from
    // aaa/. Round-robin file order pulls one from each bucket in turn.
    let scratch = ScratchRepo::new("find-round-robin");
    scratch.write("aaa/one.rs", "pub fn target_a1() {}\n");
    scratch.write("aaa/two.rs", "pub fn target_a2() {}\n");
    scratch.write("aaa/three.rs", "pub fn target_a3() {}\n");
    scratch.write("bbb/x.rs", "pub fn target_b1() {}\n");
    scratch.write("ccc/x.rs", "pub fn target_c1() {}\n");

    let value = scratch.run(&["find", "target", "--limit", "3"]);
    let names: Vec<String> = flatten_find_matches(&value)
        .iter()
        .map(|m| m["qualname"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n.starts_with("target_a")),
        "expected at least one match from aaa/, got {names:?}"
    );
    assert!(
        names.iter().any(|n| n.starts_with("target_b")),
        "expected at least one match from bbb/, got {names:?}"
    );
    assert!(
        names.iter().any(|n| n.starts_with("target_c")),
        "expected at least one match from ccc/, got {names:?}"
    );
}

#[test]
fn search_round_robin_samples_across_top_level_dirs() {
    let scratch = ScratchRepo::new("search-round-robin");
    scratch.write("aaa/one.txt", "needle\n");
    scratch.write("aaa/two.txt", "needle\n");
    scratch.write("aaa/three.txt", "needle\n");
    scratch.write("bbb/x.txt", "needle\n");
    scratch.write("ccc/x.txt", "needle\n");

    let value = scratch.run(&["search", "needle", "--limit", "3"]);
    let keys = flatten_search_keys(&value);
    assert!(keys.iter().any(|k| k.starts_with("aaa/")), "got {keys:?}");
    assert!(keys.iter().any(|k| k.starts_with("bbb/")), "got {keys:?}");
    assert!(keys.iter().any(|k| k.starts_with("ccc/")), "got {keys:?}");
}

#[test]
fn find_path_scoped_walk_preserves_user_order() {
    // When paths are given, do NOT round-robin ~ honor user-supplied order.
    // This pins down the existing behavior locked in by find_limit_preserves_
    // requested_path_order against the new round-robin code path.
    let scratch = ScratchRepo::new("find-path-order-preserved");
    scratch.write("aaa/a.rs", "pub fn target_a() {}\n");
    scratch.write("bbb/b.rs", "pub fn target_b() {}\n");

    // Pass bbb FIRST: with round-robin gated on paths.is_empty(), the walk
    // should still honor that and return target_b first.
    let value = scratch.run(&["find", "target", "--limit", "1", "bbb", "aaa"]);
    let matches = flatten_find_matches(&value);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["qualname"], "target_b");
}

#[test]
fn find_per_file_caps_matches_and_reports_overflow() {
    // src/auth.ts has class AuthService + handleAuth + validateInput. "Auth"
    // matches all three; --per-file 1 keeps one and tallies 2 in more_in_file.
    let value = run(&["find", "Auth", "--per-file", "1", "src/auth.ts"]);
    let matches = flatten_find_matches(&value);
    assert_eq!(matches.len(), 1);
    let suppressed: u64 = value["more_in_file"]
        .as_object()
        .or_else(|| {
            value.get("groups").and_then(|g| g.as_array())?.first()?["more_in_file"].as_object()
        })
        .expect("more_in_file should be set when --per-file capped a file")
        .values()
        .next()
        .and_then(|v| v.as_u64())
        .unwrap();
    assert_eq!(suppressed, 2, "{value:?}");
}

#[test]
fn find_per_file_defaults_to_five() {
    let scratch = ScratchRepo::new("find-default-per-file");
    scratch.write(
        "src/many.rs",
        "pub struct TargetOne {}\npub struct TargetTwo {}\npub struct TargetThree {}\npub struct TargetFour {}\npub struct TargetFive {}\npub struct TargetSix {}\n",
    );

    let value = scratch.run(&["find", "Target", "src/many.rs"]);
    let matches = flatten_find_matches(&value);
    assert_eq!(matches.len(), 5);
    let suppressed: u64 = value["more_in_file"]
        .as_object()
        .expect("default --per-file should report overflow")
        .values()
        .next()
        .and_then(|v| v.as_u64())
        .unwrap();
    assert_eq!(suppressed, 1);
}

#[test]
fn find_per_file_zero_means_no_cap() {
    let value = run(&["find", "Auth", "--per-file", "0", "src/auth.ts"]);
    assert!(
        value.get("more_in_file").is_none(),
        "more_in_file should be omitted when --per-file is 0"
    );
    if let Some(groups) = value.get("groups").and_then(|v| v.as_array()) {
        for g in groups {
            assert!(g.get("more_in_file").is_none());
        }
    }
}

#[test]
fn find_groups_results_when_matches_span_top_levels() {
    // Two matches in two different top-level dirs ~ no global LCP, so the
    // response should switch to the grouped shape.
    let scratch = ScratchRepo::new("find-groups");
    scratch.write("aaa/a.rs", "pub fn target() {}\n");
    scratch.write("bbb/b.rs", "pub fn target() {}\n");

    let value = scratch.run(&["find", "target"]);
    assert!(
        value.get("prefix").is_none(),
        "top-level prefix should be omitted when grouped"
    );
    let groups = value["groups"]
        .as_array()
        .expect("groups should be present when matches span top-levels");
    assert_eq!(groups.len(), 2);
    let prefixes: Vec<&str> = groups
        .iter()
        .map(|g| g["prefix"].as_str().unwrap())
        .collect();
    assert!(prefixes.contains(&"aaa/"));
    assert!(prefixes.contains(&"bbb/"));
}

#[test]
fn find_stays_flat_when_single_top_level() {
    let scratch = ScratchRepo::new("find-flat-single-top");
    scratch.write(
        "only/x.rs",
        "pub fn target_one() {}\npub fn target_two() {}\n",
    );

    let value = scratch.run(&["find", "target"]);
    assert!(
        value.get("groups").is_none(),
        "groups should be omitted when matches all share a top-level dir"
    );
    assert!(value["prefix"].as_str().unwrap().starts_with("only/"));
}

#[test]
fn search_groups_results_when_spanning_top_levels() {
    let scratch = ScratchRepo::new("search-groups");
    scratch.write("aaa/a.txt", "needle\n");
    scratch.write("bbb/b.txt", "needle\n");

    let value = scratch.run(&["search", "needle"]);
    let flat_results = value["results"].as_object().unwrap();
    assert!(
        flat_results.is_empty(),
        "top-level results should be empty when grouped"
    );
    let groups = value["groups"]
        .as_array()
        .expect("groups should be present when matches span top-levels");
    assert_eq!(groups.len(), 2);
}

#[test]
fn find_grouped_per_file_overflow_lives_inside_group() {
    // When matches span top-levels (grouped) AND --per-file caps a file,
    // the overflow tally should live inside the group, not at the top level.
    let scratch = ScratchRepo::new("find-grouped-overflow");
    scratch.write(
        "aaa/a.rs",
        "pub fn target_one() {}\npub fn target_two() {}\npub fn target_three() {}\n",
    );
    scratch.write("bbb/b.rs", "pub fn target() {}\n");

    let value = scratch.run(&["find", "target", "--per-file", "1"]);
    assert!(
        value.get("more_in_file").is_none(),
        "top-level more_in_file should be omitted in grouped responses"
    );
    let groups = value["groups"].as_array().unwrap();
    let aaa_group = groups
        .iter()
        .find(|g| g["prefix"].as_str() == Some("aaa/"))
        .expect("aaa/ group should exist");
    let overflow = aaa_group["more_in_file"]
        .as_object()
        .expect("aaa/ group should report overflow");
    let suppressed: u64 = overflow.values().next().unwrap().as_u64().unwrap();
    assert_eq!(suppressed, 2);
}

#[test]
fn langs_populates_cache_for_non_parseable_files() {
    let scratch = ScratchRepo::new("langs-cache-non-parseable");
    scratch.write("README.md", "line one\nline two\nline three\n");
    scratch.write("data.json", "{\"a\":1}\n");

    // First call: empty cache, langs walks files.
    let first = scratch.run(&["langs"]);
    let by_lang: std::collections::HashMap<String, &Value> = first["languages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| (v["language"].as_str().unwrap().to_string(), v))
        .collect();
    assert_eq!(by_lang["markdown"]["files"], 1);
    assert_eq!(by_lang["markdown"]["lines"], 3);
    assert_eq!(by_lang["json"]["files"], 1);
    assert_eq!(by_lang["json"]["lines"], 1);

    // Cache should now hold lang-only entries for the two non-parseable files.
    let status = scratch.run(&["cache", "status"]);
    assert_eq!(status["entry_count"], 2);

    // Warm call: same numbers, served from cache. (This also exercises the
    // lookup_line_count path; if it were broken, the warm response would
    // diverge from the cold one.)
    let warm = scratch.run(&["langs"]);
    assert_eq!(first, warm);
}

#[test]
fn langs_reuses_line_counts_after_find_warms_cache() {
    let scratch = ScratchRepo::new("langs-reuses-find-cache");
    scratch.write("a.rs", "pub fn one() {}\npub fn two() {}\n");
    scratch.write("b.rs", "pub fn three() {}\n");

    // Find populates the cache with full FileEntry (symbols + line_count).
    let _ = scratch.run(&["find", "one"]);
    let status_after_find = scratch.run(&["cache", "status"]);
    assert!(status_after_find["entry_count"].as_u64().unwrap() >= 2);

    // langs should now serve line counts from cache without rewriting entries.
    let langs = scratch.run(&["langs"]);
    let by_lang: std::collections::HashMap<String, &Value> = langs["languages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| (v["language"].as_str().unwrap().to_string(), v))
        .collect();
    assert_eq!(by_lang["rust"]["files"], 2);
    assert_eq!(by_lang["rust"]["lines"], 3);
    assert_eq!(by_lang["rust"]["blank"], 0);
    assert_eq!(by_lang["rust"]["comment"], 0);
    assert_eq!(by_lang["rust"]["code"], 3);

    // outline still works after langs ran ~ verifies langs didn't stamp
    // empty-symbols entries over parseable files.
    let outline = scratch.run(&["outline", "a.rs"]);
    assert!(outline["total_symbols"].as_u64().unwrap() >= 2);
    let qualnames: Vec<String> = outline["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["qualname"].as_str().unwrap().to_string())
        .collect();
    assert!(qualnames.contains(&"one".to_string()));
    assert!(qualnames.contains(&"two".to_string()));
}

#[test]
fn langs_breaks_out_blank_and_comment() {
    let scratch = ScratchRepo::new("langs-blank-comment");
    // 6 lines: 1 line-comment, 1 blank, 1 code, 2 block-comment, 1 code
    scratch.write(
        "a.rs",
        "// hi\n\nfn x() {}\n/* multi\n   line */\nfn y() {}\n",
    );

    let langs = scratch.run(&["langs"]);
    let rust = langs["languages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["language"] == "rust")
        .expect("rust entry");

    assert_eq!(rust["files"], 1);
    assert_eq!(rust["lines"], 6);
    assert_eq!(rust["blank"], 1);
    assert_eq!(rust["comment"], 3);
    assert_eq!(rust["code"], 2);
}

#[test]
fn find_truncated_within_single_top_level_omits_unsampled_dirs() {
    // When the truncated walk only ever touched one top-level dir, there's
    // nothing to flag ~ the field stays empty/omitted.
    let scratch = ScratchRepo::new("find-single-top-truncated");
    scratch.write("only/a.rs", "pub fn target() {}\n");
    scratch.write("only/b.rs", "pub fn target() {}\n");

    let value = scratch.run(&["find", "target", "--limit", "1", "only"]);
    assert_eq!(value["truncated"], true);
    assert!(
        value.get("unsampled_dirs").is_none(),
        "single-subtree walk emits no unsampled_dirs hint"
    );
}
