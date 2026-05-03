use std::path::PathBuf;

use assert_cmd::Command;
use serde_json::Value;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample_repo")
}

fn run(args: &[&str]) -> Value {
    let output = Command::cargo_bin("hitagi")
        .unwrap()
        .arg("--repo")
        .arg(fixture_repo())
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("stdout is valid JSON")
}

fn run_failure(args: &[&str]) -> String {
    let assert = Command::cargo_bin("hitagi")
        .unwrap()
        .arg("--repo")
        .arg(fixture_repo())
        .args(args)
        .assert()
        .failure();
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
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
    let names: Vec<&str> = value["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"AuthService"));
    assert!(names.contains(&"handleAuth"));
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
    assert_eq!(
        matches[0]["path"],
        "packages/mobile/src/components/Button.tsx"
    );
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
    assert_eq!(
        mobile_matches[0]["path"],
        "packages/mobile/src/components/Button.tsx"
    );
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
    assert_eq!(
        desktop_matches[0]["path"],
        "apps/desktop/src/components/Button.tsx"
    );
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
    for m in matches {
        assert!(m["path"].as_str().unwrap().starts_with("src/"));
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
    let with_keys: Vec<&str> = with["results"]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    let without_keys: Vec<&str> = without["results"]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
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
    let with_paths: Vec<&str> = with["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    let without_paths: Vec<&str> = without["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
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
fn pretty_flag_indents_output() {
    let stdout = Command::cargo_bin("hitagi")
        .unwrap()
        .arg("--repo")
        .arg(fixture_repo())
        .arg("--pretty")
        .args(["outline", "src/auth.ts"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(stdout).unwrap();
    assert!(text.contains("\n  "), "pretty output should be indented");
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
        "OUTPUT SHAPES",
    ] {
        assert!(
            text.contains(needle),
            "long help should contain `{needle}` section"
        );
    }
}
