//! Streaming TiC in-network reader.
//!
//! # Memory model
//!
//! An in-network file is `{ ...metadata, provider_references: [...],
//! in_network: [...] }` and can be 100GB+. Nothing larger than one
//! negotiated rate (or one provider reference) is held in memory:
//!
//! * `in_network[]` is visited element by element ([`InNetworkSeed`]).
//! * Each element is visited **key by key** ([`ElementVisitor`]) rather than
//!   read whole, because a single element (one billing code) can carry
//!   millions of `negotiated_rates`. Once the element's header fields
//!   (`billing_code`, `name`, ...) are known, rates are streamed straight to
//!   `tic_rate`. If `negotiated_rates` appears *before* the header fields
//!   (allowed by JSON, rare in practice), rates are buffered in a
//!   [`RateBuffer`] that spills to a temp file past a fixed count, and are
//!   replayed when the element ends.
//!
//! # Out-of-order `provider_references`
//!
//! Rates reference provider groups by id, and `tic_rate.provider_group_id`
//! is just that id, so **without NPI/TIN filters** rates can be written
//! whether or not their references have been seen yet: one pass, and ids
//! that are referenced but never defined are reported as rejects at the end.
//!
//! **With NPI/TIN filters**, whether a rate is kept depends on its groups,
//! so references must be known first. We use a *two-pass* read: pass 1
//! reads only `provider_references` (skipping everything else with
//! `IgnoredAny`, which tokenizes without allocating) and stops as soon as
//! that key has been read; pass 2 streams `in_network`. We chose two passes
//! over "buffer unresolved rates to temp Parquet and join" because:
//!
//! * the input is already staged as a local file, so a second pass costs
//!   decompression/tokenizing but no re-download;
//! * in the common layout (references first) pass 1 stops after the first
//!   few MB, so the extra cost is negligible;
//! * filtered-out rates are never materialized at all, which is the point
//!   of filtering, and there is no join step or temp data to manage.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::collections::BTreeSet;
use std::fmt;

use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use mrf_core::text::Utf8Repair;
use mrf_core::{Cell, RejectSink, Source, StagedInput, Table, TableWriter, ValueFilter};

use crate::json::{
    drive_object, get_str, snippet, str_list, str_of, Abort, EachElement, KeyHandler, ABORTED,
};
use crate::provider::{
    inline_group_id, parse_providers, reference_entry, ProviderGroup, ProviderSink, RemoteRefCache,
};
use crate::TicError;

/// Rates buffered in memory (per in_network element) before spilling.
const RATE_BUFFER_IN_MEMORY: usize = 4096;

/// Options for one TiC run.
#[derive(Clone, Default)]
pub struct TicOptions {
    pub npis: ValueFilter,
    pub tins: ValueFilter,
    pub codes: ValueFilter,
    /// Where URL inputs and rate spill files go (system temp if None).
    pub tmp_dir: Option<PathBuf>,
    /// Shared cache of remote provider-reference files.
    pub cache: Arc<RemoteRefCache>,
}

/// Row counts for one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TicCounts {
    pub providers: u64,
    pub rates: u64,
    pub bundle_codes: u64,
    pub rejects: u64,
    /// `in_network[]` elements seen / skipped by the code filter.
    pub in_network_items: u64,
    pub in_network_filtered: u64,
}

#[derive(Debug, Clone)]
pub struct TicSummary {
    pub source_uri: String,
    pub file_id: String,
    pub schema_version: Option<String>,
    pub counts: TicCounts,
    /// 1 or 2 (see module docs).
    pub passes: u8,
    pub raw_bytes: u64,
    pub elapsed: Duration,
}

/// Top-level metadata → `tic_file`.
#[derive(Debug, Clone, Default)]
struct FileMeta {
    schema_version: Option<String>,
    last_updated_on: Option<String>,
    reporting_entity_name: Option<String>,
    reporting_entity_type: Option<String>,
    plan_name: Option<String>,
    issuer_name: Option<String>,
    plan_sponsor_name: Option<String>,
    plan_id_type: Option<String>,
    plan_id: Option<String>,
    plan_market_type: Option<String>,
}

