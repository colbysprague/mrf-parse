//! Write a synthetic TiC in-network file.
//!
//! ```text
//! cargo run --release -p mrf-tic --example gen_tic -- <out.json> [size_gb] [--refs-first]
//! ```

#[path = "../../../tests/gen/gen_tic.rs"]
mod gen;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(out) = args.first() else {
        eprintln!("usage: gen_tic <out.json> [size_gb] [--refs-first]");
        std::process::exit(2);
    };
    let gb: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let opts = gen::GenOptions {
        target_bytes: (gb * 1024.0 * 1024.0 * 1024.0) as u64,
        refs_last: !args.iter().any(|a| a == "--refs-first"),
        ..Default::default()
    };
    let st = gen::write_synthetic(std::path::Path::new(out), &opts)?;
    println!(
        "wrote {out}: {} bytes, {} in_network items, {} negotiated_rates, {} expected tic_rate rows",
        st.bytes, st.items, st.rates, st.rate_rows
    );
    Ok(())
}
