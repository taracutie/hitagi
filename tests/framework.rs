use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use assert_cmd::Command;
use mimi::{commands as app_commands, repo::RepoRoot};
use serde::Serialize;
use serde_json::Value;

const TEST_PACK_LANGUAGES: &[&str] = &["typescript", "tsx", "javascript", "json"];

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn shared_cache_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("mimi-framework-itest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    })
}

fn prewarm() {
    static PREWARM: OnceLock<()> = OnceLock::new();
    PREWARM.get_or_init(|| {
        tree_sitter_language_pack::download(TEST_PACK_LANGUAGES)
            .expect("test parser languages download");
    });
}

fn run_structured(repo: &Path, args: &[&str]) -> Value {
    prewarm();
    let repo = RepoRoot::new(std::fs::canonicalize(repo).unwrap());
    assert_eq!(args[0], "framework");
    assert_eq!(args[1], "next");
    let root = parse_root(&args[3..]);
    match args[2] {
        "info" => to_value(app_commands::framework_next_info(&repo, root).unwrap()),
        "list-routes" => to_value(app_commands::framework_next_list_routes(&repo, root).unwrap()),
        "list-layouts" => to_value(app_commands::framework_next_list_layouts(&repo, root).unwrap()),
        "list-server-actions" => {
            to_value(app_commands::framework_next_list_server_actions(&repo, root).unwrap())
        }
        other => panic!("unsupported next action {other}"),
    }
}

fn parse_root<'a>(args: &'a [&'a str]) -> Option<&'a str> {
    let mut root = None;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--root" => {
                root = Some(args[i + 1]);
                i += 2;
            }
            other => panic!("unsupported framework arg {other}"),
        }
    }
    root
}

fn to_value<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).expect("response serializes for assertions")
}

fn run_text(repo: &Path, args: &[&str]) -> String {
    prewarm();
    let stdout = Command::cargo_bin("mimi")
        .unwrap()
        .env("MIMI_CACHE_DIR", shared_cache_dir())
        .arg("--repo")
        .arg(repo)
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(stdout).unwrap()
}

