use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use assert_cmd::Command;
use serde::Deserialize;
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
    let output = Command::cargo_bin("hitagi")
        .unwrap()
        .env("HITAGI_CACHE_DIR", shared_cache_dir())
        .arg("--repo")
        .arg(repo)
        .arg("--json")
        .arg("search")
        .arg(query)
        .arg("--hashing")
        .arg("-k")
        .arg("10")
        .args(extra_args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("stdout is valid JSON")
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
