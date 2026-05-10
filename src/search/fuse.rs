//! Search dispatch: BM25-only, semantic-only, hybrid (RRF + boosts).
//!
//! Hybrid over-fetches `top_k * 9` candidates from each side so the RRF
//! fusion + query/file boosts have enough room to rerank meaningfully ~
//! truncating to `top_k` first throws away half the signal that boosts
//! depend on.

use std::collections::{BTreeMap, HashMap};

use ndarray::Array2;

use super::dense::DenseIndex;
use super::ranking::{
    apply_query_boost_in_place, boost_multi_chunk_files, rerank_topk, resolve_alpha, QueryIntent,
};
use super::sparse::Bm25Index;
use super::types::{IndexedChunk, RankedHit, SearchMode};

const RRF_K: f32 = 60.0;

pub trait QueryEncoder {
    fn encode_query(&self, query: &str) -> Array2<f32>;
}

pub fn search_bm25(
    query: &str,
    bm25_index: &Bm25Index,
    chunks: &[IndexedChunk],
    top_k: usize,
    selector: Option<&[usize]>,
) -> Vec<RankedHit> {
    bm25_index
        .search(query, top_k, selector)
        .into_iter()
        .map(|(idx, score)| RankedHit {
            chunk: chunks[idx].clone(),
            score,
            source: SearchMode::Bm25,
        })
        .collect()
}

pub fn search_semantic<E: QueryEncoder + ?Sized>(
    query: &str,
    encoder: &E,
    semantic_index: &DenseIndex,
    chunks: &[IndexedChunk],
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
            chunk: chunks[idx].clone(),
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
    chunks: &[IndexedChunk],
    file_mapping: &BTreeMap<String, Vec<usize>>,
    top_k: usize,
    alpha: Option<f32>,
    selector: Option<&[usize]>,
) -> (Vec<RankedHit>, f32) {
    let alpha_weight = resolve_alpha(query, alpha);
    let candidate_count = top_k.saturating_mul(9).max(top_k).max(1);

    let encoded = encoder.encode_query(query);
    let vector = normalize_in_place(encoded.row(0).to_owned());

    let semantic_scores = semantic_index.query(&vector, candidate_count, selector);
    let bm25_scores = bm25_index.search(query, candidate_count, selector);

    let mut combined: HashMap<usize, f32> =
        HashMap::with_capacity(semantic_scores.len() + bm25_scores.len());
    add_rrf_scores(&mut combined, semantic_scores, alpha_weight);
    add_rrf_scores(&mut combined, bm25_scores, 1.0 - alpha_weight);

    boost_multi_chunk_files(&mut combined, chunks);
    let intent = QueryIntent::new(query);
    let boosted = apply_query_boost_in_place(
        combined,
        &intent,
        query,
        chunks,
        Some(file_mapping),
        selector,
    );
    let ranked = rerank_topk(&boosted, chunks, top_k, &intent, alpha_weight < 1.0);

    let hits = ranked
        .into_iter()
        .map(|(idx, score)| RankedHit {
            chunk: chunks[idx].clone(),
            score,
            source: SearchMode::Hybrid,
        })
        .collect();
    (hits, alpha_weight)
}

fn add_rrf_scores(combined: &mut HashMap<usize, f32>, ranked: Vec<(usize, f32)>, weight: f32) {
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
        let bm25 = search_bm25("parse_session_token", &sparse, &chunks, 1, None);
        assert_eq!(bm25[0].source, SearchMode::Bm25);
        // Smoke-test the encoder trait instantiates.
        let _ = TestEncoder.encode_query("anything");
        let _ = array![1.0f32];
    }
}
