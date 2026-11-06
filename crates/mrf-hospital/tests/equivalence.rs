//! Equivalence across layouts.
//!
//! The CMS tall, wide, and JSON examples describe the same hospital. After
//! parsing, `hospital_item`, `hospital_item_code`, and `hospital_charge`
//! must match across layouts as sorted multisets, ignoring `item_id` (which
//! hashes `file_id`).
//!
//! The CMS example files are not perfectly consistent with each other. The
//! parsers keep text raw, so the comparison here:
//!
//! 1. applies a few **test-only** normalizations for differences that come
//!    from the layouts themselves (documented in [`Normalize`]), and
//! 2. asserts that whatever still differs is *exactly* the list of known
//!    discrepancies in the CMS source files (each listed with its cause).
//!
//! So any parser change that alters output fails this test, while the
//! documented upstream inconsistencies do not.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use common::{parse, rows};
use mrf_core::Table;

/// Test-only normalizations, applied identically to every layout.
#[derive(Clone, Copy)]
struct Normalize {
    /// Tall CSV has no payer-notes column; the CMS tall examples put
    /// payer-specific notes in `additional_generic_notes`, where wide and
    /// JSON use `additional_payer_notes`. The parsers store each where the
    /// file put it; here both are compared as one `notes` value.
    merge_notes: bool,
    /// JSON `modifier_information` only carries payer/plan + description,
    /// while CSV modifier rows can also carry rate fields. For modifier
    /// items (no codes, modifiers set) compare only payer, plan, and notes.
    modifier_charges_payer_and_notes_only: bool,
    /// v2 JSON `modifier_information` has no `setting` (v3 added it).
    ignore_modifier_setting: bool,
    /// v2 example inconsistencies in raw text: wide payer names use `_` for
    /// spaces, an em dash (cp1252 0x97) vs a hyphen, and a trailing period
    /// on some tall notes.
    v2_text_fixups: bool,
}

const V3: Normalize = Normalize {
    merge_notes: true,
    modifier_charges_payer_and_notes_only: true,
    ignore_modifier_setting: false,
    v2_text_fixups: false,
};

const V2: Normalize = Normalize {
    merge_notes: true,
    modifier_charges_payer_and_notes_only: true,
    ignore_modifier_setting: true,
    v2_text_fixups: true,
};

fn show(v: Option<&str>) -> String {
    v.unwrap_or("∅").to_string()
}

fn fix_text(n: Normalize, s: String) -> String {
    if !n.v2_text_fixups {
        return s;
    }
    s.replace("Platform_Health_Insurance", "Platform Health Insurance")
        .replace("Region_Health_Insurance", "Region Health Insurance")
        .replace('\u{2014}', "-")
}

struct Projected {
    items: Vec<String>,
    codes: Vec<String>,
    charges: Vec<String>,
}

const CHARGE_COLS: &[&str] = &[
    "payer_name_raw",
    "plan_name_raw",
    "gross",
    "discounted_cash",
    "deid_min",
    "deid_max",
    "negotiated_dollar",
    "negotiated_percentage",
    "negotiated_algorithm",
    "methodology",
    "median_amount",
    "p10_amount",
    "p90_amount",
    "allowed_count",
    "estimated_amount",
];

fn project(out: &Path, n: Normalize) -> Projected {
    // item_id -> sorted codes
    let mut codes_by_item: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for c in rows(out, Table::HospitalItemCode) {
        codes_by_item
            .entry(c.get("item_id").unwrap().to_string())
            .or_default()
            .push(format!(
                "{}:{}",
                show(c.get("code")),
                show(c.get("code_type"))
            ));
    }
    for v in codes_by_item.values_mut() {
        v.sort();
    }

    // item_id -> (content key, is_modifier_item)
    let mut keys: BTreeMap<String, (String, bool)> = BTreeMap::new();
    for r in rows(out, Table::HospitalItem) {
        let id = r.get("item_id").unwrap().to_string();
        let codes = codes_by_item.get(&id).cloned().unwrap_or_default();
        let is_modifier = codes.is_empty() && r.get("modifiers").is_some();
        let setting = if is_modifier && n.ignore_modifier_setting {
            "-".to_string()
        } else {
            show(r.get("setting"))
        };
        let key = format!(
            "{} | setting={} | unit={} {} | mod={} | codes=[{}]",
            show(r.get("description")),
            setting,
            show(r.get("drug_unit_of_measurement")),
            show(r.get("drug_type_of_measurement")),
            show(r.get("modifiers")),
            codes.join(",")
        );
        keys.insert(id, (fix_text(n, key), is_modifier));
    }

    let mut items: Vec<String> = keys.values().map(|(k, _)| k.clone()).collect();
    let mut codes: Vec<String> = Vec::new();
    for (id, cs) in &codes_by_item {
        for c in cs {
            codes.push(format!("{} :: {c}", keys[id].0));
        }
    }
    let mut charges: Vec<String> = rows(out, Table::HospitalCharge)
        .iter()
        .map(|c| {
            let (key, is_modifier) = &keys[c.get("item_id").unwrap()];
            let cols: &[&str] = if *is_modifier && n.modifier_charges_payer_and_notes_only {
                &CHARGE_COLS[..2]
            } else {
                CHARGE_COLS
            };
            let mut fields: Vec<String> = cols
                .iter()
                .map(|col| format!("{col}={}", show(c.get(col))))
                .collect();
            if n.merge_notes {
                let mut notes = show(
                    c.get("additional_payer_notes")
                        .or(c.get("additional_generic_notes")),
                );
                if n.v2_text_fixups {
                    notes = notes.trim_end_matches('.').to_string();
                }
                fields.push(format!("notes={notes}"));
            } else {
                fields.push(format!("pn={}", show(c.get("additional_payer_notes"))));
                fields.push(format!("gn={}", show(c.get("additional_generic_notes"))));
            }
            fix_text(n, format!("{key} :: {}", fields.join(" ")))
        })
        .collect();
    items.sort();
    codes.sort();
    charges.sort();
    Projected {
        items,
        codes,
        charges,
    }
}