/// Everything the visitors write to.
struct Ctx {
    file_id: String,
    tmp_dir: Option<PathBuf>,
    codes: ValueFilter,
    cache: Arc<RemoteRefCache>,
    providers: ProviderSink,
    rates: TableWriter,
    bundles: TableWriter,
    rejects: RejectSink,
    meta: FileMeta,
    /// Group ids referenced by rates before being defined. Checked at the
    /// end; bounded by distinct ids, not by rates.
    pending_refs: BTreeSet<String>,
    refs_done: bool,
    saw_in_network: bool,
    items: u64,
    items_filtered: u64,
    abort: Abort<TicError>,
}

/// Parse one in-network file into `out`.
/// Stage `source` and parse every in-network file in it (one, or each
/// entry of a zip archive). Returns `(source_uri, result)` per file; a
/// source that cannot be staged yields a single error entry.
pub fn parse_all(
    source: &Source,
    out: &Path,
    opts: &TicOptions,
) -> Vec<(String, Result<TicSummary, TicError>)> {
    match StagedInput::stage_all(source, opts.tmp_dir.as_deref()) {
        Err(e) => vec![(source.uri(), Err(e.into()))],
        Ok(inputs) => inputs
            .iter()
            .map(|input| (input.uri.clone(), parse_staged(input, out, opts)))
            .collect(),
    }
}

/// Parse a source holding exactly one in-network file (a zip with one
/// entry is fine). For archives with several files use [`parse_all`].
pub fn parse_in_network_file(
    source: &Source,
    out: &Path,
    opts: &TicOptions,
) -> Result<TicSummary, TicError> {
    let inputs = StagedInput::stage_all(source, opts.tmp_dir.as_deref())?;
    match inputs.as_slice() {
        [one] => parse_staged(one, out, opts),
        many => Err(TicError::Input(mrf_core::InputError::Zip {
            uri: source.uri(),
            message: format!("archive holds {} files; use parse_all", many.len()),
        })),
    }
}

