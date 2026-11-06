//! Provider groups: parsing, filtering, and writing `tic_provider`.
//!
//! A negotiated rate points at providers in one of two ways:
//!
//! * `provider_references: [1, 2]` — ids into the file's top-level
//!   `provider_references[]`. Each entry there has inline `provider_groups`
//!   or a `location` URL of a separate provider-reference file.
//! * `provider_groups: [...]` inline on the rate (older files). These get a
//!   synthetic `provider_group_id` (a hash of the canonical JSON), so
//!   `tic_rate` always joins to `tic_provider` on
//!   (`file_id`, `provider_group_id`).
//!
//! `tic_provider` has one row per NPI (null NPI when a group lists none).
//! NPI/TIN filters are applied per row; a group "passes" if any of its rows
//! pass, and only passing rows are written.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::de::{IgnoredAny, MapAccess};
use serde_json::Value;

use mrf_core::{open, stable_id, Cell, RejectSink, Source, Table, TableWriter, ValueFilter};

use crate::json::{drive_object, get_str, snippet, str_list, str_of, EachElement, KeyHandler};
use crate::TicError;

/// One `provider_groups[]` entry: a TIN and its NPIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub npis: Vec<String>,
    pub tin_type: Option<String>,
    pub tin_value: Option<String>,
    pub tin_business_name: Option<String>,
}

/// A provider group as written to `tic_provider`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderGroup {
    pub id: String,
    pub network_names: Option<Vec<String>>,
    pub providers: Vec<Provider>,
}

/// Parse a `provider_groups` array. Bad entries become rejects.
pub fn parse_providers(
    v: &Value,
    path: &str,
    rejects: &mut RejectSink,
) -> Result<Vec<Provider>, TicError> {
    let Some(items) = v.as_array() else {
        rejects.reject(path, "provider_groups is not an array", Some(&snippet(v)))?;
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, p) in items.iter().enumerate() {
        match parse_provider(p) {
            Ok(p) => out.push(p),
            Err(reason) => rejects.reject(&format!("{path}[{i}]"), reason, Some(&snippet(p)))?,
        }
    }
    Ok(out)
}

fn parse_provider(p: &Value) -> Result<Provider, &'static str> {
    if !p.is_object() {
        return Err("provider group entry is not an object");
    }
    let tin = p.get("tin");
    let provider = Provider {
        npis: str_list(p.get("npi")).unwrap_or_default(),
        tin_type: tin.and_then(|t| get_str(t, "type")),
        tin_value: tin.and_then(|t| get_str(t, "value")),
        tin_business_name: tin.and_then(|t| get_str(t, "business_name")),
    };
    if provider.npis.is_empty() && provider.tin_value.is_none() {
        return Err("provider group entry has neither npi nor tin");
    }
    Ok(provider)
}

/// Deterministic id for an inline `provider_groups` array: `inline-` plus
/// the hash of its canonical JSON (object keys sorted, so key order in the
/// file does not matter).
pub fn inline_group_id(groups: &Value) -> String {
    let mut s = String::new();
    canonical_json(groups, &mut s);
    format!("inline-{}", stable_id([Some(s.as_str())]))
}

