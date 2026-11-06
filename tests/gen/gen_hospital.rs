//! Mid-size hospital MRF generator for benchmarks: scales a CMS example up
//! by repeating its data rows (CSV) or service lines (JSON) with distinct
//! descriptions, so item de-duplication still sees many distinct items.

#![allow(dead_code)]

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::Path;

/// Repeat the data rows (row 4+) of a CMS CSV example `copies` times.
/// Returns the number of data rows written.
pub fn scale_csv(example: &Path, out: &Path, copies: usize) -> io::Result<u64> {
    let text = fs::read_to_string(example)?;
    // Split into records on newlines outside quotes, so quoted cells with
    // embedded commas/newlines stay intact.
    let records = split_records(&text);
    let (head, data) = records.split_at(3);
    let mut w = BufWriter::new(fs::File::create(out)?);
    for r in head {
        writeln!(w, "{r}")?;
    }
    let mut n = 0;
    for c in 0..copies {
        for r in data {
            if r.trim().is_empty() {
                continue;
            }
            writeln!(w, "{}", prefix_description(r, c))?;
            n += 1;
        }
    }
    w.flush()?;
    Ok(n)
}

fn split_records(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in text.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                cur.push(ch);
            }
            '\n' if !quoted => {
                out.push(cur.trim_end_matches('\r').to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Prefix the first cell (the description) with `#<copy> `.
fn prefix_description(record: &str, copy: usize) -> String {
    match record.strip_prefix('"') {
        Some(rest) => format!("\"#{copy} {rest}"),
        None => format!("#{copy} {record}"),
    }
}

/// Repeat `standard_charge_information[]` of a JSON example `copies` times.
/// Returns the number of service lines written.
pub fn scale_json(example: &Path, out: &Path, copies: usize) -> io::Result<u64> {
    let mut doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(example)?)?;
    let items = doc["standard_charge_information"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut scaled = Vec::with_capacity(items.len() * copies);
    for c in 0..copies {
        for it in &items {
            let mut it = it.clone();
            if let Some(d) = it.get("description").and_then(|d| d.as_str()) {
                it["description"] = serde_json::Value::String(format!("#{c} {d}"));
            }
            scaled.push(it);
        }
    }
    let n = scaled.len() as u64;
    doc["standard_charge_information"] = serde_json::Value::Array(scaled);
    let w = BufWriter::new(fs::File::create(out)?);
    serde_json::to_writer(w, &doc)?;
    Ok(n)
}
