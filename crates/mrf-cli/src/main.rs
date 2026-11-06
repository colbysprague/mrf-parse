//! `mrf`: parse hospital and payer (TiC) price transparency MRFs to Parquet.
//!
//! ```text
//! mrf hospital <input>... --out <dir> [--codes codes.txt]
//! mrf tic <input>... --out <dir> [--npis npis.txt] [--tins tins.txt] [--codes codes.txt]
//! mrf toc <toc-input> [--plan-filter <substring>]
//! mrf validate <dir>
//! ```
//!
//! Files are processed in parallel on a bounded rayon pool (`--jobs`); each
//! file is parsed single-threaded. One failing file does not stop the
//! others, but makes the exit status non-zero.

mod validate;

use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use rayon::prelude::*;

use mrf_core::mem::{human_bytes, peak_rss_bytes};
use mrf_core::{Source, ValueFilter};
use mrf_hospital::{HospitalOptions, HospitalSummary};
use mrf_tic::{RemoteRefCache, TicOptions, TicSummary};

#[derive(Parser)]
#[command(
    name = "mrf",
    version,
    about = "Parse price transparency MRFs into Parquet"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Parse hospital MRFs (CSV tall, CSV wide, JSON; v2.x and v3.0).
    Hospital {
        #[command(flatten)]
        run: RunArgs,
        /// Keep only items with at least one of these codes (one per line).
        #[arg(long)]
        codes: Option<PathBuf>,
    },
    /// Parse TiC in-network rate files.
    Tic {
        #[command(flatten)]
        run: RunArgs,
        /// Keep only providers with these NPIs (one per line).
        #[arg(long)]
        npis: Option<PathBuf>,
        /// Keep only providers with these TIN values (one per line).
        #[arg(long)]
        tins: Option<PathBuf>,
        /// Keep only in_network items with these billing codes (one per line).
        #[arg(long)]
        codes: Option<PathBuf>,
    },
    /// Print the in-network file URLs listed in a TiC table of contents.
    Toc {
        /// TOC file path or URL (plain or gzip).
        input: String,
        /// Only files whose reporting plans include a plan name containing
        /// this text (case-insensitive).
        #[arg(long)]
        plan_filter: Option<String>,
    },
    /// Summarize an output directory: rows, null rates, rejects per file.
    Validate { dir: PathBuf },
}

