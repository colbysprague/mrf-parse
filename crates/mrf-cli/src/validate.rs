//! `mrf validate <dir>`: row counts, per-column null rates, and reject
//! counts per file for an output directory.
//!
//! Batches are read one at a time, so validating a large output does not
//! load it into memory.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use anyhow::{bail, Result};
use arrow_array::{Array, StringArray};
use mrf_core::read::{read_parquet, table_parts};
use mrf_core::Table;

/// Statistics for one table.
#[derive(Debug, Default)]
pub struct TableStats {
    pub partitions: usize,
    pub rows: u64,
    /// (column, nulls), in schema order.
    pub nulls: Vec<(String, u64)>,
}

pub fn table_stats(dir: &Path, table: Table) -> Result<Option<TableStats>> {
    let parts = table_parts(dir, table)?;
    if parts.is_empty() {
        return Ok(None);
    }
    let schema = table.schema();
    let mut s = TableStats {
        partitions: parts.len(),
        nulls: schema
            .fields()
            .iter()
            .map(|f| (f.name().clone(), 0))
            .collect(),
        ..Default::default()
    };
    for p in &parts {
        for batch in read_parquet(p)? {
            s.rows += batch.num_rows() as u64;
            for (i, col) in batch.columns().iter().enumerate() {
                if let Some(slot) = s.nulls.get_mut(i) {
                    slot.1 += col.null_count() as u64;
                }
            }
        }
    }
    Ok(Some(s))
}

/// Rejects per `file_id`, sorted by file id.
pub fn rejects_per_file(dir: &Path) -> Result<BTreeMap<String, u64>> {
    let mut out = BTreeMap::new();
    for p in table_parts(dir, Table::Rejects)? {
        for batch in read_parquet(&p)? {
            let ids = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("rejects.file_id is Utf8");
            for i in 0..ids.len() {
                *out.entry(ids.value(i).to_string()).or_insert(0) += 1;
            }
        }
        // Files with zero rejects still have a (empty) partition; list them
        // with 0 so every parsed file appears.
        if let Some(id) = p
            .parent()
            .and_then(|d| d.file_name())
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("file_id="))
        {
            out.entry(id.to_string()).or_insert(0);
        }
    }
    Ok(out)
}

pub fn run(dir: &Path) -> Result<()> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let mut out = io::stdout().lock();
    let mut any = false;
    for table in Table::ALL {
        let Some(s) = table_stats(dir, table)? else {
            continue;
        };
        any = true;
        writeln!(
            out,
            "{}: {} rows in {} file partition(s)",
            table.name(),
            s.rows,
            s.partitions
        )?;
        for (col, nulls) in &s.nulls {
            let pct = if s.rows == 0 {
                0.0
            } else {
                100.0 * *nulls as f64 / s.rows as f64
            };
            writeln!(out, "    {col:<30} null {pct:6.2}%  ({nulls})")?;
        }
    }
    if !any {
        bail!("no output tables found under {}", dir.display());
    }
    let rejects = rejects_per_file(dir)?;
    if !rejects.is_empty() {
        writeln!(out, "rejects per file:")?;
        for (id, n) in rejects {
            writeln!(out, "    file_id={id}  {n}")?;
        }
    }
    Ok(())
}
