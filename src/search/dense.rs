//! Dense (cosine-similarity index) implementation.
//!
//! Vectors are L2-normalized at construction so the per-query cosine
//! reduces to a dot product. Without a selector we run a single
//! `ArrayView2::dot(&Array1)` matvec ~ ndarray's matrixmultiply backend
//! gives us SIMD-vectorized contiguous access in one pass. With a
//! selector we fall back to per-row dots in parallel via rayon, since
//! a sparse-row matvec would compute a lot of unused scores.
//!
//! The backing matrix can be either owned (`Array2<f32>`) or borrowed via
//! a memory-mapped file. On warm loads the persisted dense matrix lives
//! in the OS page cache; pointing an `ArrayView2` at the mapped bytes
//! avoids the 33 MB memcpy + extra page-fault pass that the previous
//! `read_dense_vectors_file` did on every warm hybrid/semantic search.
//! The `Mapped` variant carries the `Mmap` itself so the view lives as
//! long as the index.

use std::sync::Arc;

use memmap2::Mmap;
use ndarray::{Array1, Array2, ArrayView2, Axis};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};

use super::ranking::truncate_top_k;

/// Mmap-backed dense matrix view. Held in `Arc` so a clone (e.g. one for
/// the search session and one to keep alive for `defer_drop`) doesn't
/// re-mmap or copy bytes ~ they just bump the refcount.
#[derive(Debug)]
pub struct MappedDense {
    mmap: Arc<Mmap>,
    /// Byte offset into `mmap` where the f32 body starts. Must be a
    /// multiple of 4 (we validate at construction). The on-disk format's
    /// 16-byte header is the only thing in front, so this is always 16
    /// today; carried as a field so the cast can ride untouched if the
    /// header layout grows later.
    body_offset: usize,
    rows: usize,
    cols: usize,
}

impl MappedDense {
    /// Construct from an mmap + shape. The body must be `rows*cols*4`
    /// bytes long starting at `body_offset` and aligned on a 4-byte
    /// boundary. We verify the alignment + length so the unchecked f32
    /// slice cast is sound on every supported platform (x86_64 / aarch64
    /// LE, which is what the binary ships to).
    pub fn new(mmap: Arc<Mmap>, body_offset: usize, rows: usize, cols: usize) -> Option<Self> {
        let bytes = rows.checked_mul(cols).and_then(|n| n.checked_mul(4))?;
        if body_offset.checked_add(bytes)? > mmap.len() {
            return None;
        }
        // f32 alignment is 4; mmap is page-aligned (>= 4 K) and the
        // header is 16 bytes so the body offset inherits 4-byte alignment.
        if (mmap.as_ptr() as usize + body_offset) % std::mem::align_of::<f32>() != 0 {
            return None;
        }
        Some(Self {
            mmap,
            body_offset,
            rows,
            cols,
        })
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Fault every page of the matrix body into the process page table.
    /// `madvise(WILLNEED)` asks the kernel to prefetch asynchronously, but
    /// on WSL2 the prefetch routinely lags behind the matvec ~ the matvec
    /// then stalls per-page-fault during the dot product (we saw ~2 ms of
    /// extra wall on this path). Issuing one read per page on a rayon pool
    /// in parallel with `ensure_sparse` pays ~1 ms of parallel work to
    /// keep the matvec at hot-cache speed.
    pub fn prefault_pages(&self) {
        use rayon::prelude::*;
        const PAGE: usize = 4096;
        const STRIPE: usize = 256 * 1024;
        let body = self.rows * self.cols * 4;
        // Use a `&[u8]` view rather than the raw pointer ~ slices implement
        // `Send`/`Sync` for shared access, and the par-chunked iterator
        // can stride over them cleanly without smuggling a raw pointer
        // across the rayon closure boundary.
        let body_slice: &[u8] =
            unsafe { std::slice::from_raw_parts(self.mmap.as_ptr().add(self.body_offset), body) };
        body_slice.par_chunks(STRIPE).for_each(|chunk| {
            let mut acc = 0u8;
            let mut i = 0;
            while i < chunk.len() {
                // Volatile read so the optimizer doesn't elide the
                // page-faulting access when the chunk's `acc` result
                // looks dead.
                let byte = unsafe { std::ptr::read_volatile(chunk.as_ptr().add(i)) };
                acc = acc.wrapping_add(byte);
                i += PAGE;
            }
            std::hint::black_box(acc);
        });
    }

    fn view(&self) -> ArrayView2<'_, f32> {
        let len = self.rows * self.cols;
        // Safety: `Self::new` validated the byte range, the alignment, and
        // that `len * 4` fits inside `mmap[body_offset..]`. f32 is `Copy`
        // and `repr(C)` so casting LE bytes to f32 mirrors what the
        // builder writes via `to_le_bytes` ~ all targets the binary ships
        // to are little-endian.
        let floats = unsafe {
            std::slice::from_raw_parts(self.mmap.as_ptr().add(self.body_offset) as *const f32, len)
        };
        ArrayView2::from_shape((self.rows, self.cols), floats)
            .expect("validated shape in MappedDense::new")
    }
}