/// Multiset difference, rendered as `table: only in <name>: <row prefix>`.
/// Rows are truncated at the first `::` + 60 chars so expected lists stay
/// readable while still pinning which item and payer differ.
fn diff(table: &str, an: &str, a: &[String], bn: &str, b: &[String]) -> BTreeSet<String> {
    fn counts(v: &[String]) -> BTreeMap<&String, usize> {
        let mut m = BTreeMap::new();
        for x in v {
            *m.entry(x).or_insert(0) += 1;
        }
        m
    }
    let (ca, cb) = (counts(a), counts(b));
    let mut out = BTreeSet::new();
    for (x, &na) in &ca {
        let nb = cb.get(x).copied().unwrap_or(0);
        if na > nb {
            out.insert(format!("{table}: only in {an}: {}", short(x)));
        }
    }
    for (x, &nb) in &cb {
        let na = ca.get(x).copied().unwrap_or(0);
        if nb > na {
            out.insert(format!("{table}: only in {bn}: {}", short(x)));
        }
    }
    out
}

fn short(row: &str) -> String {
    let (item, rest) = row.split_once(" :: ").unwrap_or((row, ""));
    let desc: String = item.split(" | ").next().unwrap().chars().take(40).collect();
    let payer = rest
        .split(' ')
        .find_map(|f| f.strip_prefix("plan_name_raw="))
        .map(|p| format!(" [{p}]"))
        .unwrap_or_default();
    format!("{desc}{payer}")
}

fn compare(version: &str, n: Normalize) -> BTreeSet<String> {
    let parsed: Vec<(String, Projected)> = ["tall.csv", "wide.csv", "example.json"]
        .iter()
        .map(|f| {
            let d = tempfile::tempdir().unwrap();
            parse(&format!("{version}/{f}"), d.path());
            (f.to_string(), project(d.path(), n))
        })
        .collect();
    let mut all = BTreeSet::new();
    let (an, a) = &parsed[0];
    for (bn, b) in &parsed[1..] {
        all.extend(diff("item", an, &a.items, bn, &b.items));
        all.extend(diff("code", an, &a.codes, bn, &b.codes));
        all.extend(diff("charge", an, &a.charges, bn, &b.charges));
    }
    // wide vs json, so every pair is covered.
    let (wn, w) = &parsed[1];
    let (jn, j) = &parsed[2];
    all.extend(diff("item", wn, &w.items, jn, &j.items));
    all.extend(diff("code", wn, &w.codes, jn, &j.codes));
    all.extend(diff("charge", wn, &w.charges, jn, &j.charges));
    all
}

fn assert_only_known(version: &str, got: BTreeSet<String>, known: &[&str]) {
    let known: BTreeSet<String> = known.iter().map(|s| s.to_string()).collect();
    let unexpected: Vec<_> = got.difference(&known).collect();
    let missing: Vec<_> = known.difference(&got).collect();
    assert!(
        unexpected.is_empty() && missing.is_empty(),
        "{version} equivalence:\n  unexpected differences:\n    {}\n  known differences no longer present:\n    {}",
        unexpected.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n    "),
        missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n    "),
    );
}

#[test]
fn v3_tall_wide_json_are_equivalent() {
    let got = compare("v3", V3);
    assert_only_known(
        "v3",
        got,
        &[
            // CMS v3 tall example lists `standard_charge|min` = 150 on the
            // PPO row of "Basic metabolic panel" but 125 on the HMO row; the
            // wide and JSON examples (one row / one object) say 125.
            "charge: only in tall.csv: Basic metabolic panel [PPO]",
            "charge: only in wide.csv: Basic metabolic panel [PPO]",
            "charge: only in example.json: Basic metabolic panel [PPO]",
        ],
    );
}

#[test]
fn v2_tall_wide_json_are_equivalent() {
    let got = compare("v2", V2);
    assert_only_known(
        "v2",
        got,
        &[
            // Behavioral health per diem: v2 tall and wide split the HMO
            // rates into three items ("..., days 1-3" / "4-5" / "6+"), each
            // with its own min/max; v2 JSON keeps one item (min 1200, max
            // 2000) and puts the day ranges in the HMO payer notes.
            "item: only in tall.csv: Behavioral health; residential (hospital",
            "item: only in wide.csv: Behavioral health; residential (hospital",
            "code: only in tall.csv: Behavioral health; residential (hospital",
            "code: only in wide.csv: Behavioral health; residential (hospital",
            "charge: only in tall.csv: Behavioral health; residential (hospital [PPO]",
            "charge: only in wide.csv: Behavioral health; residential (hospital [PPO]",
            "charge: only in example.json: Behavioral health; residential (hospital [PPO]",
            "charge: only in tall.csv: Behavioral health; residential (hospital [HMO]",
            "charge: only in wide.csv: Behavioral health; residential (hospital [HMO]",
            "charge: only in example.json: Behavioral health; residential (hospital [HMO]",
            // Hip replacement: the PPO rate has three algorithms. v2 wide
            // repeats the whole row three times (so its HMO rate appears 3x)
            // and words the third algorithm differently from tall/JSON.
            "charge: only in wide.csv: Major hip and knee joint replacement or  [HMO]",
            "charge: only in wide.csv: Major hip and knee joint replacement or  [PPO]",
            "charge: only in tall.csv: Major hip and knee joint replacement or  [PPO]",
            "charge: only in example.json: Major hip and knee joint replacement or  [PPO]",
        ],
    );
}
