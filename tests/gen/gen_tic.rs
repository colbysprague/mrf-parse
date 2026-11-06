//! Synthetic TiC in-network file generator (for the memory test and
//! benchmarks). Shared by `#[path]` includes; std + no extra deps.
//!
//! The output is shaped to stress the streaming paths:
//! * `in_network[]` elements are large (thousands of negotiated rates each),
//!   so an element can never be held whole;
//! * every 7th element puts `negotiated_rates` *before* its header fields,
//!   exercising the spill buffer;
//! * `provider_references` comes after `in_network` when
//!   `refs_last` is set, exercising the out-of-order path.

#![allow(dead_code)]

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

pub struct GenOptions {
    /// Stop adding in_network elements once roughly this many bytes are out.
    pub target_bytes: u64,
    pub provider_groups: u32,
    pub npis_per_group: u32,
    pub rates_per_item: u32,
    pub refs_per_rate: u32,
    pub refs_last: bool,
    /// Bytes of `additional_information` text per negotiated price. Real
    /// UHC files carry 1-30KB notes here; long strings are what exposed the
    /// writer's allocation behavior, so the memory test sets this.
    pub info_bytes: usize,
}

impl Default for GenOptions {
    fn default() -> Self {
        GenOptions {
            target_bytes: 64 * 1024 * 1024,
            provider_groups: 2_000,
            npis_per_group: 5,
            rates_per_item: 2_000,
            refs_per_rate: 3,
            refs_last: true,
            info_bytes: 0,
        }
    }
}

/// What was written, for computing expected rows and throughput.
#[derive(Debug, Clone, Copy, Default)]
pub struct GenStats {
    pub bytes: u64,
    pub items: u64,
    pub rates: u64,
    /// Expected `tic_rate` rows (prices × references).
    pub rate_rows: u64,
    pub provider_rows: u64,
}

struct Counting<W> {
    w: W,
    n: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let k = self.w.write(b)?;
        self.n += k as u64;
        Ok(k)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

/// Small deterministic PRNG (xorshift) so files are reproducible.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

pub fn write_synthetic(path: &Path, o: &GenOptions) -> io::Result<GenStats> {
    let f = File::create(path)?;
    let mut w = Counting {
        w: BufWriter::with_capacity(1 << 20, f),
        n: 0,
    };
    let mut st = GenStats::default();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    write!(
        w,
        r#"{{"reporting_entity_name":"Synthetic Payer","reporting_entity_type":"health insurance issuer","plan_name":"Synthetic PPO","plan_id_type":"hios","plan_id":"1234567890","plan_market_type":"group","last_updated_on":"2026-01-01","version":"1.0.0","#
    )?;
    if !o.refs_last {
        write_refs(&mut w, o, &mut st)?;
        w.write_all(b",")?;
    }
    w.write_all(br#""in_network":["#)?;
    // Distinct per price (prefix) so it does not dictionary-encode away.
    let filler = "Rates subject to stop-loss and lesser-of provisions; see contract. "
        .repeat(o.info_bytes / 66 + 1);
    let mut item = 0u64;
    while w.n < o.target_bytes {
        if item > 0 {
            w.write_all(b",")?;
        }
        let code = 10_000 + item % 90_000;
        let header = format!(
            r#""negotiation_arrangement":"ffs","name":"Synthetic service {item}","billing_code_type":"CPT","billing_code_type_version":"2026","billing_code":"{code}","description":"Synthetic description for code {code}""#
        );
        let rates_first = item % 7 == 3;
        w.write_all(b"{")?;
        if !rates_first {
            w.write_all(header.as_bytes())?;
            w.write_all(b",")?;
        }
        w.write_all(br#""negotiated_rates":["#)?;
        for r in 0..o.rates_per_item {
            if r > 0 {
                w.write_all(b",")?;
            }
            w.write_all(br#"{"provider_references":["#)?;
            for k in 0..o.refs_per_rate {
                if k > 0 {
                    w.write_all(b",")?;
                }
                write!(w, "{}", 1 + rng.next() % u64::from(o.provider_groups))?;
            }
            let cents = 1_000 + rng.next() % 500_000;
            let info = if o.info_bytes > 0 {
                format!(
                    r#","additional_information":"{} {}""#,
                    st.rates,
                    &filler[..o.info_bytes]
                )
            } else {
                String::new()
            };
            write!(
                w,
                r#"],"negotiated_prices":[{{"negotiated_type":"negotiated","negotiated_rate":{}.{:02},"expiration_date":"9999-12-31","service_code":["11","22"],"billing_class":"professional","setting":"outpatient"{info}}},{{"negotiated_type":"percentage","negotiated_rate":{},"expiration_date":"9999-12-31","billing_class":"institutional","setting":"both"{info}}}]}}"#,
                cents / 100,
                cents % 100,
                50 + rng.next() % 100
            )?;
            st.rates += 1;
            st.rate_rows += 2 * u64::from(o.refs_per_rate);
        }
        w.write_all(b"]")?;
        if rates_first {
            w.write_all(b",")?;
            w.write_all(header.as_bytes())?;
        }
        w.write_all(b"}")?;
        item += 1;
    }
    w.write_all(b"]")?;
    if o.refs_last {
        w.write_all(b",")?;
        write_refs(&mut w, o, &mut st)?;
    }
    w.write_all(b"}")?;
    w.flush()?;
    st.items = item;
    st.bytes = w.n;
    Ok(st)
}

fn write_refs<W: Write>(w: &mut W, o: &GenOptions, st: &mut GenStats) -> io::Result<()> {
    w.write_all(br#""provider_references":["#)?;
    for g in 1..=o.provider_groups {
        if g > 1 {
            w.write_all(b",")?;
        }
        write!(
            w,
            r#"{{"provider_group_id":{g},"network_name":["Synthetic Network"],"provider_groups":[{{"npi":["#
        )?;
        for n in 0..o.npis_per_group {
            if n > 0 {
                w.write_all(b",")?;
            }
            write!(
                w,
                "{}",
                1_000_000_000u64 + u64::from(g) * 100 + u64::from(n)
            )?;
        }
        write!(
            w,
            r#"],"tin":{{"type":"ein","value":"{:02}-{:07}"}}}}]}}"#,
            g % 100,
            g
        )?;
        st.provider_rows += u64::from(o.npis_per_group);
    }
    w.write_all(b"]")
}