/// Parse one staged in-network file into `out`.
pub fn parse_staged(
    input: &StagedInput,
    out: &Path,
    opts: &TicOptions,
) -> Result<TicSummary, TicError> {
    // Every log event emitted while parsing this file carries its URI.
    let _span = tracing::warn_span!("file", src = %input.uri).entered();
    let started = Instant::now();
    let fid = input.file_id.clone();
    let mut ctx = Ctx {
        file_id: fid.clone(),
        tmp_dir: opts.tmp_dir.clone(),
        codes: opts.codes.clone(),
        cache: Arc::clone(&opts.cache),
        providers: ProviderSink::create(out, &fid, opts.npis.clone(), opts.tins.clone())?,
        rates: TableWriter::create(out, Table::TicRate, &fid)?,
        bundles: TableWriter::create(out, Table::TicBundleCode, &fid)?,
        rejects: RejectSink::create(out, &fid)?,
        meta: FileMeta::default(),
        pending_refs: BTreeSet::new(),
        refs_done: false,
        saw_in_network: false,
        items: 0,
        items_filtered: 0,
        abort: Abort::new(),
    };

    let mut passes = 1;
    if ctx.providers.filtering() {
        passes = 2;
        run_pass(input, &mut ctx, Pass::References)?;
    }
    let repaired = run_pass(input, &mut ctx, Pass::Main)?;
    if repaired > 0 {
        tracing::warn!(bytes = repaired, "invalid UTF-8 decoded as Windows-1252");
        ctx.rejects.reject(
            "$",
            &format!("{repaired} byte(s) of invalid UTF-8 decoded as Windows-1252 (file kept)"),
            None,
        )?;
    }

    if !ctx.saw_in_network {
        return Err(TicError::UnknownLayout(
            "not an in-network rates file (no `in_network` array)".into(),
        ));
    }
    // Ids referenced by rates but never defined anywhere in the file.
    let pending = std::mem::take(&mut ctx.pending_refs);
    for id in pending {
        if !ctx.providers.is_defined(&id) {
            ctx.rejects.reject(
                "$.provider_references",
                &format!(
                    "provider_group_id {id} is referenced by negotiated_rates but not defined"
                ),
                Some(&id),
            )?;
        }
    }

    write_file_row(out, input, &ctx.meta)?;
    let counts = TicCounts {
        providers: ctx.providers.finish()?,
        rates: ctx.rates.finish()?,
        bundle_codes: ctx.bundles.finish()?,
        rejects: ctx.rejects.finish()?,
        in_network_items: ctx.items,
        in_network_filtered: ctx.items_filtered,
    };
    Ok(TicSummary {
        source_uri: input.uri.clone(),
        file_id: fid,
        schema_version: ctx.meta.schema_version,
        counts,
        passes,
        raw_bytes: input.raw_bytes,
        elapsed: started.elapsed(),
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// Read `provider_references` only, then stop.
    References,
    /// Everything else (and references too, if pass 1 did not run).
    Main,
}

/// Marker error used to stop pass 1 early once references are read.
const STOP: &str = "__mrf_stop_after_references__";

/// Run one pass; returns how many invalid UTF-8 bytes were decoded as
/// Windows-1252 (see `mrf_core::text::Utf8Repair`).
fn run_pass(input: &StagedInput, ctx: &mut Ctx, pass: Pass) -> Result<u64, TicError> {
    let reader = Utf8Repair::new(input.open()?);
    let repaired = reader.counter();
    let mut h = Handler { ctx, pass };
    let r = drive_object(reader, &mut h);
    if let Some(e) = ctx.abort.0.take() {
        return Err(e);
    }
    let repaired = repaired.load(Ordering::Relaxed);
    match r {
        Ok(()) => Ok(repaired),
        Err(e) if pass == Pass::References && e.to_string().contains(STOP) => Ok(repaired),
        Err(e) if e.to_string().contains(ABORTED) => {
            Err(TicError::Json("aborted without a recorded error".into()))
        }
        Err(e) => Err(TicError::Json(e.to_string())),
    }
}

struct Handler<'c> {
    ctx: &'c mut Ctx,
    pass: Pass,
}

impl KeyHandler for Handler<'_> {
    fn on_key<'de, A: MapAccess<'de>>(&mut self, key: &str, map: &mut A) -> Result<(), A::Error> {
        let ctx = &mut *self.ctx;
        match (self.pass, key) {
            (Pass::References, "provider_references") => {
                read_references(ctx, map)?;
                ctx.refs_done = true;
                Err(de::Error::custom(STOP))
            }
            (Pass::References, _) => map.next_value::<IgnoredAny>().map(|_| ()),
            (Pass::Main, "provider_references") => {
                if ctx.refs_done {
                    map.next_value::<IgnoredAny>().map(|_| ())
                } else {
                    read_references(ctx, map)?;
                    ctx.refs_done = true;
                    Ok(())
                }
            }
            (Pass::Main, "in_network") => {
                ctx.saw_in_network = true;
                map.next_value_seed(InNetworkSeed { ctx })
            }
            (Pass::Main, _) => {
                let v: Value = map.next_value()?;
                let s = str_of(&v);
                let m = &mut ctx.meta;
                match key {
                    "version" => m.schema_version = s,
                    "last_updated_on" => m.last_updated_on = s,
                    "reporting_entity_name" => m.reporting_entity_name = s,
                    "reporting_entity_type" => m.reporting_entity_type = s,
                    "plan_name" => m.plan_name = s,
                    "issuer_name" => m.issuer_name = s,
                    "plan_sponsor_name" => m.plan_sponsor_name = s,
                    "plan_id_type" => m.plan_id_type = s,
                    "plan_id" => m.plan_id = s,
                    "plan_market_type" => m.plan_market_type = s,
                    _ => tracing::warn!(key, "unknown top-level key (ignored)"),
                }
                Ok(())
            }
        }
    }
}

fn read_references<'de, A: MapAccess<'de>>(ctx: &mut Ctx, map: &mut A) -> Result<(), A::Error> {
    let Ctx {
        providers,
        cache,
        rejects,
        abort,
        ..
    } = ctx;
    map.next_value_seed(EachElement {
        f: |i, v: Value| {
            let path = format!("$.provider_references[{i}]");
            abort.keep_going(reference_entry(&v, &path, providers, cache, rejects))
        },
    })
}

