//! Search dispatch: BM25-only, semantic-only, hybrid (RRF + boosts).
//!
//! Hybrid over-fetches `top_k * 9` candidates from each side so the RRF
//! fusion + query/file boosts have enough room to rerank meaningfully ~
//! truncating to `top_k` first throws away half the signal that boosts
//! depend on.

use std::collections::BTreeMap;

use ndarray::Array2;
use rustc_hash::FxHashMap;

use super::chunk_store::ChunkStore;
use super::dense::DenseIndex;
use super::ranking::{boost_multi_chunk_files, rerank_topk, resolve_alpha, QueryIntent};
use super::sparse::Bm25Index;
use super::types::{RankedHit, SearchMode};

const RRF_K: f32 = 60.0;

pub trait QueryEncoder {
    fn encode_query(&self, query: &str) -> Array2<f32>;
}

pub fn search_bm25(
    query: &str,
    bm25_index: &Bm25Index,
    chunks: &ChunkStore,
    top_k: usize,
    selector: Option<&[usize]>,
) -> Vec<RankedHit> {
    bm25_index
        .search(query, top_k, selector)
        .into_iter()
        .map(|(idx, score)| RankedHit {
            chunk: chunks.to_indexed(idx),
            score,
            source: SearchMode::Bm25,
        })
        .collect()
}

pub fn search_semantic<E: QueryEncoder + ?Sized>(
    query: &str,
    encoder: &E,
    semantic_index: &DenseIndex,
    chunks: &ChunkStore,
    top_k: usize,
    selector: Option<&[usize]>,
) -> Vec<RankedHit> {
    let encoded = encoder.encode_query(query);
    let vector = encoded.row(0).to_owned();
    let normalized = normalize_in_place(vector);
    semantic_index
        .query(&normalized, top_k, selector)
        .into_iter()
        .map(|(idx, score)| RankedHit {
            chunk: chunks.to_indexed(idx),
            score,
            source: SearchMode::Semantic,
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn search_hybrid<E: QueryEncoder + ?Sized>(
    query: &str,
    encoder: &E,
    semantic_index: &DenseIndex,
    bm25_index: &Bm25Index,
    chunks: &ChunkStore,
    file_mapping: &BTreeMap<String, Vec<usize>>,
    top_k: usize,
    alpha: Option<f32>,
    selector: Option<&[usize]>,
) -> (Vec<RankedHit>, f32) {
    let alpha_weight = resolve_alpha(query, alpha);
    let candidate_count = top_k.saturating_mul(9).max(top_k).max(1);

    let encoded = encoder.encode_query(query);
    let vector = normalize_in_place(encoded.row(0).to_owned());
    let intent = QueryIntent::new(query);

    // Three independent fan-outs: the dense matvec, the BM25 posting walk,
    // and the path-words cache build (skipped when the query has no path
    // intent). None depend on the others' results, and the boost pass
    // downstream consumes all three. Folding the cache build into the
    // join with the dense matvec hides its ~1-3 ms parallel cost behind
    // the matvec wall.
    let need_path_cache = intent.has_path_intent();
    let (semantic_scores, (bm25_scores, path_cache)) = rayon::join(
        || semantic_index.query(&vector, candidate_count, selector),
        || {
            rayon::join(
                || bm25_index.search(query, candidate_count, selector),
                || -> Option<crate::search::ranking::PathWordsCache<'_>> {
                    if !need_path_cache {
                        return None;
                    }
                    Some(crate::search::ranking::PathWordsCache::new(
                        file_mapping.keys().map(String::as_str),
                    ))
                },
            )
        },
    );

    let mut combined: FxHashMap<usize, f32> = FxHashMap::with_capacity_and_hasher(
        semantic_scores.len() + bm25_scores.len(),
        Default::default(),
    );
    add_rrf_scores(&mut combined, semantic_scores, alpha_weight);
    add_rrf_scores(&mut combined, bm25_scores, 1.0 - alpha_weight);

    boost_multi_chunk_files(&mut combined, chunks);
    let boosted = crate::search::ranking::apply_query_boost_in_place_with_cache(
        combined,
        &intent,
        query,
        chunks,
        Some(file_mapping),
        selector,
        path_cache.as_ref(),
    );
    let ranked = rerank_topk(&boosted, chunks, top_k, &intent, alpha_weight < 1.0);

    let hits = ranked
        .into_iter()
        .map(|(idx, score)| RankedHit {
            chunk: chunks.to_indexed(idx),
            score,
            source: SearchMode::Hybrid,
        })
        .collect();
    (hits, alpha_weight)
}

fn add_rrf_scores<S: std::hash::BuildHasher>(
    combined: &mut std::collections::HashMap<usize, f32, S>,
    ranked: Vec<(usize, f32)>,
    weight: f32,
) {
    for (rank, (id, _)) in ranked.into_iter().enumerate() {
        *combined.entry(id).or_default() += weight / (RRF_K + rank as f32 + 1.0);
    }
}

fn normalize_in_place(mut vector: ndarray::Array1<f32>) -> ndarray::Array1<f32> {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 1e-8 {
        vector.mapv_inplace(|v| v / norm);
    }
    vector
}

#[cfg(test)]
mod tests {
    use super::{search_bm25, QueryEncoder};
    use crate::search::chunk_store::ChunkStore;
    use crate::search::sparse::Bm25Index;
    use crate::search::types::{IndexedChunk, SearchMode};
    use ndarray::{array, Array2};

    struct TestEncoder;

    impl QueryEncoder for TestEncoder {
        fn encode_query(&self, _query: &str) -> Array2<f32> {
            let mut out = Array2::<f32>::zeros((1, 2));
            out[(0, 0)] = 1.0;
            out
        }
    }

    fn chunk(content: &str, file_path: &str) -> IndexedChunk {
        IndexedChunk {
            content: content.to_owned(),
            file_path: file_path.to_owned(),
            start_line: 1,
            end_line: 1,
            language: Some("rust".to_owned()),
        }
    }

    #[test]
    fn bm25_helper_reports_correct_source_mode() {
        let chunks = vec![chunk("fn parse_session_token() {}", "src/auth.rs")];
        let sparse = Bm25Index::build_from_chunks(&chunks);
        let store = ChunkStore::from_indexed(chunks);
        let bm25 = search_bm25("parse_session_token", &sparse, &store, 1, None);
        assert_eq!(bm25[0].source, SearchMode::Bm25);
        // Smoke-test the encoder trait instantiates.
        let _ = TestEncoder.encode_query("anything");
        let _ = array![1.0f32];
    }
}
