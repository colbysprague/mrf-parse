//! Memory test: parse a synthetic multi-GB in-network file and assert that
//! peak RSS stays under a fixed bound regardless of input size.
//!
//! The big test is `#[ignore]`d; run it with `make memtest` (which runs it
//! alone, so no other test inflates the process's peak RSS).
//!
//! * `MRF_MEMTEST_GB` — input size in GB (default 2)
//! * `MRF_MEMTEST_DIR` — scratch dir for input + output (default: temp dir)

#[path = "../../../tests/gen/gen_tic.rs"]
mod gen;

use std::time::Instant;

use mrf_core::mem::{human_bytes, peak_rss_bytes};
use mrf_core::{Source, ValueFilter};
use mrf_tic::{parse_in_network_file, TicOptions};

const RSS_LIMIT: u64 = 512 * 1024 * 1024;

/// Small version, always run: checks the generator and the reader agree on
/// row counts, in both the one-pass and two-pass (filtered) modes.
#[test]
fn synthetic_counts_match() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("syn.json");
    let o = gen::GenOptions {
        target_bytes: 4 * 1024 * 1024,
        provider_groups: 200,
        rates_per_item: 500,
        ..Default::default()
    };
    let st = gen::write_synthetic(&input, &o).unwrap();
    assert!(st.items > 7, "covers a rates-first element");

    let s = parse_in_network_file(
        &Source::Path(input.clone()),
        &dir.path().join("out"),
        &TicOptions::default(),
    )
    .unwrap();
    assert_eq!(s.passes, 1);
    assert_eq!(s.counts.in_network_items, st.items);
    assert_eq!(s.counts.rates, st.rate_rows);
    assert_eq!(s.counts.providers, st.provider_rows);
    assert_eq!(s.counts.rejects, 0);

    // NPI filter keeps exactly group 1 (its first NPI).
    let opts = TicOptions {
        npis: ValueFilter::from_values(["1000000100"]),
        ..Default::default()
    };
    let f = parse_in_network_file(&Source::Path(input), &dir.path().join("out_f"), &opts).unwrap();
    assert_eq!(f.passes, 2, "references come last → two passes");
    assert_eq!(f.counts.providers, 1);
    assert!(f.counts.rates > 0 && f.counts.rates < st.rate_rows);
}

#[test]
#[ignore = "multi-GB; run with `make memtest`"]
fn peak_rss_bounded_on_multi_gb_input() {
    let gb: f64 = std::env::var("MRF_MEMTEST_GB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2.0);
    let tmp;
    let dir = match std::env::var_os("MRF_MEMTEST_DIR") {
        Some(d) => std::path::PathBuf::from(d),
        None => {
            tmp = tempfile::tempdir().unwrap();
            tmp.path().to_path_buf()
        }
    };
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("memtest-in-network.json");
    let out = dir.join("memtest-out");

    let t = Instant::now();
    let st = gen::write_synthetic(
        &input,
        &gen::GenOptions {
            target_bytes: (gb * 1024.0 * 1024.0 * 1024.0) as u64,
            // Long per-price notes, as in real UHC files.
            info_bytes: 1500,
            ..Default::default()
        },
    )
    .unwrap();
    eprintln!(
        "generated {} ({} items, {} rates) in {:.1}s",
        human_bytes(st.bytes),
        st.items,
        st.rates,
        t.elapsed().as_secs_f64()
    );

    let s =
        parse_in_network_file(&Source::Path(input.clone()), &out, &TicOptions::default()).unwrap();
    let secs = s.elapsed.as_secs_f64();
    let peak = peak_rss_bytes().expect("peak RSS available on this platform");
    eprintln!(
        "parsed {} in {:.1}s: {:.1} MB/s, {:.0} tic_rate rows/s, peak RSS {}",
        human_bytes(st.bytes),
        secs,
        st.bytes as f64 / secs / 1e6,
        s.counts.rates as f64 / secs,
        human_bytes(peak)
    );
    assert_eq!(s.counts.rates, st.rate_rows);
    assert_eq!(s.counts.rejects, 0);
    assert!(
        peak < RSS_LIMIT,
        "peak RSS {} exceeds {}",
        human_bytes(peak),
        human_bytes(RSS_LIMIT)
    );
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_dir_all(&out);
}
