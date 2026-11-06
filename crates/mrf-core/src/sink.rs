//! Batching Parquet writer and the `rejects` sink.
//!
//! Readers push rows one at a time as `&[Cell]`. A `TableWriter` accumulates
//! them in Arrow column builders and hands a `RecordBatch` to the Parquet
//! writer every `batch_rows` rows, so memory per table is bounded by one
//! batch plus one in-progress row group, independent of input size.
//!
//! Layout: `<root>/<table>/file_id=<id>/part-0.parquet` (Hive-style
//! partitioning, readable by DuckDB/Polars/Spark as a dataset).
//!
//! Each partition is written to `part-0.parquet.tmp` and renamed on
//! `finish()`, so a crashed or failed run never leaves a half-written file
//! that looks valid. An existing partition for the same `file_id` is replaced.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::builder::{
    BooleanBuilder, Float64Builder, Int64Builder, ListBuilder, StringBuilder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{ArrowError, DataType, SchemaRef};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::errors::ParquetError;
use parquet::file::properties::WriterProperties;

use crate::schema::Table;

/// Rows buffered in Arrow builders before being encoded.
pub const DEFAULT_BATCH_ROWS: usize = 8 * 1024;
/// A batch is also flushed once its text reaches this many bytes. Real TiC
/// files carry multi-KB `additional_information` strings; without a byte
/// cap an 8192-row batch grows ~15MB string buffers by doubling, and the
/// macOS allocator keeps each freed large block resident (seen as ~900MB
/// peak RSS on a 1.5GB UHC file). Capping the bytes keeps buffers small and
/// reusable, so memory stays flat whatever the value sizes.
pub const BATCH_MAX_BYTES: usize = 2 * 1024 * 1024;
/// Rows per Parquet row group. The writer keeps one encoded row group in
/// memory, so this caps the writer's memory too.
pub const ROW_GROUP_ROWS: usize = 64 * 1024;
/// Maximum bytes kept for `rejects.raw_snippet`.
pub const SNIPPET_MAX_BYTES: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parquet error: {0}")]
    Parquet(#[from] ParquetError),
    #[error("arrow error: {0}")]
    Arrow(#[from] ArrowError),
    /// A reader built a row that does not match the table schema. This is a
    /// programming error, not bad input data.
    #[error("row for {table} does not match schema: {message}")]
    Schema {
        table: &'static str,
        message: String,
    },
}

/// One value in a row. Borrowed (`&'a str`) so readers can push values
/// straight out of their parse buffers without allocating.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cell<'a> {
    Str(Option<&'a str>),
    F64(Option<f64>),
    I64(Option<i64>),
    Bool(Option<bool>),
    StrList(Option<&'a [String]>),
}

impl<'a> Cell<'a> {
    /// `Cell::Str` from an `Option<String>` field, the common case.
    pub fn s(v: &'a Option<String>) -> Cell<'a> {
        Cell::Str(v.as_deref())
    }
}

impl<'a> From<&'a str> for Cell<'a> {
    fn from(v: &'a str) -> Self {
        Cell::Str(Some(v))
    }
}

impl<'a> From<Option<&'a str>> for Cell<'a> {
    fn from(v: Option<&'a str>) -> Self {
        Cell::Str(v)
    }
}

impl From<Option<f64>> for Cell<'_> {
    fn from(v: Option<f64>) -> Self {
        Cell::F64(v)
    }
}

/// One Arrow builder per column. An enum (rather than
/// `Box<dyn ArrayBuilder>` + downcasting) keeps the type dispatch explicit
/// and checked by the compiler.
enum ColBuilder {
    Str(StringBuilder),
    F64(Float64Builder),
    I64(Int64Builder),
    Bool(BooleanBuilder),
    StrList(ListBuilder<StringBuilder>),
}