fn run_failure(repo: &Path, args: &[&str]) -> String {
    prewarm();
    let assert = Command::cargo_bin("mimi")
        .unwrap()
        .env("MIMI_CACHE_DIR", shared_cache_dir())
        .arg("--repo")
        .arg(repo)
        .args(args)
        .assert()
        .failure();
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

#[test]
fn next_info_detects_app_router() {
    let repo = fixture_path("next_app");
    let value = run_structured(&repo, &["framework", "next", "info"]);
    assert_eq!(value["framework"], "next");
    assert_eq!(value["detected"], true);
    assert_eq!(value["version"], "15.0.0");
    assert_eq!(value["router"], "app");
    assert_eq!(value["root"], ".");
    // src_layout is false for this fixture, so omitted from JSON entirely.
    assert!(value.get("src_layout").is_none());
}

#[test]
fn next_root_flag_accepts_explicit_repo_root() {
    let repo = fixture_path("next_app");

    for root in [".", "./"] {
        let value = run_structured(&repo, &["framework", "next", "info", "--root", root]);
        assert_eq!(value["detected"], true, "root={root}");
        assert_eq!(value["router"], "app", "root={root}");
        assert_eq!(value["root"], ".", "root={root}");

        let value = run_structured(&repo, &["framework", "next", "list-routes", "--root", root]);
        assert_eq!(value["root"], ".", "root={root}");
        let routes = value["routes"].as_array().expect("routes array");
        let home = routes
            .iter()
            .find(|route| route["pattern"] == "/")
            .expect("home route");
        assert_eq!(home["file"], "app/page.tsx", "root={root}");
    }
}

#[test]
fn next_info_detects_pages_router() {
    let repo = fixture_path("next_pages");
    let value = run_structured(&repo, &["framework", "next", "info"]);
    assert_eq!(value["framework"], "next");
    assert_eq!(value["router"], "pages");
    assert_eq!(value["version"], "14.2.0");
}

#[test]
fn next_info_errors_when_not_a_next_project() {
    // mimi's own repo has no `next` dep ~ so we use it as the negative case.
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let stderr = run_failure(&repo, &["framework", "next", "info"]);
    assert!(
        stderr.contains("framework not detected"),
        "expected detection error, got: {stderr}"
    );
}

#[test]
fn next_info_text_output_summarises_detection() {
    let repo = fixture_path("next_app");
    let text = run_text(&repo, &["framework", "next", "info"]);
    assert!(text.contains("next"));
    assert!(text.contains("app router"));
    assert!(text.contains("15.0.0"));
}

#[test]
fn next_list_routes_app_router_includes_pages_and_api() {
    let repo = fixture_path("next_app");
    let value = run_structured(&repo, &["framework", "next", "list-routes"]);
    let routes = value["routes"].as_array().expect("routes array");
    let by_pattern: std::collections::HashMap<String, &Value> = routes
        .iter()
        .map(|r| (r["pattern"].as_str().unwrap().to_string(), r))
        .collect();

    assert!(by_pattern.contains_key("/"), "missing /");
    assert!(by_pattern.contains_key("/about"), "missing /about");
    assert!(
        by_pattern.contains_key("/users/[id]"),
        "missing /users/[id]"
    );
    assert!(
        by_pattern.contains_key("/blog/[...slug]"),
        "missing /blog/[...slug]"
    );
    assert!(
        by_pattern.contains_key("/promo"),
        "route group should be dropped from URL"
    );
    assert!(by_pattern.contains_key("/api/users"), "missing /api/users");

    let api = by_pattern["/api/users"];
    assert_eq!(api["kind"], "api");
    assert_eq!(api["router"], "app");
    let methods: Vec<String> = api["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        methods,
        vec![
            "GET".to_string(),
            "POST".to_string(),
            "DELETE".to_string(),
            "PATCH".to_string()
        ]
    );
}

#[test]
fn next_list_routes_excludes_layouts_and_private_folders() {
    let repo = fixture_path("next_app");
    let value = run_structured(&repo, &["framework", "next", "list-routes"]);
    let routes = value["routes"].as_array().unwrap();
    for route in routes {
        let file = route["file"].as_str().unwrap();
        assert!(
            !file.ends_with("layout.tsx"),
            "layout.tsx should not appear as a route: {file}"
        );
        assert!(
            !file.ends_with("error.tsx"),
            "error.tsx should not appear as a route: {file}"
        );
        assert!(
            !file.contains("_components"),
            "private folder should be skipped: {file}"
        );
    }

    let api = routes
        .iter()
        .find(|r| r["pattern"] == "/api/users")
        .unwrap();
    let methods: Vec<&str> = api["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    assert_eq!(methods, vec!["GET", "POST", "DELETE", "PATCH"]);
}

#[test]
fn next_list_routes_pages_router_drops_special_files_and_index() {
    let repo = fixture_path("next_pages");
    let value = run_structured(&repo, &["framework", "next", "list-routes"]);
    let routes = value["routes"].as_array().unwrap();
    let patterns: Vec<&str> = routes
        .iter()
        .map(|r| r["pattern"].as_str().unwrap())
        .collect();

    assert!(patterns.contains(&"/"), "missing /");
    assert!(patterns.contains(&"/about"), "missing /about");
    assert!(patterns.contains(&"/users/[id]"), "missing /users/[id]");
    assert!(patterns.contains(&"/api/users"), "missing /api/users");
    assert!(
        patterns.contains(&"/api/posts"),
        "index.ts under api/posts should fold to /api/posts"
    );

    for route in routes {
        let file = route["file"].as_str().unwrap();
        assert!(!file.contains("_app"), "_app should be excluded: {file}");
        assert!(
            !file.contains("_document"),
            "_document should be excluded: {file}"
        );
        assert!(!file.contains("404"), "404 should be excluded: {file}");
    }

    let api = routes
        .iter()
        .find(|r| r["pattern"] == "/api/users")
        .unwrap();
    assert_eq!(api["kind"], "api");
    assert_eq!(api["router"], "pages");
}

#[test]
fn next_list_layouts_finds_root_layout_and_error() {
    let repo = fixture_path("next_app");
    let value = run_structured(&repo, &["framework", "next", "list-layouts"]);
    let layouts = value["layouts"].as_array().unwrap();
    let kinds: Vec<&str> = layouts
        .iter()
        .map(|l| l["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"layout"), "missing layout");
    assert!(kinds.contains(&"error"), "missing error");

    let layout = layouts.iter().find(|l| l["kind"] == "layout").unwrap();
    assert_eq!(layout["scope"], "/");
    assert_eq!(layout["file"], "app/layout.tsx");
}

#[test]
fn next_list_server_actions_picks_up_file_and_function_directives() {
    let repo = fixture_path("next_app");
    let value = run_structured(&repo, &["framework", "next", "list-server-actions"]);
    let actions = value["actions"].as_array().unwrap();

    let names: Vec<(String, String, String)> = actions
        .iter()
        .map(|a| {
            (
                a["name"].as_str().unwrap().to_string(),
                a["scope"].as_str().unwrap().to_string(),
                a["file"].as_str().unwrap().to_string(),
            )
        })
        .collect();

    // File-level: actions.ts has "use server" at the top — every exported async
    // function should surface.
    assert!(
        names
            .iter()
            .any(|(n, s, f)| n == "createPost" && s == "file" && f == "app/actions.ts"),
        "expected createPost as file-level server action, got: {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|(n, s, f)| n == "deletePost" && s == "file" && f == "app/actions.ts"),
        "expected deletePost as file-level server action, got: {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|(n, s, f)| n == "archivePost" && s == "file" && f == "app/actions.ts"),
        "expected export-list archivePost as file-level server action, got: {names:?}"
    );
    assert!(
        !names
            .iter()
            .any(|(n, _, f)| n == "*" && f == "app/actions.ts"),
        "export-list actions should prevent wildcard fallback: {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|(n, s, f)| n == "default" && s == "file" && f == "app/default-action.ts"),
        "expected default identifier export as file-level server action, got: {names:?}"
    );
    assert!(
        !names
            .iter()
            .any(|(n, _, f)| n == "*" && f == "app/default-action.ts"),
        "default identifier export should prevent wildcard fallback: {names:?}"
    );

    // Function-level: top-level and nested functions with "use server" inside
    // their bodies should both surface.
    assert!(
        names
            .iter()
            .any(|(n, s, f)| n == "submitForm" && s == "function" && f == "app/forms/submit.ts"),
        "expected submitForm as function-level action, got: {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|(n, s, f)| n == "saveHome" && s == "function" && f == "app/page.tsx"),
        "expected nested saveHome as function-level action, got: {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|(n, s, f)| n == "saveDraft" && s == "function" && f == "app/page.tsx"),
        "expected async arrow saveDraft as function-level action, got: {names:?}"
    );

    // Negative: functions without directives must not appear, including the
    // parent component that encloses a nested action.
    assert!(
        !names.iter().any(|(n, _, _)| n == "plainHelper"),
        "plainHelper has no directive and should not appear"
    );
    assert!(
        !names.iter().any(|(n, _, _)| n == "plainArrow"),
        "plainArrow has no directive and should not appear"
    );
    assert!(
        !names.iter().any(|(n, _, _)| n == "HomePage"),
        "HomePage encloses an action but has no directive and should not appear"
    );
}

#[test]
fn next_list_server_actions_returns_empty_for_pages_only_fixture() {
    let repo = fixture_path("next_pages");
    let value = run_structured(&repo, &["framework", "next", "list-server-actions"]);
    assert_eq!(
        value["actions"].as_array().unwrap().len(),
        0,
        "pages fixture has no server actions"
    );
}

#[test]
fn next_info_root_flag_scopes_to_subdirectory() {
    // Build a scratch monorepo: root/apps/web is the next.js app; root itself
    // has no package.json. `framework next info` at the root must fail; with
    // --root apps/web it must succeed and still emit repo-relative file paths.
    let scratch =
        std::env::temp_dir().join(format!("mimi-framework-monorepo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let app_dir = scratch.join("apps").join("web").join("app");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(
        scratch.join("apps").join("web").join("package.json"),
        r#"{"dependencies":{"next":"15.0.0"}}"#,
    )
    .unwrap();
    std::fs::write(app_dir.join("page.tsx"), "export default function P(){}\n").unwrap();
    std::fs::write(
        app_dir.join("layout.tsx"),
        "export default function L({ children }) { return children; }\n",
    )
    .unwrap();
    std::fs::write(
        app_dir.join("actions.ts"),
        "\"use server\";\n\nexport const createPost = async () => ({ ok: true });\nexport default async function save() { return { ok: true }; }\n",
    )
    .unwrap();

    let repo = std::fs::canonicalize(&scratch).unwrap();
    let stderr = run_failure(&repo, &["framework", "next", "info"]);
    assert!(
        stderr.contains("framework not detected"),
        "expected error at repo root, got: {stderr}"
    );

    let value = run_structured(&repo, &["framework", "next", "info", "--root", "apps/web"]);
    assert_eq!(value["detected"], true);
    assert_eq!(value["router"], "app");

    let value = run_structured(
        &repo,
        &["framework", "next", "list-routes", "--root", "apps/web"],
    );
    assert_eq!(value["root"], "apps/web");
    let routes = value["routes"].as_array().unwrap();
    let home = routes.iter().find(|r| r["pattern"] == "/").unwrap();
    assert_eq!(home["file"], "apps/web/app/page.tsx");

    let value = run_structured(
        &repo,
        &["framework", "next", "list-layouts", "--root", "apps/web"],
    );
    let layouts = value["layouts"].as_array().unwrap();
    let layout = layouts.iter().find(|l| l["kind"] == "layout").unwrap();
    assert_eq!(layout["file"], "apps/web/app/layout.tsx");

    let value = run_structured(
        &repo,
        &[
            "framework",
            "next",
            "list-server-actions",
            "--root",
            "apps/web",
        ],
    );
    let actions = value["actions"].as_array().unwrap();
    let names: Vec<(String, String, String)> = actions
        .iter()
        .map(|a| {
            (
                a["name"].as_str().unwrap().to_string(),
                a["scope"].as_str().unwrap().to_string(),
                a["file"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(
        names
            .iter()
            .any(|(n, s, f)| n == "createPost" && s == "file" && f == "apps/web/app/actions.ts"),
        "expected exported binding action with repo-relative file, got: {names:?}"
    );
    assert!(
        names
            .iter()
            .any(|(n, s, f)| n == "default" && s == "file" && f == "apps/web/app/actions.ts"),
        "expected default action with repo-relative file, got: {names:?}"
    );
    assert!(
        !names.iter().any(|(n, _, _)| n == "*"),
        "exported action bindings should prevent wildcard fallback: {names:?}"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn next_info_detects_src_layout() {
    // Scratch project using src/app/ instead of app/ at the root.
    let scratch = std::env::temp_dir().join(format!(
        "mimi-framework-src-layout-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&scratch);
    let app_dir = scratch.join("src").join("app");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(
        scratch.join("package.json"),
        r#"{"dependencies":{"next":"15.0.0"}}"#,
    )
    .unwrap();
    std::fs::write(app_dir.join("page.tsx"), "export default function P(){}\n").unwrap();

    let repo = std::fs::canonicalize(&scratch).unwrap();
    let value = run_structured(&repo, &["framework", "next", "info"]);
    assert_eq!(value["detected"], true);
    assert_eq!(value["router"], "app");
    assert_eq!(value["src_layout"], true);

    let _ = std::fs::remove_dir_all(&scratch);
}