#[derive(Debug)]
enum Backing {
    Owned(Array2<f32>),
    Mapped(MappedDense),
}

#[derive(Debug)]
pub struct DenseIndex {
    backing: Backing,
}

impl DenseIndex {
    pub fn new(mut vectors: Array2<f32>) -> Self {
        for mut row in vectors.axis_iter_mut(Axis(0)) {
            let norm = row.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm > 1e-8 {
                row.mapv_inplace(|v| v / norm);
            }
        }
        Self {
            backing: Backing::Owned(vectors),
        }
    }

    /// Construct without re-normalizing. Use when the caller has already
    /// produced unit-norm rows (the engine builder does this before
    /// persisting; the lossless f32 on-disk blob preserves it exactly).
    /// Saves an O(rows * dim) sweep per warm hybrid/semantic search.
    pub fn from_normalized(vectors: Array2<f32>) -> Self {
        Self {
            backing: Backing::Owned(vectors),
        }
    }

    /// Construct a zero-copy view over `mapped`. The matrix is left
    /// unnormalized (the persistence layer writes normalized vectors so
    /// the disk image is already unit-norm). Subsequent `dot` calls run
    /// directly against the OS page cache.
    pub fn from_mapped(mapped: MappedDense) -> Self {
        Self {
            backing: Backing::Mapped(mapped),
        }
    }

    fn view(&self) -> ArrayView2<'_, f32> {
        match &self.backing {
            Backing::Owned(arr) => arr.view(),
            Backing::Mapped(m) => m.view(),
        }
    }

    pub fn len(&self) -> usize {
        match &self.backing {
            Backing::Owned(arr) => arr.shape()[0],
            Backing::Mapped(m) => m.rows,
        }
    }

    pub fn dim(&self) -> usize {
        match &self.backing {
            Backing::Owned(arr) => arr.shape()[1],
            Backing::Mapped(m) => m.cols,
        }
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
        let view = self.view();
        let mut scores: Vec<(usize, f32)> = match selector {
            Some(candidates) => candidates
                .par_iter()
                .map(|&idx| (idx, view.row(idx).dot(vector)))
                .collect(),
            None => {
                // ndarray's matrixmultiply-backed gemv: a single contiguous
                // SIMD pass over the whole matrix is faster than rayon
                // parallelism over per-row hand-rolled zip+fold.
                let dots = view.dot(vector);
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

    #[test]
    fn mapped_backing_matches_owned() {
        use super::{Backing, MappedDense};
        use memmap2::MmapMut;
        use std::sync::Arc;

        // Build a 4x3 matrix in a buffer with a 16-byte header, then mmap it
        // (anon mmap copy keeps the data inside the test process).
        let rows = 4usize;
        let cols = 3usize;
        let body = rows * cols;
        let mut buf = vec![0u8; 16 + body * 4];
        let values: [f32; 12] = [0.5, 0.5, 0.0, 0.7, 0.2, 0.1, 0.0, 0.9, 0.1, 0.3, 0.3, 0.4];
        for (i, v) in values.iter().enumerate() {
            let off = 16 + i * 4;
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        let mut mmap = MmapMut::map_anon(buf.len()).unwrap();
        mmap.copy_from_slice(&buf);
        let mmap = mmap.make_read_only().unwrap();
        let mapped = MappedDense::new(Arc::new(mmap), 16, rows, cols).unwrap();
        let index = DenseIndex::from_mapped(mapped);
        // Same content, owned backing.
        let owned = DenseIndex {
            backing: Backing::Owned(
                ndarray::Array2::from_shape_vec((rows, cols), values.to_vec()).unwrap(),
            ),
        };
        let query = ndarray::array![0.6, 0.3, 0.1];
        let from_mapped = index.query(&query, 4, None);
        let from_owned = owned.query(&query, 4, None);
        assert_eq!(from_mapped.len(), from_owned.len());
        for (a, b) in from_mapped.iter().zip(from_owned.iter()) {
            assert_eq!(a.0, b.0);
            assert!((a.1 - b.1).abs() < 1e-6);
        }
    }
}
