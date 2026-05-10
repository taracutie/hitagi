//! Measures `apply_query_boost_in_place` (and downstream `rerank_topk`) on a
//! synthetic chunk corpus that mirrors the real search index shape: a few
//! thousand chunks across ~50 files, mixed languages, each chunk with
//! plausible identifier-bearing content. Drives the queries through the
//! public fuse layer so we exercise QueryIntent + definition_matchers in
//! the same call paths the CLI hits.

use std::collections::{BTreeMap, HashMap};

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use hitagi::search::ranking::{apply_query_boost_in_place, rerank_topk, QueryIntent};
use hitagi::search::types::IndexedChunk;

fn synth_chunk(id: usize) -> IndexedChunk {
    let file_idx = id % 60;
    let language = match file_idx % 4 {
        0 => "rust",
        1 => "typescript",
        2 => "tsx",
        _ => "python",
    };
    let path = format!("src/area_{}/file_{}.rs", file_idx % 8, file_idx);
    // Plant some real-looking definition keywords + identifiers so boosts
    // actually fire. Vary by id so bm25 / dense scores differ per chunk.
    let body = if id % 7 == 0 {
        format!(
            "pub fn parse_session_token(input: &str) -> Result<Token, Error> {{\n    // chunk {id}\n    let token = decode(input)?;\n    Ok(token)\n}}"
        )
    } else if id % 5 == 0 {
        format!("struct AuthService {{ token_store: HashMap<String, Token>, /* {id} */ }}")
    } else if id % 3 == 0 {
        format!("CREATE TABLE sessions (id TEXT PRIMARY KEY, token TEXT NOT NULL); -- {id}")
    } else {
        format!("// helper utility id={id}\nfn helper_{id}() -> usize {{ {id} }}")
    };
    IndexedChunk {
        content: body,
        file_path: path,
        start_line: 1 + id * 20,
        end_line: 20 + id * 20,
        language: Some(language.to_string()),
    }
}

fn build_corpus(n: usize) -> (Vec<IndexedChunk>, BTreeMap<String, Vec<usize>>) {
    let chunks: Vec<IndexedChunk> = (0..n).map(synth_chunk).collect();
    let mut file_mapping: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, ch) in chunks.iter().enumerate() {
        file_mapping
            .entry(ch.file_path.clone())
            .or_default()
            .push(i);
    }
    (chunks, file_mapping)
}

fn seed_scores(n: usize, take: usize) -> HashMap<usize, f32> {
    // Mimic post-RRF state: ~9*top_k candidates with descending RRF-style
    // scores. The actual distribution doesn't matter for the boost cost
    // because the boost paths inspect chunk content, not score magnitudes.
    let mut map = HashMap::with_capacity(take);
    for rank in 0..take {
        let id = (rank * 17) % n;
        let score = 1.0 / (60.0 + rank as f32);
        map.entry(id).and_modify(|s| *s += score).or_insert(score);
    }
    map
}

fn bench_apply_query_boost(c: &mut Criterion) {
    let n = 2400;
    let (chunks, file_mapping) = build_corpus(n);
    let initial = seed_scores(n, 90);

    let mut group = c.benchmark_group("ranking_boost");

    // Symbol query ~ triggers boost_symbol_definitions (regex-heavy path).
    let q = "parse_session_token";
    let intent = QueryIntent::new(q);
    group.bench_function("symbol_query", |b| {
        b.iter(|| {
            let scores = apply_query_boost_in_place(
                initial.clone(),
                &intent,
                black_box(q),
                &chunks,
                Some(&file_mapping),
                None,
            );
            let ranked = rerank_topk(&scores, &chunks, 10, &intent, true);
            black_box(ranked);
        });
    });

    // Mixed phrase ~ fires path-intent + named-non-candidates which loop
    // over the file mapping (the O(files * regex) hotspot).
    let q = "parse_session_token in auth service";
    let intent = QueryIntent::new(q);
    group.bench_function("mixed_phrase_query", |b| {
        b.iter(|| {
            let scores = apply_query_boost_in_place(
                initial.clone(),
                &intent,
                black_box(q),
                &chunks,
                Some(&file_mapping),
                None,
            );
            let ranked = rerank_topk(&scores, &chunks, 10, &intent, true);
            black_box(ranked);
        });
    });

    // Natural-language query ~ skips symbol path, exercises stem boost.
    let q = "how does session token validation work";
    let intent = QueryIntent::new(q);
    group.bench_function("natural_language_query", |b| {
        b.iter(|| {
            let scores = apply_query_boost_in_place(
                initial.clone(),
                &intent,
                black_box(q),
                &chunks,
                Some(&file_mapping),
                None,
            );
            let ranked = rerank_topk(&scores, &chunks, 10, &intent, true);
            black_box(ranked);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_apply_query_boost);
criterion_main!(benches);