// --- in_network[] ---------------------------------------------------------

struct InNetworkSeed<'c> {
    ctx: &'c mut Ctx,
}

impl<'de> DeserializeSeed<'de> for InNetworkSeed<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for InNetworkSeed<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("the in_network array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut index = 0u64;
        while seq
            .next_element_seed(ElementSeed {
                ctx: &mut *self.ctx,
                index,
            })?
            .is_some()
        {
            index += 1;
        }
        Ok(())
    }
}

/// Fields of an `in_network[]` element that go on every `tic_rate` row.
#[derive(Debug, Default, Clone)]
struct ItemHeader {
    negotiation_arrangement: Option<String>,
    name: Option<String>,
    billing_code_type: Option<String>,
    billing_code_type_version: Option<String>,
    billing_code: Option<String>,
    description: Option<String>,
    severity_of_illness: Option<String>,
}

impl ItemHeader {
    /// All schema-required header fields seen, so rows can be written now.
    fn complete(&self, seen: &HeaderSeen) -> bool {
        seen.0 & HeaderSeen::REQUIRED == HeaderSeen::REQUIRED
    }
}

/// Bit set of header keys seen (present, even if null).
#[derive(Default)]
struct HeaderSeen(u8);

impl HeaderSeen {
    const REQUIRED: u8 = 0b0011_1111;
    fn mark(&mut self, key: &str) {
        let bit = match key {
            "negotiation_arrangement" => 0,
            "name" => 1,
            "billing_code_type" => 2,
            "billing_code_type_version" => 3,
            "billing_code" => 4,
            "description" => 5,
            "severity_of_illness" => 6,
            _ => return,
        };
        self.0 |= 1 << bit;
    }
}

struct ElementSeed<'c> {
    ctx: &'c mut Ctx,
    index: u64,
}

impl<'de> DeserializeSeed<'de> for ElementSeed<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        // Count every element, objects or not.
        self.ctx.items += 1;
        d.deserialize_any(ElementVisitor {
            ctx: self.ctx,
            index: self.index,
        })
    }
}

struct ElementVisitor<'c> {
    ctx: &'c mut Ctx,
    index: u64,
}

impl ElementVisitor<'_> {
    fn path(&self) -> String {
        format!("$.in_network[{}]", self.index)
    }

    fn not_object<E: de::Error>(self, what: &str) -> Result<(), E> {
        let path = self.path();
        let r = self
            .ctx
            .rejects
            .reject(
                &path,
                &format!("in_network element is {what}, not an object"),
                None,
            )
            .map_err(TicError::from);
        if self.ctx.abort.keep_going(r) {
            Ok(())
        } else {
            Err(de::Error::custom(ABORTED))
        }
    }
}