impl ColBuilder {
    fn for_type(dt: &DataType) -> Option<ColBuilder> {
        Some(match dt {
            DataType::Utf8 => ColBuilder::Str(StringBuilder::new()),
            DataType::Float64 => ColBuilder::F64(Float64Builder::new()),
            DataType::Int64 => ColBuilder::I64(Int64Builder::new()),
            DataType::Boolean => ColBuilder::Bool(BooleanBuilder::new()),
            DataType::List(item) if item.data_type() == &DataType::Utf8 => {
                ColBuilder::StrList(ListBuilder::new(StringBuilder::new()))
            }
            _ => return None,
        })
    }

    /// Append one cell; returns false on a type mismatch.
    fn append(&mut self, cell: &Cell<'_>) -> bool {
        match (self, cell) {
            (ColBuilder::Str(b), Cell::Str(v)) => b.append_option(*v),
            (ColBuilder::F64(b), Cell::F64(v)) => b.append_option(*v),
            (ColBuilder::I64(b), Cell::I64(v)) => b.append_option(*v),
            (ColBuilder::Bool(b), Cell::Bool(v)) => b.append_option(*v),
            (ColBuilder::StrList(b), Cell::StrList(v)) => match v {
                Some(items) => {
                    for s in items.iter() {
                        b.values().append_value(s);
                    }
                    b.append(true);
                }
                None => b.append(false),
            },
            _ => return false,
        }
        true
    }

    /// Take the accumulated values as an array, leaving the builder empty.
    fn finish(&mut self) -> ArrayRef {
        match self {
            ColBuilder::Str(b) => Arc::new(b.finish()),
            ColBuilder::F64(b) => Arc::new(b.finish()),
            ColBuilder::I64(b) => Arc::new(b.finish()),
            ColBuilder::Bool(b) => Arc::new(b.finish()),
            ColBuilder::StrList(b) => Arc::new(b.finish()),
        }
    }
}

/// Writes one table partition: `<root>/<table>/file_id=<id>/part-0.parquet`.
pub struct TableWriter {
    table: Table,
    schema: SchemaRef,
    builders: Vec<ColBuilder>,
    pending: usize,
    /// Approximate bytes of text in the pending batch.
    pending_bytes: usize,
    batch_rows: usize,
    rows_written: u64,
    // `Option` so `finish(self)` can take the writer out while `Drop` still
    // sees whether finishing happened.
    writer: Option<ArrowWriter<BufWriter<File>>>,
    tmp_path: PathBuf,
    final_path: PathBuf,
}

impl TableWriter {
    pub fn create(root: &Path, table: Table, file_id: &str) -> Result<TableWriter, SinkError> {
        Self::with_batch_rows(root, table, file_id, DEFAULT_BATCH_ROWS)
    }

    pub fn with_batch_rows(
        root: &Path,
        table: Table,
        file_id: &str,
        batch_rows: usize,
    ) -> Result<TableWriter, SinkError> {
        let dir = partition_dir(root, table, file_id);
        let io = |path: &Path| {
            let path = path.to_path_buf();
            move |source| SinkError::Io { path, source }
        };
        // Replace any partition from a previous run of the same file, so a
        // rerun never leaves stale parts behind.
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(io(&dir))?;
        }
        fs::create_dir_all(&dir).map_err(io(&dir))?;
        let final_path = dir.join("part-0.parquet");
        let tmp_path = dir.join("part-0.parquet.tmp");
        let file = File::create(&tmp_path).map_err(io(&tmp_path))?;

