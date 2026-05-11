use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use hitagi::{
    commands::{self as app_commands, SearchModeArg, SearchOptions},
    repo::RepoRoot,
};
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

const TEST_PACK_LANGUAGES: &[&str] = &[
    "rust",
    "typescript",
    "tsx",
    "markdown",
    "json",
    "javascript",
];

#[derive(Debug, Deserialize)]
struct GoldenQuery {
    query: String,
    expected_path: String,
    max_rank: usize,
}

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("search_relevance")
}

fn shared_cache_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "hitagi-search-relevance-{}-cache",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    })
}

fn prewarm_language_pack() {
    static PREWARM: OnceLock<()> = OnceLock::new();
    PREWARM.get_or_init(|| {
        tree_sitter_language_pack::download(TEST_PACK_LANGUAGES)
            .expect("test parser languages download");
    });
}

fn search(repo: &Path, query: &str, extra_args: &[&str]) -> Value {
    prewarm_language_pack();
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let old = std::env::var_os("HITAGI_CACHE_DIR");
    std::env::set_var("HITAGI_CACHE_DIR", shared_cache_dir());

    let mut options = SearchOptions {
        paths: Vec::new(),
        excludes: Vec::new(),
        limit: 10,
        mode: SearchModeArg::Hybrid,
        languages: Vec::new(),
        alpha: None,
        snippet: false,
        hashing: true,
        no_download: false,
        offline: false,
        model: None,
    };
    let mut i = 0;
    while i < extra_args.len() {
        match extra_args[i] {
            "--exclude" => {
                options.excludes.push(extra_args[i + 1].to_string());
                i += 2;
            }
            other => panic!("unsupported search arg {other}"),
        }
    }
    let repo = RepoRoot::new(std::fs::canonicalize(repo).unwrap());
    let response = app_commands::search(&repo, query, options).unwrap();

    match old {
        Some(old) => std::env::set_var("HITAGI_CACHE_DIR", old),
        None => std::env::remove_var("HITAGI_CACHE_DIR"),
    }
    to_value(response)
}

fn to_value<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).expect("response serializes for assertions")
}

#[test]
fn search_relevance_fixture_keeps_expected_surfaces_findable() {
    let repo = fixture_repo();
    let golden: Vec<GoldenQuery> =
        serde_json::from_str(&std::fs::read_to_string(repo.join("golden.json")).unwrap()).unwrap();

    for case in golden {
        let response = search(&repo, &case.query, &[]);
        let paths: Vec<_> = response["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|result| result["path"].as_str())
            .collect();

        assert!(
            paths
                .iter()
                .take(case.max_rank)
                .any(|path| path.ends_with(&case.expected_path)),
            "query {:?} expected {:?} in top {}, got {:?}",
            case.query,
            case.expected_path,
            case.max_rank,
            paths
        );
    }
}

#[test]
fn search_relevance_exclude_still_applies_to_rank_insertions() {
    let repo = fixture_repo();
    let response = search(
        &repo,
        "generated grammar parse hunk header",
        &["--exclude", "vendor"],
    );
    let paths: Vec<_> = response["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|result| result["path"].as_str())
        .collect();

    assert!(
        paths.iter().all(|path| !path.contains("vendor/")),
        "excluded vendor path appeared in results: {:?}",
        paths
    );
}