impl<'de> Visitor<'de> for ElementVisitor<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an in_network object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let path = self.path();
        let index = self.index;
        let ctx = self.ctx;

        let mut h = ItemHeader::default();
        let mut seen = HeaderSeen::default();
        let mut buffer: Option<RateBuffer> = None;
        let mut streamed = false;
        let mut skipped = false;
        let mut bundle_lists: Vec<(String, Value)> = Vec::new();

        while let Some(key) = map.next_key::<String>()? {
            seen.mark(&key);
            match key.as_str() {
                "negotiation_arrangement"
                | "name"
                | "billing_code_type"
                | "billing_code_type_version"
                | "billing_code"
                | "description"
                | "severity_of_illness" => {
                    let v = str_of(&map.next_value::<Value>()?);
                    if streamed && key == "severity_of_illness" {
                        tracing::warn!(%path, "severity_of_illness after negotiated_rates; not applied to rows already written");
                    }
                    match key.as_str() {
                        "negotiation_arrangement" => h.negotiation_arrangement = v,
                        "name" => h.name = v,
                        "billing_code_type" => h.billing_code_type = v,
                        "billing_code_type_version" => h.billing_code_type_version = v,
                        "billing_code" => h.billing_code = v,
                        "description" => h.description = v,
                        _ => h.severity_of_illness = v,
                    }
                }
                "negotiated_rates" => {
                    if h.complete(&seen) {
                        if !item_passes(ctx, &h) {
                            skipped = true;
                            map.next_value::<IgnoredAny>()?;
                            continue;
                        }
                        // Stream: each rate becomes rows immediately.
                        streamed = true;
                        let header = h.clone();
                        let path = path.clone();
                        let c = &mut *ctx;
                        map.next_value_seed(EachElement {
                            f: |j, v: Value| {
                                let r = emit_rate(
                                    c,
                                    &header,
                                    index,
                                    &format!("{path}.negotiated_rates[{j}]"),
                                    &v,
                                );
                                c.abort.keep_going(r)
                            },
                        })?;
                    } else {
                        // Header incomplete: buffer (spilling to disk) and
                        // replay once the element ends.
                        let mut buf = RateBuffer::new(ctx.tmp_dir.clone());
                        let c = &mut *ctx;
                        map.next_value_seed(EachElement {
                            f: |_, v: Value| {
                                let r = buf.push(v).map_err(TicError::from);
                                c.abort.keep_going(r)
                            },
                        })?;
                        buffer = Some(buf);
                    }
                }
                "bundled_codes" | "covered_services" => {
                    let v: Value = map.next_value()?;
                    bundle_lists.push((key, v));
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        // End of element: everything needing the full header happens here.
        let r = finish_element(
            ctx,
            &path,
            index,
            &h,
            buffer,
            streamed,
            skipped,
            &bundle_lists,
        );
        if ctx.abort.keep_going(r) {
            Ok(())
        } else {
            Err(de::Error::custom(ABORTED))
        }
    }

    // Non-object elements: reject and move on (the value is already consumed
    // or trivially small for scalars; arrays are drained).
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        self.not_object("an array")
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        self.not_object("null")
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        self.not_object("a boolean")
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        self.not_object("a number")
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        self.not_object("a number")
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        self.not_object("a number")
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        self.not_object("a string")
    }
}

fn item_passes(ctx: &mut Ctx, h: &ItemHeader) -> bool {
    let ok = ctx.codes.is_all()
        || h.billing_code
            .as_deref()
            .is_some_and(|c| ctx.codes.matches(c));
    if !ok {
        ctx.items_filtered += 1;
    }
    ok
}

#[allow(clippy::too_many_arguments)]
fn finish_element(
    ctx: &mut Ctx,
    path: &str,
    index: u64,
    h: &ItemHeader,
    buffer: Option<RateBuffer>,
    streamed: bool,
    skipped: bool,
    bundle_lists: &[(String, Value)],
) -> Result<(), TicError> {
    if skipped {
        return Ok(());
    }
    if h.billing_code.is_none() {
        let n = buffer.as_ref().map_or(0, |b| b.len());
        ctx.rejects.reject(
            path,
            &format!("in_network element has no billing_code ({n} negotiated_rates skipped)"),
            None,
        )?;
        return Ok(());
    }
    let passes = if streamed { true } else { item_passes(ctx, h) };
    if !passes {
        return Ok(());
    }
    match buffer {
        Some(buf) => buf.replay(|j, v| {
            emit_rate(ctx, h, index, &format!("{path}.negotiated_rates[{j}]"), &v)
        })?,
        None if !streamed => {
            ctx.rejects
                .reject(path, "in_network element has no negotiated_rates", None)?;
        }
        None => {}
    }
    for (key, list) in bundle_lists {
        write_bundle_codes(ctx, &format!("{path}.{key}"), index, list)?;
    }
    Ok(())
}