/// Serialize with object keys sorted. Written by hand so the result does
/// not depend on whether serde_json's `preserve_order` feature is enabled
/// somewhere in the dependency graph.
fn canonical_json(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                canonical_json(&m[k], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(x, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// Fetched provider-reference files, keyed by URL. Shared across files (and
/// threads) in one run, so a `location` used by many in-network files is
/// fetched once. Failures are cached too, so a dead URL is not retried for
/// every reference to it.
#[derive(Default)]
pub struct RemoteRefCache {
    inner: Mutex<HashMap<String, FetchResult>>,
}

/// Providers from one remote file, or why it could not be loaded.
type FetchResult = Result<Arc<Vec<Provider>>, String>;

impl RemoteRefCache {
    pub fn new() -> Arc<RemoteRefCache> {
        Arc::new(RemoteRefCache::default())
    }

    /// Get the provider groups at `url`, fetching on first use.
    pub fn get(&self, url: &str) -> FetchResult {
        if let Some(hit) = self.inner.lock().expect("cache lock").get(url) {
            return hit.clone();
        }
        // Fetch without holding the lock so other threads are not blocked
        // on network I/O. Two threads may race to fetch the same URL; both
        // get the same answer and the second insert is harmless.
        let fetched = fetch_provider_file(url).map(Arc::new);
        self.inner
            .lock()
            .expect("cache lock")
            .insert(url.to_string(), fetched.clone());
        fetched
    }
}

/// Read a provider-reference file: `{"provider_groups": [...], ...}`.
/// Streamed; bad entries are skipped (counted in the error only if no
/// entries at all could be read).
fn fetch_provider_file(url: &str) -> Result<Vec<Provider>, String> {
    struct H {
        providers: Vec<Provider>,
        bad: usize,
        saw_groups: bool,
    }
    impl KeyHandler for H {
        fn on_key<'de, A: MapAccess<'de>>(
            &mut self,
            key: &str,
            map: &mut A,
        ) -> Result<(), A::Error> {
            if key == "provider_groups" {
                self.saw_groups = true;
                let (providers, bad) = (&mut self.providers, &mut self.bad);
                map.next_value_seed(EachElement {
                    f: |_, v: Value| {
                        match parse_provider(&v) {
                            Ok(p) => providers.push(p),
                            Err(_) => *bad += 1,
                        }
                        true
                    },
                })
            } else {
                map.next_value::<IgnoredAny>().map(|_| ())
            }
        }
    }
    let reader = open(&Source::parse(url)).map_err(|e| e.to_string())?;
    let mut h = H {
        providers: Vec::new(),
        bad: 0,
        saw_groups: false,
    };
    drive_object(reader, &mut h).map_err(|e| format!("invalid provider reference file: {e}"))?;
    if !h.saw_groups {
        return Err("provider reference file has no provider_groups".into());
    }
    if h.bad > 0 {
        tracing::warn!(
            url,
            bad = h.bad,
            "skipped malformed entries in provider reference file"
        );
    }
    Ok(h.providers)
}

/// Writes `tic_provider` for one in-network file and remembers which group
/// ids exist and which pass the NPI/TIN filters.
pub struct ProviderSink {
    file_id: String,
    writer: TableWriter,
    npis: ValueFilter,
    tins: ValueFilter,
    /// Every group id defined so far (top-level or inline).
    defined: HashSet<String>,
    /// Group ids with at least one row passing the filters. Only maintained
    /// when a filter is active (otherwise every group passes).
    passing: HashSet<String>,
}

impl ProviderSink {
    pub fn create(
        out: &std::path::Path,
        file_id: &str,
        npis: ValueFilter,
        tins: ValueFilter,
    ) -> Result<ProviderSink, TicError> {
        Ok(ProviderSink {
            file_id: file_id.to_string(),
            writer: TableWriter::create(out, Table::TicProvider, file_id)?,
            npis,
            tins,
            defined: HashSet::new(),
            passing: HashSet::new(),
        })
    }

    pub fn filtering(&self) -> bool {
        !(self.npis.is_all() && self.tins.is_all())
    }

    pub fn is_defined(&self, id: &str) -> bool {
        self.defined.contains(id)
    }

    /// Whether rates for this group should be written.
    pub fn passes(&self, id: &str) -> bool {
        !self.filtering() || self.passing.contains(id)
    }

    /// Write the group's rows that pass the filters (once per id; a group
    /// id seen again is ignored). Returns whether the group passes.
    pub fn write_group(&mut self, g: &ProviderGroup) -> Result<bool, TicError> {
        if !self.defined.insert(g.id.clone()) {
            return Ok(self.passes(&g.id));
        }
        let mut any = false;
        for p in &g.providers {
            let tin_ok =
                self.tins.is_all() || p.tin_value.as_deref().is_some_and(|t| self.tins.matches(t));
            if !tin_ok {
                continue;
            }
            if p.npis.is_empty() {
                if self.npis.is_all() {
                    self.row(g, p, None)?;
                    any = true;
                }
                continue;
            }
            for npi in &p.npis {
                if self.npis.matches(npi) {
                    self.row(g, p, Some(npi))?;
                    any = true;
                }
            }
        }
        if any && self.filtering() {
            self.passing.insert(g.id.clone());
        }
        Ok(!self.filtering() || any)
    }

    fn row(&mut self, g: &ProviderGroup, p: &Provider, npi: Option<&str>) -> Result<(), TicError> {
        self.writer.append(&[
            Cell::Str(Some(&self.file_id)),
            Cell::Str(Some(&g.id)),
            Cell::StrList(g.network_names.as_deref()),
            Cell::s(&p.tin_type),
            Cell::s(&p.tin_value),
            Cell::Str(npi),
            Cell::s(&p.tin_business_name),
        ])?;
        Ok(())
    }

    pub fn finish(self) -> Result<u64, TicError> {
        Ok(self.writer.finish()?)
    }
}

/// Process one top-level `provider_references[]` entry: resolve its groups
/// (inline or via `location`) and write them.
pub fn reference_entry(
    v: &Value,
    path: &str,
    sink: &mut ProviderSink,
    cache: &RemoteRefCache,
    rejects: &mut RejectSink,
) -> Result<(), TicError> {
    let Some(id) = v.get("provider_group_id").and_then(str_of) else {
        rejects.reject(
            path,
            "provider reference has no provider_group_id",
            Some(&snippet(v)),
        )?;
        return Ok(());
    };
    let network_names = str_list(v.get("network_name"));
    let providers = if let Some(groups) = v.get("provider_groups") {
        parse_providers(groups, &format!("{path}.provider_groups"), rejects)?
    } else if let Some(url) = get_str(v, "location") {
        match cache.get(&url) {
            Ok(p) => p.as_ref().clone(),
            Err(e) => {
                rejects.reject(
                    &format!("{path}.location"),
                    &format!("cannot load provider reference {url}: {e}"),
                    Some(&url),
                )?;
                return Ok(());
            }
        }
    } else {
        rejects.reject(
            path,
            "provider reference has neither provider_groups nor location",
            Some(&snippet(v)),
        )?;
        return Ok(());
    };
    sink.write_group(&ProviderGroup {
        id,
        network_names,
        providers,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mrf_core::read::read_rows;
    use mrf_core::testutil::TestServer;
    use serde_json::json;

    struct Ctx {
        _dir: tempfile::TempDir,
        out: std::path::PathBuf,
        sink: ProviderSink,
        rejects: RejectSink,
    }

    fn ctx(npis: ValueFilter, tins: ValueFilter) -> Ctx {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().to_path_buf();
        Ctx {
            sink: ProviderSink::create(&out, "f1", npis, tins).unwrap(),
            rejects: RejectSink::create(&out, "f1").unwrap(),
            out,
            _dir: dir,
        }
    }

    type Rows = Vec<Vec<Option<String>>>;

    fn finish(c: Ctx) -> (Rows, Rows) {
        c.sink.finish().unwrap();
        c.rejects.finish().unwrap();
        (
            read_rows(&c.out, Table::TicProvider).unwrap(),
            read_rows(&c.out, Table::Rejects).unwrap(),
        )
    }

    fn entry() -> Value {
        json!({"provider_group_id": 1, "network_name": ["Net A"], "provider_groups": [
            {"npi": [1111111111u64, 2222222222u64], "tin": {"type": "ein", "value": "11-1111111", "business_name": "ACME"}},
            {"npi": [], "tin": {"type": "npi", "value": "3333333333"}}
        ]})
    }

    #[test]
    fn top_level_inline_groups_explode_per_npi() {
        let mut c = ctx(ValueFilter::all(), ValueFilter::all());
        let cache = RemoteRefCache::default();
        reference_entry(
            &entry(),
            "$.provider_references[0]",
            &mut c.sink,
            &cache,
            &mut c.rejects,
        )
        .unwrap();
        assert!(c.sink.passes("1") && c.sink.is_defined("1"));
        let (rows, rejects) = finish(c);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][1].as_deref(), Some("1"));
        assert_eq!(rows[0][2].as_deref(), Some("[Net A]"));
        assert_eq!(rows[0][5].as_deref(), Some("1111111111"));
        assert_eq!(rows[0][6].as_deref(), Some("ACME"));
        // Group with no NPIs: one row with null NPI, tin.type kept raw.
        assert_eq!(rows[2][3].as_deref(), Some("npi"));
        assert_eq!(rows[2][5], None);
        assert!(rejects.is_empty());
    }

    #[test]
    fn npi_and_tin_filters() {
        let mut c = ctx(ValueFilter::from_values(["2222222222"]), ValueFilter::all());
        let cache = RemoteRefCache::default();
        reference_entry(&entry(), "$", &mut c.sink, &cache, &mut c.rejects).unwrap();
        let other = json!({"provider_group_id": 2, "provider_groups": [{"npi": [9], "tin": {"type": "ein", "value": "x"}}]});
        reference_entry(&other, "$", &mut c.sink, &cache, &mut c.rejects).unwrap();
        assert!(c.sink.passes("1"));
        assert!(!c.sink.passes("2"));
        assert!(c.sink.is_defined("2"));
        let (rows, _) = finish(c);
        assert_eq!(rows.len(), 1, "only the matching NPI row");
        assert_eq!(rows[0][5].as_deref(), Some("2222222222"));

        let mut c = ctx(ValueFilter::all(), ValueFilter::from_values(["3333333333"]));
        reference_entry(&entry(), "$", &mut c.sink, &cache, &mut c.rejects).unwrap();
        let (rows, _) = finish(c);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][4].as_deref(), Some("3333333333"));
    }

    #[test]
    fn remote_location_fetched_once_and_cached() {
        let body = serde_json::to_vec(&json!({"version": "1.0.0", "provider_groups": [
            {"npi": [5555555555u64], "tin": {"type": "ein", "value": "55-5555555"}}
        ]}))
        .unwrap();
        let server = TestServer::start(vec![("/refs/a.json".into(), body)]);
        let url = server.url("/refs/a.json");
        let cache = RemoteRefCache::default();
        let mut c = ctx(ValueFilter::all(), ValueFilter::all());
        for id in [7, 8] {
            let e = json!({"provider_group_id": id, "location": url});
            reference_entry(&e, "$", &mut c.sink, &cache, &mut c.rejects).unwrap();
        }
        assert_eq!(server.hits(), 1, "second reference served from cache");
        // A dead URL becomes a reject, not a failure, and is cached too.
        let dead = json!({"provider_group_id": 9, "location": server.url("/missing.json")});
        reference_entry(
            &dead,
            "$.provider_references[2]",
            &mut c.sink,
            &cache,
            &mut c.rejects,
        )
        .unwrap();
        reference_entry(
            &dead,
            "$.provider_references[3]",
            &mut c.sink,
            &cache,
            &mut c.rejects,
        )
        .unwrap();
        assert_eq!(server.hits(), 2);
        let (rows, rejects) = finish(c);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1].as_deref(), Some("7"));
        assert_eq!(rows[1][5].as_deref(), Some("5555555555"));
        assert_eq!(rejects.len(), 2);
        assert_eq!(
            rejects[0][1].as_deref(),
            Some("$.provider_references[2].location")
        );
    }

    #[test]
    fn malformed_entries_rejected() {
        let mut c = ctx(ValueFilter::all(), ValueFilter::all());
        let cache = RemoteRefCache::default();
        for e in [
            json!({"provider_groups": []}),
            json!({"provider_group_id": 3}),
            json!({"provider_group_id": 4, "provider_groups": [42, {"tin": {}}]}),
        ] {
            reference_entry(&e, "$.p", &mut c.sink, &cache, &mut c.rejects).unwrap();
        }
        let (rows, rejects) = finish(c);
        assert!(rows.is_empty());
        assert_eq!(rejects.len(), 4);
    }

    #[test]
    fn inline_ids_are_deterministic_and_key_order_independent() {
        let a = json!([{"npi": [1], "tin": {"type": "ein", "value": "x"}}]);
        let b: Value =
            serde_json::from_str(r#"[{"tin": {"value": "x", "type": "ein"}, "npi": [1]}]"#)
                .unwrap();
        assert_eq!(inline_group_id(&a), inline_group_id(&b));
        assert!(inline_group_id(&a).starts_with("inline-"));
        let c = json!([{"npi": [2], "tin": {"type": "ein", "value": "x"}}]);
        assert_ne!(inline_group_id(&a), inline_group_id(&c));
    }
}
