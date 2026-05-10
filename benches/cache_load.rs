//! Times `ParseCache::open` + first lookup against an empty cache directory
//! ~ measures the SQLite open + bulk-SELECT scaffolding cost in isolation.
//! Baseline for any future cache work; not tied to a Part B win this round.
//!
//! We can't populate via the public API without a real `Metadata` and
//! `Language` value, and `open_at` is `#[cfg(test)]`. Instead this bench
//! redirects `HITAGI_CACHE_DIR` to a tempdir and times the cold open
//! itself, which is the dominant fixed cost on every CLI invocation.

use std::path::PathBuf;

use criterion::{criterion_group, criterion_main, Criterion};

use hitagi::cache::ParseCache;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample_repo")
}

fn fresh_cache_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hitagi-bench-cache-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn bench_cache_load(c: &mut Criterion) {
    let cache_dir = fresh_cache_dir();
    // SAFETY: bench runs single-threaded against this env var.
    unsafe {
        std::env::set_var("HITAGI_CACHE_DIR", &cache_dir);
    }
    let repo = fixture_repo();

    let mut group = c.benchmark_group("cache_load");
    group.sample_size(50);

    group.bench_function("open_empty_cache", |b| {
        b.iter(|| {
            let cache = ParseCache::open(&repo);
            std::hint::black_box(cache);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_cache_load);
criterion_main!(benches);