#[derive(Args)]
struct RunArgs {
    /// Input paths or http(s) URLs (plain or gzip).
    inputs: Vec<String>,
    /// Read more inputs from this file, one per line (`-` for stdin).
    #[arg(long)]
    inputs_from: Option<PathBuf>,
    /// Output directory.
    #[arg(long)]
    out: PathBuf,
    /// Files processed in parallel (default: number of CPUs).
    #[arg(long)]
    jobs: Option<usize>,
    /// Where URL downloads are staged (default: system temp dir).
    #[arg(long)]
    tmp_dir: Option<PathBuf>,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        // Output piped into `head` etc.: the reader went away; not an error.
        Err(e)
            if e.chain().any(|c| {
                c.downcast_ref::<io::Error>()
                    .is_some_and(|io| io.kind() == io::ErrorKind::BrokenPipe)
            }) =>
        {
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("mrf: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Returns Ok(false) when some files failed.
fn run(cli: Cli) -> Result<bool> {
    match cli.cmd {
        Cmd::Hospital { run, codes } => {
            let opts = HospitalOptions {
                codes: load_filter(codes.as_deref())?,
                tmp_dir: run.tmp_dir.clone(),
            };
            let out = run.out.clone();
            run_files(&run, |src| {
                mrf_hospital::parse_all(src, &out, &opts)
                    .into_iter()
                    .map(|(uri, r)| (uri, r.map(|s| hospital_line(&s)).map_err(|e| e.to_string())))
                    .collect()
            })
        }
        Cmd::Tic {
            run,
            npis,
            tins,
            codes,
        } => {
            let opts = TicOptions {
                npis: load_filter(npis.as_deref())?,
                tins: load_filter(tins.as_deref())?,
                codes: load_filter(codes.as_deref())?,
                tmp_dir: run.tmp_dir.clone(),
                // One cache for the whole run, shared by all worker threads.
                cache: RemoteRefCache::new(),
            };
            let out = run.out.clone();
            run_files(&run, |src| {
                mrf_tic::parse_all(src, &out, &opts)
                    .into_iter()
                    .map(|(uri, r)| (uri, r.map(|s| tic_line(&s)).map_err(|e| e.to_string())))
                    .collect()
            })
        }
        Cmd::Toc { input, plan_filter } => toc(&input, plan_filter.as_deref()).map(|()| true),
        Cmd::Validate { dir } => validate::run(&dir).map(|()| true),
    }
}

fn load_filter(path: Option<&Path>) -> Result<ValueFilter> {
    match path {
        None => Ok(ValueFilter::all()),
        Some(p) => {
            ValueFilter::from_file(p).with_context(|| format!("reading filter {}", p.display()))
        }
    }
}

fn collect_inputs(run: &RunArgs) -> Result<Vec<String>> {
    let mut inputs = run.inputs.clone();
    if let Some(p) = &run.inputs_from {
        let reader: Box<dyn BufRead> = if p.as_os_str() == "-" {
            Box::new(io::stdin().lock())
        } else {
            Box::new(io::BufReader::new(
                std::fs::File::open(p).with_context(|| format!("opening {}", p.display()))?,
            ))
        };
        for line in reader.lines() {
            let line = line?;
            let t = line.trim();
            if !t.is_empty() && !t.starts_with('#') {
                inputs.push(t.to_string());
            }
        }
    }
    if inputs.is_empty() {
        anyhow::bail!("no inputs given");
    }
    Ok(inputs)
}

/// One parsed file: its `source_uri` and summary line or error.
type FileResult = (String, Result<String, String>);

/// Parse every input on a bounded pool. An input can yield several files
/// (zip archives); those are parsed one after another on the input's
/// worker. Progress lines go to stderr as files finish; the final summary
/// goes to stdout in input order.
fn run_files<F>(run: &RunArgs, parse: F) -> Result<bool>
where
    // `Sync` because rayon calls `parse` from several threads at once.
    F: Fn(&Source) -> Vec<FileResult> + Sync,
{
    let inputs = collect_inputs(run)?;
    std::fs::create_dir_all(&run.out).with_context(|| format!("creating {}", run.out.display()))?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(run.jobs.unwrap_or(0)) // 0 = rayon default (CPU count)
        .build()?;
    let stderr = Mutex::new(io::stderr());
    let started = Instant::now();

    let results: Vec<FileResult> = pool.install(|| {
        inputs
            .par_iter()
            .flat_map_iter(|input| {
                let files = parse(&Source::parse(input));
                for (uri, r) in &files {
                    let line = match r {
                        Ok(s) => format!("done  {uri}  {s}"),
                        Err(e) => format!("FAIL  {uri}: {e}"),
                    };
                    let _ = writeln!(stderr.lock().expect("stderr lock"), "{line}");
                }
                files
            })
            .collect()
    });

    let mut stdout = io::stdout().lock();
    let mut failed = 0;
    for (input, r) in &results {
        match r {
            Ok(s) => writeln!(stdout, "OK    {input}\n      {s}")?,
            Err(e) => {
                failed += 1;
                writeln!(stdout, "FAIL  {input}\n      {e}")?
            }
        }
    }
    writeln!(
        stdout,
        "{} file(s), {} failed, {:.2}s, peak RSS {}",
        results.len(),
        failed,
        started.elapsed().as_secs_f64(),
        peak_rss_bytes().map_or("n/a".into(), human_bytes)
    )?;
    Ok(failed == 0)
}

fn rss() -> String {
    peak_rss_bytes().map_or("n/a".into(), human_bytes)
}

fn hospital_line(s: &HospitalSummary) -> String {
    let c = &s.counts;
    let mut line = format!(
        "file_id={} layout={} version={} items={} item_codes={} charges={} rejects={} filtered_items={} input={} elapsed={:.2}s peak_rss={}",
        s.file_id,
        s.layout.as_str(),
        s.mrf_version.as_deref().unwrap_or("?"),
        c.items,
        c.item_codes,
        c.charges,
        c.rejects,
        s.filtered_out,
        human_bytes(s.raw_bytes),
        s.elapsed.as_secs_f64(),
        rss()
    );
    if let Some(m) = &s.version_mismatch {
        line.push_str(&format!(" WARNING: {m}"));
    }
    line
}

fn tic_line(s: &TicSummary) -> String {
    let c = &s.counts;
    format!(
        "file_id={} version={} passes={} in_network={} filtered_items={} providers={} rates={} bundle_codes={} rejects={} input={} elapsed={:.2}s peak_rss={}",
        s.file_id,
        s.schema_version.as_deref().unwrap_or("?"),
        s.passes,
        c.in_network_items,
        c.in_network_filtered,
        c.providers,
        c.rates,
        c.bundle_codes,
        c.rejects,
        human_bytes(s.raw_bytes),
        s.elapsed.as_secs_f64(),
        rss()
    )
}

/// Print distinct in-network URLs (first-seen order) to stdout.
fn toc(input: &str, plan_filter: Option<&str>) -> Result<()> {
    let reader = mrf_core::text::Utf8Repair::new(mrf_core::open(&Source::parse(input))?);
    let mut seen = HashSet::new();
    let mut stdout = io::stdout().lock();
    let mut write_err: Option<io::Error> = None;
    let meta = mrf_tic::toc::read_toc(reader, |e| {
        if plan_filter.is_some_and(|f| !e.plan_matches(f)) {
            return Ok(());
        }
        if seen.insert(e.location.clone()) {
            if let Err(err) = writeln!(stdout, "{}", e.location) {
                write_err.get_or_insert(err);
            }
        }
        Ok(())
    })?;
    if let Some(e) = write_err {
        // A closed pipe (`| head`) is not an error worth reporting.
        if e.kind() != io::ErrorKind::BrokenPipe {
            return Err(e.into());
        }
    }
    for (path, reason) in &meta.bad_structures {
        eprintln!("warning: {path}: {reason}");
    }
    eprintln!(
        "{} in-network file(s) from {} ({})",
        seen.len(),
        meta.reporting_entity_name.as_deref().unwrap_or("?"),
        meta.version.as_deref().unwrap_or("?")
    );
    Ok(())
}
