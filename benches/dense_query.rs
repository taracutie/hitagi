use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use ndarray::{Array1, Array2};

use mimi::search::dense::DenseIndex;

fn deterministic_matrix(rows: usize, dim: usize) -> Array2<f32> {
    // Deterministic but cheap pseudo-random fill so two runs produce
    // identical inputs. Numbers stay small enough that L2-norm in
    // DenseIndex::new doesn't underflow.
    let mut data = Vec::with_capacity(rows * dim);
    let mut x: u64 = 0x9E3779B97F4A7C15;
    for _ in 0..(rows * dim) {
        x ^= x.rotate_left(13);
        x = x.wrapping_mul(0x9E3779B97F4A7C15);
        x ^= x.rotate_right(7);
        let bits = (x >> 32) as u32;
        let f = (bits as f32 / u32::MAX as f32) - 0.5;
        data.push(f);
    }
    Array2::from_shape_vec((rows, dim), data).expect("shape")
}

fn deterministic_vector(dim: usize, seed: u64) -> Array1<f32> {
    let mut x: u64 = seed.wrapping_add(0x9E3779B97F4A7C15);
    let mut data = Vec::with_capacity(dim);
    for _ in 0..dim {
        x ^= x.rotate_left(13);
        x = x.wrapping_mul(0x9E3779B97F4A7C15);
        x ^= x.rotate_right(7);
        let bits = (x >> 32) as u32;
        data.push((bits as f32 / u32::MAX as f32) - 0.5);
    }
    let mut v = Array1::from(data);
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        v.mapv_inplace(|x| x / norm);
    }
    v
}

fn bench_dense_query(c: &mut Criterion) {
    let rows = 5000;
    let dim = 256;
    let index = DenseIndex::new(deterministic_matrix(rows, dim));
    let vector = deterministic_vector(dim, 42);

    let mut group = c.benchmark_group("dense_query");
    group.throughput(Throughput::Elements(rows as u64));

    group.bench_function("no_selector_top10", |b| {
        b.iter(|| {
            let scores = index.query(black_box(&vector), 10, None);
            black_box(scores);
        });
    });

    group.bench_function("no_selector_top90", |b| {
        // top_k * 9 = the candidate window fuse.rs actually uses
        b.iter(|| {
            let scores = index.query(black_box(&vector), 90, None);
            black_box(scores);
        });
    });

    let selector: Vec<usize> = (0..rows).step_by(5).collect();
    group.bench_function("selector_1k_top10", |b| {
        b.iter(|| {
            let scores = index.query(black_box(&vector), 10, Some(&selector));
            black_box(scores);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_dense_query);
criterion_main!(benches);
