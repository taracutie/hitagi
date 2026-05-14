//! Times `model2vec::model_fingerprint` against a synthetic on-disk model
//! directory (three files of realistic sizes for tokenizer, weights,
//! config). Headline bench for Win 6: skipping rehashing when file
//! metadata is unchanged should drop 30-150 ms in the warm encoder load
//! path.

use std::io::Write;
use std::path::PathBuf;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use mimi::search::model2vec::{
    model_files_meta, model_fingerprint, ModelLoadPolicy, ModelOptions,
};

fn build_model_dir() -> (PathBuf, ModelOptions) {
    let dir = std::env::temp_dir().join(format!(
        "mimi-bench-model-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");

    // Realistic sizes: tokenizer ~2 MB, model weights ~30 MB, config ~1 KB.
    let mut writer = std::fs::File::create(dir.join("tokenizer.json")).expect("tokenizer");
    writer
        .write_all(&vec![0xABu8; 2 * 1024 * 1024])
        .expect("tokenizer fill");
    let mut writer = std::fs::File::create(dir.join("model.safetensors")).expect("weights");
    writer
        .write_all(&vec![0xCDu8; 30 * 1024 * 1024])
        .expect("weights fill");
    let mut writer = std::fs::File::create(dir.join("config.json")).expect("config");
    writer
        .write_all(b"{\"hidden_size\":256}")
        .expect("config fill");

    let options = ModelOptions {
        model: dir.to_string_lossy().to_string(),
        policy: ModelLoadPolicy::Offline,
    };
    (dir, options)
}

fn bench_fingerprint(c: &mut Criterion) {
    let (_dir, options) = build_model_dir();

    let mut group = c.benchmark_group("model_fingerprint");
    group.sample_size(20);

    group.bench_function("hash_30mb_model", |b| {
        b.iter(|| {
            let fp = model_fingerprint(black_box(&options)).expect("fingerprint");
            black_box(fp);
        });
    });

    // Win 6's warm path: compute the cheap stat tuple and compare against a
    // remembered string. This is what `load_encoder_with_policy` runs when
    // the cached dense row's metadata still matches.
    let cached_meta = model_files_meta(&options).expect("baseline meta");
    group.bench_function("meta_tuple_match", |b| {
        b.iter(|| {
            let current = model_files_meta(black_box(&options)).expect("meta");
            let matches = current == cached_meta;
            black_box((current, matches));
        });
    });

    group.finish();
}

criterion_group!(benches, bench_fingerprint);
criterion_main!(benches);
