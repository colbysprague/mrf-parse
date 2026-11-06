//! Throughput of the TiC readers on a mid-size synthetic in-network file
//! (see `tests/gen/gen_tic.rs`) and a scaled table of contents.
//!
//! Criterion reports MB/s; rows/s is printed once per reader.

#[path = "../../../tests/gen/gen_tic.rs"]
mod gen;

use std::io::Write;
use std::time::Instant;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use mrf_core::{Source, ValueFilter};
use mrf_tic::{parse_in_network_file, toc::read_toc, TicOptions};

fn bench(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let mut group = c.benchmark_group("tic");
    group.sample_size(10);

    // In-network, ~64MB, references last.
    let input = dir.path().join("in_network.json");
    let st = gen::write_synthetic(&input, &gen::GenOptions::default()).unwrap();
    let src = Source::Path(input);
    let out = dir.path().join("out");
    for (name, opts) in [
        ("in_network", TicOptions::default()),
        (
            // Two-pass path: NPI filter with references after in_network.
            "in_network_npi_filter",
            TicOptions {
                npis: ValueFilter::from_values(["1000000100", "1000050000"]),
                ..Default::default()
            },
        ),
    ] {
        let t = Instant::now();
        let s = parse_in_network_file(&src, &out, &opts).unwrap();
        let secs = t.elapsed().as_secs_f64();
        eprintln!(
            "{name}: {:.1}MB, {} tic_rate rows, {:.1} MB/s, {:.0} rate rows/s ({} negotiated_rates/s)",
            st.bytes as f64 / 1e6,
            s.counts.rates,
            st.bytes as f64 / secs / 1e6,
            s.counts.rates as f64 / secs,
            (st.rates as f64 / secs) as u64,
        );
        group.throughput(Throughput::Bytes(st.bytes));
        group.bench_function(name, |b| {
            b.iter(|| std::hint::black_box(parse_in_network_file(&src, &out, &opts).unwrap()))
        });
    }

    // Table of contents with 50k reporting structures.
    let toc = dir.path().join("toc.json");
    {
        let mut w = std::io::BufWriter::new(std::fs::File::create(&toc).unwrap());
        write!(w, r#"{{"reporting_entity_name":"S","reporting_entity_type":"t","last_updated_on":"2026-01-01","version":"1.0.0","reporting_structure":["#).unwrap();
        for i in 0..50_000 {
            if i > 0 {
                w.write_all(b",").unwrap();
            }
            write!(w, r#"{{"reporting_plans":[{{"plan_name":"Employer plan {i}","plan_id_type":"ein","plan_id":"{i:09}","plan_sponsor_name":"Employer {i}","plan_market_type":"group"}}],"in_network_files":[{{"description":"in-network","location":"https://example.com/in-network/{}.json.gz"}}]}}"#, i % 500).unwrap();
        }
        w.write_all(b"]}").unwrap();
    }
    let bytes = std::fs::metadata(&toc).unwrap().len();
    let read = || {
        let mut n = 0u64;
        read_toc(std::fs::File::open(&toc).unwrap(), |_| {
            n += 1;
            Ok(())
        })
        .unwrap();
        n
    };
    let t = Instant::now();
    let n = read();
    let secs = t.elapsed().as_secs_f64();
    eprintln!(
        "toc: {:.1}MB, {n} entries, {:.1} MB/s, {:.0} entries/s",
        bytes as f64 / 1e6,
        bytes as f64 / secs / 1e6,
        n as f64 / secs
    );
    group.throughput(Throughput::Bytes(bytes));
    group.bench_function("toc", |b| b.iter(|| std::hint::black_box(read())));
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
