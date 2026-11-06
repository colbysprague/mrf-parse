//! Hospital price transparency MRF readers (CSV tall, CSV wide, JSON).
//!
//! Entry points:
//! * [`parse_all`]: stage a source (hash → `file_id`; zip archives expand to
//!   one input per entry) and parse every resulting file.
//! * [`parse_file`]: the same for a source known to hold one file.
//! * [`parse_staged`]: parse one already-staged input.
//!
//! Parsing detects the layout from content, runs the matching reader, and
//! writes the `hospital_*` tables plus `rejects` under the output directory.

pub mod csv_reader;
pub mod header;
pub mod json_reader;
pub mod model;

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mrf_core::{InputError, RejectSink, SinkError, Source, StagedInput, ValueFilter};

pub use model::{HospitalCounts, Layout};

#[derive(Debug, thiserror::Error)]
pub enum HospitalError {
    #[error(transparent)]
    Input(#[from] InputError),
    #[error(transparent)]
    Sink(#[from] SinkError),
    #[error("CSV read error: {0}")]
    Csv(#[from] csv::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unknown layout: {0}")]
    UnknownLayout(String),
    #[error("JSON error: {0}")]
    Json(String),
}

/// Options for one hospital run.
#[derive(Debug, Clone, Default)]
pub struct HospitalOptions {
    /// Keep only items with at least one code in this set.
    pub codes: ValueFilter,
    /// Where URL inputs are downloaded before parsing (system temp if None).
    pub tmp_dir: Option<PathBuf>,
}

/// Result of parsing one file.
#[derive(Debug, Clone)]
pub struct HospitalSummary {
    pub source_uri: String,
    pub file_id: String,
    pub layout: Layout,
    pub mrf_version: Option<String>,
    pub counts: HospitalCounts,
    /// Items dropped by the code filter.
    pub filtered_out: u64,
    /// Set when the declared `version` disagrees with the file's shape
    /// (logged, never fatal).
    pub version_mismatch: Option<String>,
    pub raw_bytes: u64,
    pub elapsed: Duration,
}

/// Stage `source` and parse every file in it (one, or each entry of a zip
/// archive). Returns `(source_uri, result)` per file, in archive order; a
/// source that cannot be staged at all yields a single error entry.
pub fn parse_all(
    source: &Source,
    out: &Path,
    opts: &HospitalOptions,
) -> Vec<(String, Result<HospitalSummary, HospitalError>)> {
    match StagedInput::stage_all(source, opts.tmp_dir.as_deref()) {
        Err(e) => vec![(source.uri(), Err(e.into()))],
        Ok(inputs) => inputs
            .iter()
            .map(|input| (input.uri.clone(), parse_staged(input, out, opts)))
            .collect(),
    }
}

/// Parse a source that holds exactly one file (a zip with one entry is
/// fine). For archives with several files use [`parse_all`].
pub fn parse_file(
    source: &Source,
    out: &Path,
    opts: &HospitalOptions,
) -> Result<HospitalSummary, HospitalError> {
    let inputs = StagedInput::stage_all(source, opts.tmp_dir.as_deref())?;
    match inputs.as_slice() {
        [one] => parse_staged(one, out, opts),
        many => Err(HospitalError::Input(InputError::Zip {
            uri: source.uri(),
            message: format!("archive holds {} files; use parse_all", many.len()),
        })),
    }
}

/// Parse one staged input into `out`.
pub fn parse_staged(
    input: &StagedInput,
    out: &Path,
    opts: &HospitalOptions,
) -> Result<HospitalSummary, HospitalError> {
    // Every log event emitted while parsing this file carries its URI.
    let _span = tracing::warn_span!("file", src = %input.uri).entered();
    let started = Instant::now();
    let mut reader = input.open()?;
    let layout_kind = sniff(&mut reader)?;

    let mut sink = model::HospitalSink::create(out, &input.file_id, opts.codes.clone())?;
    let mut rejects = RejectSink::create(out, &input.file_id)?;

    let (meta, layout, shape) = match layout_kind {
        Sniffed::Csv => {
            let r = csv_reader::read_csv(reader, &mut sink, &mut rejects)?;
            (r.meta, r.layout, r.shape_major)
        }
        Sniffed::Json => {
            // Stray Windows-1252 bytes would otherwise fail the whole file.
            let reader = mrf_core::text::Utf8Repair::new(reader);
            let repaired = reader.counter();
            let (meta, shape) = json_reader::read_json(reader, &mut sink, &mut rejects)?;
            let repaired = repaired.load(std::sync::atomic::Ordering::Relaxed);
            if repaired > 0 {
                tracing::warn!(bytes = repaired, "invalid UTF-8 decoded as Windows-1252");
                rejects.reject(
                    "$",
                    &format!(
                        "{repaired} byte(s) of invalid UTF-8 decoded as Windows-1252 (file kept)"
                    ),
                    None,
                )?;
            }
            (meta, Layout::Json, shape)
        }
    };
    let version_mismatch = version_mismatch(meta.mrf_version.as_deref(), shape);
    if let Some(msg) = &version_mismatch {
        tracing::warn!("{msg}");
    }

    let filtered_out = sink.filtered_out();
    let mut counts = sink.finish(out, input, layout, &meta)?;
    counts.rejects = rejects.finish()?;
    Ok(HospitalSummary {
        source_uri: input.uri.clone(),
        file_id: input.file_id.clone(),
        layout,
        mrf_version: meta.mrf_version,
        counts,
        filtered_out,
        version_mismatch,
        raw_bytes: input.raw_bytes,
        elapsed: started.elapsed(),
    })
}

/// Major CMS version implied by normalized header/key names: v3 added
/// `location_name`, `type_2_npi`, `attester_name`, `attestation` and the
/// allowed-amount fields; v2 used `hospital_location`, `affirmation` and
/// `estimated_amount`. `None` when there are no markers or they conflict.
pub(crate) fn shape_major<I: IntoIterator<Item = String>>(names: I) -> Option<u32> {
    let (mut v2, mut v3) = (false, false);
    for n in names {
        match n.as_str() {
            "location_name" | "type_2_npi" | "attester_name" | "attestation" | "median_amount"
            | "10th_percentile" | "90th_percentile" => v3 = true,
            "hospital_location" | "affirmation" | "estimated_amount" => v2 = true,
            _ => {}
        }
    }
    match (v2, v3) {
        (true, false) => Some(2),
        (false, true) => Some(3),
        _ => None,
    }
}

fn version_mismatch(declared: Option<&str>, shape: Option<u32>) -> Option<String> {
    let shape = shape?;
    let declared = declared?;
    let major: u32 = declared
        .trim()
        .trim_start_matches(['v', 'V'])
        .split('.')
        .next()?
        .parse()
        .ok()?;
    (major != shape)
        .then(|| format!("declared version {declared:?} but file shape looks like v{shape}.x"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sniffed {
    Csv,
    Json,
}

/// Look at the first non-whitespace byte (after an optional UTF-8 BOM)
/// without consuming it: `{` means JSON, anything else is treated as CSV
/// and validated by the CSV reader's header checks.
fn sniff(r: &mut dyn BufRead) -> Result<Sniffed, HospitalError> {
    // `fill_buf` exposes the reader's internal buffer (256KB), so peeking
    // does not consume input. Leading whitespace and the BOM are consumed;
    // neither matters to the CSV or JSON readers.
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return Err(HospitalError::UnknownLayout("file is empty".into()));
        }
        if buf.starts_with(b"\xef\xbb\xbf") {
            r.consume(3);
            continue;
        }
        let ws = buf.iter().take_while(|b| b.is_ascii_whitespace()).count();
        // Copy the byte out before `consume`, which needs `r` mutably again.
        let first = buf.get(ws).copied();
        r.consume(ws);
        match first {
            None => continue, // buffer was all whitespace; refill
            Some(b'{') => return Ok(Sniffed::Json),
            Some(_) => return Ok(Sniffed::Csv),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_and_mismatch() {
        let s = |v: &[&str]| shape_major(v.iter().map(|x| x.to_string()));
        assert_eq!(s(&["location_name", "median_amount"]), Some(3));
        assert_eq!(s(&["hospital_location", "estimated_amount"]), Some(2));
        assert_eq!(s(&["hospital_location", "type_2_npi"]), None);
        assert_eq!(s(&["description"]), None);
        assert!(version_mismatch(Some("2.0.0"), Some(3)).is_some());
        assert!(version_mismatch(Some("3.0.0"), Some(3)).is_none());
        assert!(version_mismatch(Some("v2.2"), Some(2)).is_none());
        assert!(version_mismatch(Some("garbage"), Some(2)).is_none());
    }
}
