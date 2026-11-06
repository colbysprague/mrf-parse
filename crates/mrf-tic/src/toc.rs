//! Table-of-contents (TOC) reader.
//!
//! A TOC lists, per `reporting_structure[]` entry, the plans it covers and
//! the in-network files that hold their rates. Real TOCs can be several GB
//! (one entry per employer plan), so `reporting_structure` is streamed one
//! entry at a time.

use std::io::Read;

use serde::de::{IgnoredAny, MapAccess};
use serde_json::Value;

use crate::json::{drive_objects, get_str, Abort, EachElement, KeyHandler};
use crate::TicError;

/// Plan metadata from `reporting_plans[]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TocPlan {
    pub plan_name: Option<String>,
    pub plan_id_type: Option<String>,
    pub plan_id: Option<String>,
    pub plan_sponsor_name: Option<String>,
    pub issuer_name: Option<String>,
    pub plan_market_type: Option<String>,
}

/// One in-network file listed in one reporting structure, with the plans
/// that structure covers. The same URL often appears in many structures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocEntry {
    /// Index of the `reporting_structure[]` entry.
    pub structure_index: usize,
    pub location: String,
    pub description: Option<String>,
    pub plans: Vec<TocPlan>,
}

impl TocEntry {
    /// Case-insensitive substring match on any plan name.
    pub fn plan_matches(&self, needle: &str) -> bool {
        let needle = needle.to_lowercase();
        self.plans.iter().any(|p| {
            p.plan_name
                .as_deref()
                .is_some_and(|n| n.to_lowercase().contains(&needle))
        })
    }
}

/// TOC top-level metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TocMeta {
    pub reporting_entity_name: Option<String>,
    pub reporting_entity_type: Option<String>,
    pub last_updated_on: Option<String>,
    pub version: Option<String>,
    /// Structures that could not be read (not objects, no files); reported,
    /// not fatal.
    pub bad_structures: Vec<(String, String)>,
}

/// Stream a TOC, calling `on_entry` for every in-network file of every
/// reporting structure, in file order.
pub fn read_toc<R: Read>(
    input: R,
    mut on_entry: impl FnMut(TocEntry) -> Result<(), TicError>,
) -> Result<TocMeta, TicError> {
    let mut h = TocHandler {
        meta: TocMeta::default(),
        on_entry: &mut on_entry,
        abort: Abort::new(),
        saw_structure: false,
    };
    // `drive_objects`: some payers (CareFirst) publish `[toc, toc, ...]`.
    let r = drive_objects(input, &mut h);
    if let Some(e) = h.abort.0.take() {
        return Err(e);
    }
    r.map_err(|e| TicError::Json(e.to_string()))?;
    if !h.saw_structure {
        return Err(TicError::UnknownLayout(
            "not a table-of-contents file (no `reporting_structure`)".into(),
        ));
    }
    Ok(h.meta)
}

struct TocHandler<'f, F> {
    meta: TocMeta,
    on_entry: &'f mut F,
    abort: Abort<TicError>,
    saw_structure: bool,
}

impl<F: FnMut(TocEntry) -> Result<(), TicError>> KeyHandler for TocHandler<'_, F> {
    fn on_key<'de, A: MapAccess<'de>>(&mut self, key: &str, map: &mut A) -> Result<(), A::Error> {
        match key {
            "reporting_structure" => {
                self.saw_structure = true;
                let on_entry = &mut *self.on_entry;
                let abort = &mut self.abort;
                let bad = &mut self.meta.bad_structures;
                map.next_value_seed(EachElement {
                    f: |i, v: Value| abort.keep_going(structure(i, &v, on_entry, bad)),
                })
            }
            "reporting_entity_name" | "reporting_entity_type" | "last_updated_on" | "version" => {
                let v: Value = map.next_value()?;
                let s = crate::json::str_of(&v);
                match key {
                    "reporting_entity_name" => self.meta.reporting_entity_name = s,
                    "reporting_entity_type" => self.meta.reporting_entity_type = s,
                    "last_updated_on" => self.meta.last_updated_on = s,
                    _ => self.meta.version = s,
                }
                Ok(())
            }
            _ => map.next_value::<IgnoredAny>().map(|_| ()),
        }
    }
}

