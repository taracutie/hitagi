//! Dense (cosine-similarity index) implementation.
//!
//! Vectors are L2-normalized at construction so the per-query cosine
//! reduces to a dot product. Without a selector we run a single
//! `Array2::dot(&Array1)` matvec ~ ndarray's matrixmultiply backend
//! gives us SIMD-vectorized contiguous access in one pass. With a
//! selector we fall back to per-row dots in parallel via rayon, since
//! a sparse-row matvec would compute a lot of unused scores.

use ndarray::{Array1, Array2, Axis};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use serde::{Deserialize, Serialize};

use super::ranking::truncate_top_k;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DenseIndex {
    vectors: Array2<f32>,
}

impl DenseIndex {
    pub fn new(mut vectors: Array2<f32>) -> Self {
        for mut row in vectors.axis_iter_mut(Axis(0)) {
            let norm = row.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm > 1e-8 {
                row.mapv_inplace(|v| v / norm);
            }
        }
        Self { vectors }
    }

    /// Construct without re-normalizing. Use when the caller has already
    /// produced unit-norm rows (the engine builder does this before
    /// persisting; the lossless f32 on-disk blob preserves it exactly).
    /// Saves an O(rows * dim) sweep per warm hybrid/semantic search.
    pub fn from_normalized(vectors: Array2<f32>) -> Self {
        Self { vectors }
    }

    pub fn len(&self) -> usize {
        self.vectors.shape()[0]
    }

    pub fn dim(&self) -> usize {
        self.vectors.shape()[1]
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn query(
        &self,
        vector: &Array1<f32>,
        k: usize,
        selector: Option<&[usize]>,
    ) -> Vec<(usize, f32)> {
        if k == 0 || self.is_empty() {
            return Vec::new();
        }
        if selector.is_some_and(|s| s.is_empty()) {
            return Vec::new();
        }
        let mut scores: Vec<(usize, f32)> = match selector {
            Some(candidates) => candidates
                .par_iter()
                .map(|&idx| (idx, self.vectors.row(idx).dot(vector)))
                .collect(),
            None => {
                // ndarray's matrixmultiply-backed gemv: a single contiguous
                // SIMD pass over the whole matrix is faster than rayon
                // parallelism over per-row hand-rolled zip+fold.
                let dots = self.vectors.dot(vector);
                dots.iter().copied().enumerate().collect()
            }
        };
        truncate_top_k(&mut scores, k);
        scores
    }
}

#[cfg(test)]
mod tests {
    use super::DenseIndex;
    use ndarray::array;

    #[test]
    fn query_respects_selector_and_top_k_order() {
        let index = DenseIndex::new(array![[1.0, 0.0], [0.9, 0.1], [0.0, 1.0]]);
        let results = index.query(&array![1.0, 0.0], 1, Some(&[1, 2]));
        assert_eq!(results, vec![(1, results[0].1)]);
        assert!(results[0].1 > 0.9);
    }

    #[test]
    fn query_with_empty_selector_returns_no_candidates() {
        let index = DenseIndex::new(array![[1.0, 0.0], [0.0, 1.0]]);
        let results = index.query(&array![1.0, 0.0], 10, Some(&[]));
        assert!(results.is_empty());
    }

    #[test]
    fn unfiltered_query_matches_all_row_selector() {
        let index = DenseIndex::new(array![
            [1.0, 0.0, 0.2],
            [0.9, 0.2, 0.1],
            [0.0, 1.0, 0.3],
            [0.5, 0.5, 0.5]
        ]);
        let query = array![0.8, 0.2, 0.1];
        let all_rows = [0, 1, 2, 3];

        let unfiltered = index.query(&query, 3, None);
        let selected = index.query(&query, 3, Some(&all_rows));

        assert_eq!(unfiltered.len(), selected.len());
        for ((left_id, left_score), (right_id, right_score)) in
            unfiltered.iter().zip(selected.iter())
        {
            assert_eq!(left_id, right_id);
            assert!((left_score - right_score).abs() < 1e-6);
        }
    }
}