        let schema = table.schema();
        let builders = schema
            .fields()
            .iter()
            .map(|f| {
                ColBuilder::for_type(f.data_type()).ok_or_else(|| SinkError::Schema {
                    table: table.name(),
                    message: format!("unsupported type for {}: {}", f.name(), f.data_type()),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Determinism: the defaults write no timestamps or random values;
        // `created_by` is the parquet crate version, fixed by Cargo.lock.
        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .set_max_row_group_row_count(Some(ROW_GROUP_ROWS))
            .build();
        let writer = ArrowWriter::try_new(BufWriter::new(file), schema.clone(), Some(props))?;

        Ok(TableWriter {
            table,
            schema,
            builders,
            pending: 0,
            pending_bytes: 0,
            batch_rows: batch_rows.max(1),
            rows_written: 0,
            writer: Some(writer),
            tmp_path,
            final_path,
        })
    }

    pub fn table(&self) -> Table {
        self.table
    }

    /// Rows appended so far (flushed or pending).
    pub fn rows(&self) -> u64 {
        self.rows_written + self.pending as u64
    }

    /// Append one row. `row` must have one cell per schema column, in order.
    pub fn append(&mut self, row: &[Cell<'_>]) -> Result<(), SinkError> {
        if row.len() != self.builders.len() {
            return Err(SinkError::Schema {
                table: self.table.name(),
                message: format!("expected {} cells, got {}", self.builders.len(), row.len()),
            });
        }
        // Check every cell's type before appending any, so a bad row cannot
        // leave the builders at different lengths.
        for (i, (b, c)) in self.builders.iter().zip(row).enumerate() {
            let ok = matches!(
                (b, c),
                (ColBuilder::Str(_), Cell::Str(_))
                    | (ColBuilder::F64(_), Cell::F64(_))
                    | (ColBuilder::I64(_), Cell::I64(_))
                    | (ColBuilder::Bool(_), Cell::Bool(_))
                    | (ColBuilder::StrList(_), Cell::StrList(_))
            );
            if !ok {
                return Err(SinkError::Schema {
                    table: self.table.name(),
                    message: format!("column {} got {:?}", self.schema.field(i).name(), c),
                });
            }
        }
        self.pending_bytes += row
            .iter()
            .map(|c| match c {
                Cell::Str(Some(s)) => s.len(),
                Cell::StrList(Some(items)) => items.iter().map(|s| s.len()).sum(),
                _ => 0,
            })
            .sum::<usize>();
        for (b, c) in self.builders.iter_mut().zip(row) {
            let appended = b.append(c);
            debug_assert!(appended, "type checked above");
        }
        self.pending += 1;
        if self.pending >= self.batch_rows || self.pending_bytes >= BATCH_MAX_BYTES {
            self.flush_batch()?;
        }
        Ok(())
    }

    fn flush_batch(&mut self) -> Result<(), SinkError> {
        if self.pending == 0 {
            return Ok(());
        }
        let columns: Vec<ArrayRef> = self.builders.iter_mut().map(|b| b.finish()).collect();
        // `try_new` also rejects nulls in non-nullable columns.
        let batch = RecordBatch::try_new(self.schema.clone(), columns)?;
        self.writer
            .as_mut()
            .expect("writer present until finish")
            .write(&batch)?;
        self.rows_written += self.pending as u64;
        self.pending = 0;
        self.pending_bytes = 0;
        Ok(())
    }

    /// Flush, close the Parquet file, and move it into place. Returns the
    /// number of rows written. A partition is written even with zero rows,
    /// so "parsed, nothing found" is distinguishable from "never parsed".
    pub fn finish(mut self) -> Result<u64, SinkError> {
        self.flush_batch()?;
        let writer = self.writer.take().expect("finish called once");
        writer.close()?;
        fs::rename(&self.tmp_path, &self.final_path).map_err(|source| SinkError::Io {
            path: self.final_path.clone(),
            source,
        })?;
        Ok(self.rows_written)
    }
}

impl Drop for TableWriter {
    fn drop(&mut self) {
        // Dropped without `finish()` (error path): discard the partial file.
        if self.writer.take().is_some() {
            let _ = fs::remove_file(&self.tmp_path);
        }
    }
}

/// `<root>/<table>/file_id=<id>`
pub fn partition_dir(root: &Path, table: Table, file_id: &str) -> PathBuf {
    root.join(table.name()).join(format!("file_id={file_id}"))
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 character.
pub fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Collects malformed rows/objects for one input file.
pub struct RejectSink {
    file_id: String,
    writer: TableWriter,
}

impl RejectSink {
    pub fn create(root: &Path, file_id: &str) -> Result<RejectSink, SinkError> {
        Ok(RejectSink {
            file_id: file_id.to_string(),
            writer: TableWriter::create(root, Table::Rejects, file_id)?,
        })
    }

    /// Record a reject. `location` is a CSV row number or JSON path;
    /// `raw` is truncated to 1KB.
    pub fn reject(
        &mut self,
        location: &str,
        reason: &str,
        raw: Option<&str>,
    ) -> Result<(), SinkError> {
        tracing::debug!(file_id = %self.file_id, location, reason, "reject");
        let snippet = raw.map(|r| truncate_utf8(r, SNIPPET_MAX_BYTES));
        self.writer.append(&[
            Cell::Str(Some(&self.file_id)),
            Cell::Str(Some(location)),
            Cell::Str(Some(reason)),
            Cell::Str(snippet),
        ])
    }

    pub fn count(&self) -> u64 {
        self.writer.rows()
    }

    pub fn finish(self) -> Result<u64, SinkError> {
        self.writer.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Array, Float64Array, ListArray, StringArray};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    fn read_batches(path: &Path) -> Vec<RecordBatch> {
        let f = File::open(path).unwrap();
        ParquetRecordBatchReaderBuilder::try_new(f)
            .unwrap()
            .build()
            .unwrap()
            .map(|b| b.unwrap())
            .collect()
    }

    fn write_sample(root: &Path, batch_rows: usize) -> PathBuf {
        let mut w = TableWriter::with_batch_rows(root, Table::HospitalItemCode, "abcd", batch_rows)
            .unwrap();
        for i in 0..25 {
            let id = format!("item{i}");
            let code = (i % 3 != 0).then(|| format!("{}", 99200 + i));
            w.append(&[Cell::Str(Some(&id)), Cell::s(&code), Cell::Str(Some("CPT"))])
                .unwrap();
        }
        assert_eq!(w.finish().unwrap(), 25);
        partition_dir(root, Table::HospitalItemCode, "abcd").join("part-0.parquet")
    }

    #[test]
    fn writes_partitioned_parquet_in_batches() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_sample(dir.path(), 4); // forces many flushes
        assert!(path.ends_with("hospital_item_code/file_id=abcd/part-0.parquet"));
        assert!(!path.with_extension("parquet.tmp").exists());
        let batches = read_batches(&path);
        let total: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total, 25);
        let codes = batches[0]
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert!(codes.is_null(0));
        assert_eq!(codes.value(1), "99201");
    }

    #[test]
    fn output_is_byte_identical_across_runs() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let pa = write_sample(a.path(), 4);
        let pb = write_sample(b.path(), 4);
        assert_eq!(fs::read(pa).unwrap(), fs::read(pb).unwrap());
    }

    #[test]
    fn rerun_replaces_partition() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_sample(dir.path(), 100);
        let stale = path.with_file_name("part-1.parquet");
        fs::write(&stale, b"stale").unwrap();
        write_sample(dir.path(), 100);
        assert!(!stale.exists());
    }

    #[test]
    fn rejects_bad_rows_without_corrupting_builders() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = TableWriter::create(dir.path(), Table::HospitalItemCode, "x").unwrap();
        // Wrong arity.
        assert!(w.append(&[Cell::Str(Some("a"))]).is_err());
        // Wrong type in last column: first two must not be appended.
        assert!(w
            .append(&[Cell::Str(Some("a")), Cell::Str(None), Cell::F64(Some(1.0))])
            .is_err());
        w.append(&[Cell::Str(Some("a")), Cell::Str(None), Cell::Str(None)])
            .unwrap();
        assert_eq!(w.finish().unwrap(), 1);
    }