fn structure(
    i: usize,
    v: &Value,
    on_entry: &mut impl FnMut(TocEntry) -> Result<(), TicError>,
    bad: &mut Vec<(String, String)>,
) -> Result<(), TicError> {
    let path = format!("$.reporting_structure[{i}]");
    if !v.is_object() {
        bad.push((path, "reporting structure is not an object".into()));
        return Ok(());
    }
    let plans: Vec<TocPlan> = v
        .get("reporting_plans")
        .and_then(Value::as_array)
        .map(|ps| {
            ps.iter()
                .map(|p| TocPlan {
                    plan_name: get_str(p, "plan_name"),
                    plan_id_type: get_str(p, "plan_id_type"),
                    plan_id: get_str(p, "plan_id"),
                    plan_sponsor_name: get_str(p, "plan_sponsor_name"),
                    issuer_name: get_str(p, "issuer_name"),
                    plan_market_type: get_str(p, "plan_market_type"),
                })
                .collect()
        })
        .unwrap_or_default();
    let files = v.get("in_network_files").and_then(Value::as_array);
    let mut n = 0;
    for f in files.into_iter().flatten() {
        match get_str(f, "location") {
            Some(location) => {
                n += 1;
                on_entry(TocEntry {
                    structure_index: i,
                    location,
                    description: get_str(f, "description"),
                    plans: plans.clone(),
                })?;
            }
            None => bad.push((
                path.clone(),
                "in_network_files entry has no location".into(),
            )),
        }
    }
    if n == 0 && v.get("allowed_amount_file").is_none() {
        bad.push((path, "reporting structure lists no files".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> std::fs::File {
        std::fs::File::open(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/tic/table-of-contents-sample.json"
        ))
        .unwrap()
    }

    #[test]
    fn cms_toc_example() {
        let mut entries = Vec::new();
        let meta = read_toc(fixture(), |e| {
            entries.push(e);
            Ok(())
        })
        .unwrap();
        assert_eq!(meta.reporting_entity_name.as_deref(), Some("medicare"));
        assert_eq!(meta.version.as_deref(), Some("2.0.0"));
        assert!(meta.bad_structures.is_empty());
        let urls: Vec<_> = entries.iter().map(|e| e.location.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://www.some_site.com/files/in-network-file-123456.json",
                "https://www.some_site.com/files/behavioral-health-0000.json",
                "https://www.some_site.com/files/chip-in-network-file.json",
            ]
        );
        assert_eq!(entries[0].plans.len(), 2);
        assert_eq!(entries[0].plans[1].plan_id.as_deref(), Some("000000000"));
        assert_eq!(entries[2].structure_index, 1);
    }

    #[test]
    fn plan_filter_substring() {
        let mut hits = Vec::new();
        read_toc(fixture(), |e| {
            if e.plan_matches("CHI") {
                hits.push(e.location);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            hits,
            vec!["https://www.some_site.com/files/chip-in-network-file.json"]
        );
    }

    #[test]
    fn bad_structures_are_reported_not_fatal() {
        let doc = r#"{"reporting_structure": [42, {"reporting_plans": [], "in_network_files": [{"description": "x"}]},
            {"reporting_plans": [{"plan_name": "p"}], "in_network_files": [{"location": "https://a/b.json"}]}]}"#;
        let mut n = 0;
        let meta = read_toc(doc.as_bytes(), |_| {
            n += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(meta.bad_structures.len(), 3);
    }

    #[test]
    fn top_level_array_of_tocs() {
        // CareFirst publishes some tables of contents in this shape.
        let doc = r#"[
            {"reporting_entity_name": "CareFirst Inc", "version": "2.0.0", "reporting_structure": [
                {"reporting_plans": [{"plan_name": "PPO"}], "in_network_files": [{"location": "https://a/1.json"}]}]},
            {"reporting_entity_name": "CareFirst Inc", "version": "2.0.0", "reporting_structure": [
                {"reporting_plans": [{"plan_name": "HMO"}], "in_network_files": [{"location": "https://a/2.json"}]}]}
        ]"#;
        let mut urls = Vec::new();
        let meta = read_toc(doc.as_bytes(), |e| {
            urls.push(e.location);
            Ok(())
        })
        .unwrap();
        assert_eq!(urls, vec!["https://a/1.json", "https://a/2.json"]);
        assert_eq!(meta.reporting_entity_name.as_deref(), Some("CareFirst Inc"));
    }

    #[test]
    fn non_toc_is_unknown_layout() {
        let err = read_toc(r#"{"in_network": []}"#.as_bytes(), |_| Ok(())).unwrap_err();
        assert!(matches!(err, TicError::UnknownLayout(_)));
    }
}