/// `bundled_codes` / `covered_services` → `tic_bundle_code`.
fn write_bundle_codes(ctx: &mut Ctx, path: &str, index: u64, list: &Value) -> Result<(), TicError> {
    let Some(items) = list.as_array() else {
        ctx.rejects
            .reject(path, "expected an array", Some(&snippet(list)))?;
        return Ok(());
    };
    for (k, c) in items.iter().enumerate() {
        let code = get_str(c, "billing_code");
        if !c.is_object() || code.is_none() {
            ctx.rejects.reject(
                &format!("{path}[{k}]"),
                "bundled/covered code entry has no billing_code",
                Some(&snippet(c)),
            )?;
            continue;
        }
        ctx.bundles.append(&[
            Cell::Str(Some(&ctx.file_id)),
            Cell::I64(Some(index as i64)),
            Cell::s(&code),
            Cell::s(&get_str(c, "billing_code_type")),
            Cell::s(&get_str(c, "billing_code_type_version")),
            Cell::s(&get_str(c, "description")),
        ])?;
    }
    Ok(())
}

/// One `negotiated_rates[]` entry → one `tic_rate` row per
/// (negotiated price × provider group that passes the filters).
fn emit_rate(
    ctx: &mut Ctx,
    h: &ItemHeader,
    index: u64,
    path: &str,
    v: &Value,
) -> Result<(), TicError> {
    if !v.is_object() {
        ctx.rejects.reject(
            path,
            "negotiated_rates entry is not an object",
            Some(&snippet(v)),
        )?;
        return Ok(());
    }

    // Resolve provider group ids.
    let mut groups: Vec<String> = Vec::new();
    let mut had_refs = false;
    if let Some(refs) = v.get("provider_references") {
        had_refs = true;
        match refs.as_array() {
            Some(ids) => {
                for id in ids.iter().filter_map(str_of) {
                    if !ctx.providers.is_defined(&id) {
                        ctx.pending_refs.insert(id.clone());
                    }
                    groups.push(id);
                }
            }
            None => ctx.rejects.reject(
                &format!("{path}.provider_references"),
                "provider_references is not an array",
                Some(&snippet(refs)),
            )?,
        }
    }
    if let Some(inline) = v.get("provider_groups") {
        had_refs = true;
        let id = inline_group_id(inline);
        if !ctx.providers.is_defined(&id) {
            let providers =
                parse_providers(inline, &format!("{path}.provider_groups"), &mut ctx.rejects)?;
            ctx.providers.write_group(&ProviderGroup {
                id: id.clone(),
                network_names: None,
                providers,
            })?;
        }
        groups.push(id);
    }
    if !had_refs {
        ctx.rejects.reject(
            path,
            "negotiated rate has neither provider_references nor provider_groups",
            Some(&snippet(v)),
        )?;
        return Ok(());
    }
    // Under filters an undefined id cannot be verified, so it does not pass.
    groups.retain(|g| ctx.providers.passes(g));
    if groups.is_empty() {
        return Ok(());
    }

    let Some(prices) = v.get("negotiated_prices").and_then(Value::as_array) else {
        ctx.rejects.reject(
            path,
            "negotiated rate has no negotiated_prices array",
            Some(&snippet(v)),
        )?;
        return Ok(());
    };
    for (k, p) in prices.iter().enumerate() {
        let ppath = format!("{path}.negotiated_prices[{k}]");
        let rate = match p.get("negotiated_rate") {
            Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()),
            Some(Value::String(s)) => s
                .trim()
                .replace(['$', ','], "")
                .parse::<f64>()
                .ok()
                .filter(|f| f.is_finite()),
            _ => None,
        };
        let Some(rate) = rate else {
            ctx.rejects.reject(
                &ppath,
                "negotiated price has no numeric negotiated_rate",
                Some(&snippet(p)),
            )?;
            continue;
        };
        let negotiated_type = get_str(p, "negotiated_type");
        let setting = get_str(p, "setting");
        let expiration = get_str(p, "expiration_date");
        let billing_class = get_str(p, "billing_class");
        let service_codes = str_list(p.get("service_code"));
        let modifiers = str_list(p.get("billing_code_modifier"));
        let info = get_str(p, "additional_information");
        for g in &groups {
            ctx.rates.append(&[
                Cell::Str(Some(&ctx.file_id)),
                Cell::I64(Some(index as i64)),
                Cell::Str(Some(g)),
                Cell::s(&h.negotiation_arrangement),
                Cell::s(&h.name),
                Cell::s(&h.billing_code),
                Cell::s(&h.billing_code_type),
                Cell::s(&h.billing_code_type_version),
                Cell::s(&h.description),
                Cell::s(&h.severity_of_illness),
                Cell::s(&setting),
                Cell::s(&negotiated_type),
                Cell::F64(Some(rate)),
                Cell::s(&expiration),
                Cell::s(&billing_class),
                Cell::StrList(service_codes.as_deref()),
                Cell::StrList(modifiers.as_deref()),
                Cell::s(&info),
            ])?;
        }
    }
    Ok(())
}