    #[test]
    fn null_in_required_column_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = TableWriter::create(dir.path(), Table::HospitalItemCode, "x").unwrap();
        w.append(&[Cell::Str(None), Cell::Str(None), Cell::Str(None)])
            .unwrap();
        assert!(w.finish().is_err());
        // The temp file was cleaned up by Drop.
        let part = partition_dir(dir.path(), Table::HospitalItemCode, "x");
        assert!(!part.join("part-0.parquet.tmp").exists());
        assert!(!part.join("part-0.parquet").exists());
    }

    #[test]
    fn lists_floats_and_empty_tables() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = TableWriter::create(dir.path(), Table::TicProvider, "f").unwrap();
        let nets = vec!["Net A".to_string(), "Net B".to_string()];
        w.append(&[
            "f".into(),
            "g1".into(),
            Cell::StrList(Some(&nets)),
            "ein".into(),
            "123".into(),
            Cell::Str(None),
            Cell::Str(None),
        ])
        .unwrap();
        w.finish().unwrap();
        let p = partition_dir(dir.path(), Table::TicProvider, "f").join("part-0.parquet");
        let b = &read_batches(&p)[0];
        let l = b.column(2).as_any().downcast_ref::<ListArray>().unwrap();
        assert_eq!(l.value(0).len(), 2);

        let mut w = TableWriter::create(dir.path(), Table::TicRate, "f").unwrap();
        let mut row: Vec<Cell> = Table::TicRate
            .schema()
            .fields()
            .iter()
            .map(|f| match f.data_type() {
                DataType::Float64 => Cell::F64(Some(1234.5)),
                DataType::Int64 => Cell::I64(Some(0)),
                DataType::List(_) => Cell::StrList(None),
                _ => Cell::Str(Some("x")),
            })
            .collect();
        row[3] = Cell::Str(None);
        w.append(&row).unwrap();
        w.finish().unwrap();
        let p = partition_dir(dir.path(), Table::TicRate, "f").join("part-0.parquet");
        let b = &read_batches(&p)[0];
        let idx = b.schema().index_of("negotiated_rate").unwrap();
        let r = b
            .column(idx)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert_eq!(r.value(0), 1234.5);

        // Zero rows still produces a readable file.
        let w = TableWriter::create(dir.path(), Table::TicFile, "e").unwrap();
        assert_eq!(w.finish().unwrap(), 0);
        let p = partition_dir(dir.path(), Table::TicFile, "e").join("part-0.parquet");
        assert!(read_batches(&p).iter().all(|b| b.num_rows() == 0));
    }

    #[test]
    fn batches_flush_by_bytes_not_only_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = TableWriter::create(dir.path(), Table::Rejects, "b").unwrap();
        let big = "x".repeat(700 * 1024);
        for _ in 0..3 {
            w.append(&["b".into(), "loc".into(), "r".into(), Cell::Str(Some(&big))])
                .unwrap();
        }
        // 3 × 700KB crosses BATCH_MAX_BYTES, so the batch was encoded long
        // before reaching DEFAULT_BATCH_ROWS rows.
        assert_eq!(w.rows_written, 3);
        assert_eq!(w.pending, 0);
        assert_eq!(w.finish().unwrap(), 3);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate_utf8("abc", 10), "abc");
        assert_eq!(truncate_utf8("abcdef", 3), "abc");
        // 'é' is 2 bytes; cutting at 2 would split it.
        assert_eq!(truncate_utf8("aé", 2), "a");
    }

    #[test]
    fn reject_sink_truncates_snippet() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = RejectSink::create(dir.path(), "rr").unwrap();
        let big = "é".repeat(1000); // 2000 bytes
        r.reject("row 7", "bad money", Some(&big)).unwrap();
        r.reject("$.in_network[3]", "missing billing_code", None)
            .unwrap();
        assert_eq!(r.count(), 2);
        r.finish().unwrap();
        let p = partition_dir(dir.path(), Table::Rejects, "rr").join("part-0.parquet");
        let b = &read_batches(&p)[0];
        let snip = b.column(3).as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(snip.value(0).len(), 1024);
        assert!(snip.is_null(1));
    }
}
