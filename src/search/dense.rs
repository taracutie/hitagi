//! Dense (cosine-similarity index) implementation.
//!
//! Vectors are L2-normalized at construction so the per-query cosine
//! reduces to a dot product. Per-chunk dot is parallel via rayon ~ on a
//! 5k-chunk index this is the difference between 4 ms and 30 ms per
//! search call.

use ndarray::{Array1, Array2, Axis};
use rayon::iter::{IntoParallelIterator, IntoParallelRefIterator, ParallelIterator};
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
                .map(|&idx| {
                    let row = self.vectors.row(idx);
                    let score = row
                        .iter()
                        .zip(vector.iter())
                        .map(|(a, b)| a * b)
                        .sum::<f32>();
                    (idx, score)
                })
                .collect(),
            None => (0..self.len())
                .into_par_iter()
                .map(|idx| {
                    let row = self.vectors.row(idx);
                    let score = row
                        .iter()
                        .zip(vector.iter())
                        .map(|(a, b)| a * b)
                        .sum::<f32>();
                    (idx, score)
                })
                .collect(),
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
}