fn write_file_row(out: &Path, input: &StagedInput, m: &FileMeta) -> Result<(), TicError> {
    let mut w = TableWriter::create(out, Table::TicFile, &input.file_id)?;
    let uri = input.uri.clone();
    w.append(&[
        Cell::Str(Some(&input.file_id)),
        Cell::Str(Some(&uri)),
        Cell::Str(Some(&input.sha256)),
        Cell::s(&m.schema_version),
        Cell::s(&m.last_updated_on),
        Cell::s(&m.reporting_entity_name),
        Cell::s(&m.reporting_entity_type),
        Cell::s(&m.plan_name),
        Cell::s(&m.issuer_name),
        Cell::s(&m.plan_sponsor_name),
        Cell::s(&m.plan_id_type),
        Cell::s(&m.plan_id),
        Cell::s(&m.plan_market_type),
    ])?;
    w.finish()?;
    Ok(())
}

// --- rate buffer ------------------------------------------------------------

/// Holds negotiated rates for one element until its header is complete.
/// The first `RATE_BUFFER_IN_MEMORY` stay in memory; the rest go to an
/// anonymous temp file as JSON lines (deleted automatically on drop).
struct RateBuffer {
    mem: Vec<Value>,
    spill: Option<BufWriter<File>>,
    spilled: usize,
    tmp_dir: Option<PathBuf>,
}

impl RateBuffer {
    fn new(tmp_dir: Option<PathBuf>) -> RateBuffer {
        RateBuffer {
            mem: Vec::new(),
            spill: None,
            spilled: 0,
            tmp_dir,
        }
    }

    fn len(&self) -> usize {
        self.mem.len() + self.spilled
    }

    fn push(&mut self, v: Value) -> std::io::Result<()> {
        if self.mem.len() < RATE_BUFFER_IN_MEMORY {
            self.mem.push(v);
            return Ok(());
        }
        if self.spill.is_none() {
            let f = match &self.tmp_dir {
                Some(d) => tempfile::tempfile_in(d)?,
                None => tempfile::tempfile()?,
            };
            self.spill = Some(BufWriter::new(f));
        }
        let w = self.spill.as_mut().expect("spill file just created");
        serde_json::to_writer(&mut *w, &v)?;
        w.write_all(b"\n")?;
        self.spilled += 1;
        Ok(())
    }

    /// Feed every buffered rate, in original order, to `f`.
    fn replay(
        self,
        mut f: impl FnMut(usize, Value) -> Result<(), TicError>,
    ) -> Result<(), TicError> {
        let n_mem = self.mem.len();
        for (j, v) in self.mem.into_iter().enumerate() {
            f(j, v)?;
        }
        if let Some(w) = self.spill {
            let mut file = w.into_inner().map_err(|e| e.into_error())?;
            file.seek(SeekFrom::Start(0))?;
            for (k, line) in BufReader::new(file).lines().enumerate() {
                let v: Value = serde_json::from_str(&line?)
                    .map_err(|e| TicError::Json(format!("rate spill file: {e}")))?;
                f(n_mem + k, v)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rate_buffer_spills_and_replays_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = RateBuffer::new(Some(dir.path().to_path_buf()));
        let n = RATE_BUFFER_IN_MEMORY + 10;
        for i in 0..n {
            b.push(json!({"i": i})).unwrap();
        }
        assert_eq!(b.len(), n);
        assert!(b.spill.is_some());
        let mut got = Vec::new();
        b.replay(|j, v| {
            assert_eq!(v["i"], json!(j));
            got.push(j);
            Ok(())
        })
        .unwrap();
        assert_eq!(got.len(), n);
    }
}
