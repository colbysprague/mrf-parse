//! Throughput of each hospital reader on mid-size inputs built by scaling
//! the CMS v3 examples (see `tests/gen/gen_hospital.rs`).
//!
//! Criterion reports MB/s (`Throughput::Bytes`); rows/s is printed once per
//! reader from a timed warm-up run.

#[path = "../../../tests/gen/gen_hospital.rs"]
mod gen;

use std::path::{Path, PathBuf};
use std::time::Instant;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use mrf_core::Source;
use mrf_hospital::{parse_file, HospitalOptions};

fn example(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/hospital")
        .join(rel)
}

fn bench(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    // ~10-25MB per input.
    let inputs = [
        ("csv_tall", "v3/tall.csv", 2000usize),
        ("csv_wide", "v3/wide.csv", 2000),
        ("json", "v3/example.json", 800),
    ];
    let mut group = c.benchmark_group("hospital");
    group.sample_size(10);
    for (name, rel, copies) in inputs {
        let input = dir.path().join(format!("{name}.in"));
        if rel.ends_with(".json") {
            gen::scale_json(&example(rel), &input, copies).unwrap();
        } else {
            gen::scale_csv(&example(rel), &input, copies).unwrap();
        }
        let bytes = std::fs::metadata(&input).unwrap().len();
        let out = dir.path().join(format!("{name}.out"));
        let src = Source::Path(input);
        let opts = HospitalOptions::default();

        let t = Instant::now();
        let s = parse_file(&src, &out, &opts).unwrap();
        let secs = t.elapsed().as_secs_f64();
        eprintln!(
            "{name}: {:.1}MB, {} charge rows, {:.1} MB/s, {:.0} charge rows/s",
            bytes as f64 / 1e6,
            s.counts.charges,
            bytes as f64 / secs / 1e6,
            s.counts.charges as f64 / secs
        );

        group.throughput(Throughput::Bytes(bytes));
        group.bench_function(name, |b| {
            b.iter(|| std::hint::black_box(parse_file(&src, &out, &opts).unwrap()))
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
