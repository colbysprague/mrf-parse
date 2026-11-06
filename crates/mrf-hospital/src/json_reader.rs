//! Streaming hospital JSON reader (v2.x and v3.0).
//!
//! # How the streaming works
//!
//! `serde_json` normally deserializes a whole document into one value. To
//! keep memory flat we instead drive the deserializer with our own
//! [`Visitor`]s:
//!
//! * `TopVisitor` walks the top-level object key by key (`visit_map`).
//!   Small metadata keys are read as `serde_json::Value`s.
//! * For `standard_charge_information`, it hands the deserializer an
//!   `ItemsSeed`, whose `visit_seq` pulls **one array element at a time**
//!   (`next_element::<Value>()`), converts it to rows, writes them, and drops
//!   it before reading the next. Only one service line is ever in memory.
//!
//! Because metadata keys are visited in file order, it does not matter
//! whether they appear before or after the big array.
//!
//! Each element is read as a generic `Value` (not a typed struct) so that a
//! semantically bad element (wrong type, missing field) becomes a reject
//! with its JSON path instead of a deserialization error that would end the
//! stream. Syntax errors (truncated file, bad token) cannot be skipped in a
//! streaming parser and fail the file.

use std::fmt;
use std::io::Read;

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use mrf_core::RejectSink;

use crate::model::{
    parse_money, split_multi, text, Charge, Code, Extras, FileMeta, HospitalSink, Item,
};
use crate::HospitalError;

/// Read a hospital JSON MRF.
/// Returns the metadata and the major version implied by the keys present.
pub fn read_json<R: Read>(
    input: R,
    sink: &mut HospitalSink,
    rejects: &mut RejectSink,
) -> Result<(FileMeta, Option<u32>), HospitalError> {
    let mut state = State {
        sink,
        rejects,
        meta: FileMeta::default(),
        file_extra: Extras::default(),
        fatal: None,
        saw_items: false,
        keys: Default::default(),
    };
    // serde_json reads a byte at a time; buffer so a bare `File` is not one
    // syscall per byte (callers in this crate already pass a BufRead).
    let input = std::io::BufReader::with_capacity(64 * 1024, input);
    let mut de = serde_json::Deserializer::from_reader(input);
    let result = de.deserialize_map(TopVisitor { state: &mut state });
    // An error we raised inside a visitor (I/O, sink) takes precedence over
    // the generic serde error it was smuggled out as.
    if let Some(e) = state.fatal.take() {
        return Err(e);
    }
    result.map_err(|e| HospitalError::Json(e.to_string()))?;
    if let Err(e) = de.end() {
        tracing::warn!(error = %e, "trailing data after top-level JSON object (ignored)");
    }
    if !state.saw_items {
        return Err(HospitalError::UnknownLayout(
            "JSON has no `standard_charge_information` array".into(),
        ));
    }
    state.meta.extra = state.file_extra.to_json();
    let shape = crate::shape_major(state.keys.iter().cloned());
    Ok((state.meta, shape))
}

/// Mutable state shared by the visitors.
struct State<'a> {
    sink: &'a mut HospitalSink,
    rejects: &'a mut RejectSink,
    /// Top-level keys not in the CMS spec → `hospital_file.extra`.
    file_extra: Extras,
    meta: FileMeta,
    /// Set when processing fails for a reason other than JSON syntax.
    fatal: Option<HospitalError>,
    saw_items: bool,
    /// Top-level and payer-level keys seen, for version-shape detection.
    keys: std::collections::BTreeSet<String>,
}

impl State<'_> {
    /// Run `f`; on error, stash it and return a serde error to unwind the
    /// deserializer. (Visitor methods can only return `E: de::Error`.)
    fn guard<E: de::Error>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<(), HospitalError>,
    ) -> Result<(), E> {
        f(self).map_err(|e| {
            self.fatal = Some(e);
            E::custom("aborted")
        })
    }

    fn reject(&mut self, path: &str, reason: &str, v: Option<&Value>) -> Result<(), HospitalError> {
        let snippet = v.map(|v| v.to_string());
        self.rejects.reject(path, reason, snippet.as_deref())?;
        Ok(())
    }
}

// --- top level -------------------------------------------------------------

/// Visits the top-level object. Holds `&mut State` rather than owning it,
/// so the caller can inspect the state after deserialization finishes.
struct TopVisitor<'s, 'a> {
    state: &'s mut State<'a>,
}

impl<'de> Visitor<'de> for TopVisitor<'_, '_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a hospital MRF JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            self.state.keys.insert(key.clone());
            match key.as_str() {
                "standard_charge_information" => {
                    self.state.saw_items = true;
                    map.next_value_seed(ItemsSeed {
                        state: &mut *self.state,
                    })?;
                }
                "modifier_information" => {
                    // Small (one entry per modifier); read whole.
                    let v: Value = map.next_value()?;
                    self.state.guard(|s| modifiers(s, &v))?;
                }
                _ => {
                    let v: Value = map.next_value()?;
                    self.state.guard(|s| meta_key(s, &key, v))?;
                }
            }
        }
        Ok(())
    }
}

fn meta_key(s: &mut State, key: &str, v: Value) -> Result<(), HospitalError> {
    let path = format!("$.{key}");
    let m = &mut s.meta;
    match key {
        "hospital_name" => m.hospital_name = str_of(&v),
        "last_updated_on" => m.last_updated_on = str_of(&v),
        "version" => m.mrf_version = str_of(&v),
        "location_name" | "hospital_location" => m.location_names = str_list(&v),
        "hospital_address" => m.hospital_addresses = str_list(&v),
        "type_2_npi" => m.type_2_npis = str_list(&v),
        "license_information" => {
            m.license_number = v.get("license_number").and_then(str_of);
            m.license_state = v.get("state").and_then(str_of);
        }
        // v3 `attestation`, v2 `affirmation`; same shape.
        "attestation" | "affirmation" => {
            let flag = v
                .get("confirm_attestation")
                .or_else(|| v.get("confirm_affirmation"));
            m.attester_name = v.get("attester_name").and_then(str_of);
            match flag {
                None | Some(Value::Null) => {}
                Some(Value::Bool(b)) => m.attestation = Some(*b),
                Some(Value::String(t)) => match crate::model::parse_bool(t) {
                    Ok(b) => m.attestation = b,
                    Err(reason) => s.reject(&path, &reason, flag)?,
                },
                Some(other) => s.reject(&path, "attestation flag is not a boolean", Some(other))?,
            }
        }
        // An array of `{payer_name, plan_name, provisions}` in JSON; kept as
        // JSON text so no structure is lost. A plain string is kept as-is.
        "general_contract_provisions" => {
            m.general_contract_provisions = match &v {
                Value::Null => None,
                Value::String(t) => text(t),
                other => Some(other.to_string()),
            }
        }
        "financial_aid_policy" => m.financial_aid_policy = str_of(&v),
        _ => {
            tracing::warn!(
                key,
                "top-level key not in the CMS spec (kept in hospital_file.extra)"
            );
            s.file_extra.push(key, v);
        }
    }
    Ok(())
}

// --- standard_charge_information[] ----------------------------------------

/// Seed that streams the `standard_charge_information` array.
struct ItemsSeed<'s, 'a> {
    state: &'s mut State<'a>,
}

impl<'de> DeserializeSeed<'de> for ItemsSeed<'_, '_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for ItemsSeed<'_, '_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("the standard_charge_information array")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut i = 0usize;
        // One element in memory at a time.
        while let Some(v) = seq.next_element::<Value>()? {
            let path = format!("$.standard_charge_information[{i}]");
            self.state.guard(|s| service_line(s, &path, &v))?;
            i += 1;
        }
        Ok(())
    }
}

/// Convert one `standard_charge_information` element to items and charges.
///
/// In JSON, `setting` (and v3 `modifier_code`) live on each
/// `standard_charges[]` entry, so each entry becomes its own item — matching
/// the CSV layouts, where setting is a per-row item column.
fn service_line(s: &mut State, path: &str, v: &Value) -> Result<(), HospitalError> {
    let Some(obj) = v.as_object() else {
        return s.reject(path, "element is not an object", Some(v));
    };
    let description = obj.get("description").and_then(str_of);
    let codes = code_list(s, path, obj)?;
    let (drug_unit, drug_type) = match obj.get("drug_information") {
        Some(d) if d.is_object() => (
            num_field(s, &format!("{path}.drug_information.unit"), d.get("unit"))?,
            d.get("type").and_then(str_of),
        ),
        _ => (None, None),
    };
    if description.is_none() && codes.is_empty() {
        return s.reject(path, "service line has no description or codes", Some(v));
    }

    let item_extra = unknown_keys(obj, ITEM_KEYS).to_json();
    let Some(charges) = obj.get("standard_charges").and_then(Value::as_array) else {
        return s.reject(path, "missing or non-array `standard_charges`", Some(v));
    };
    for (j, sc) in charges.iter().enumerate() {
        let sc_path = format!("{path}.standard_charges[{j}]");
        let Some(sc) = sc.as_object() else {
            s.reject(
                &sc_path,
                "standard_charges entry is not an object",
                Some(sc),
            )?;
            continue;
        };
        let sc_extra = unknown_keys(sc, STANDARD_CHARGE_KEYS);
        let item = Item {
            extra: item_extra.clone(),
            description: description.clone(),
            setting: sc.get("setting").and_then(str_of),
            drug_unit_of_measurement: drug_unit,
            drug_type_of_measurement: drug_type.clone(),
            modifiers: sc.get("modifier_code").and_then(modifier_text),
            billing_class: sc.get("billing_class").and_then(str_of),
            codes: codes.clone(),
        };
        let base = Charge {
            gross: num_field(
                s,
                &format!("{sc_path}.gross_charge"),
                sc.get("gross_charge"),
            )?,
            discounted_cash: num_field(
                s,
                &format!("{sc_path}.discounted_cash"),
                sc.get("discounted_cash"),
            )?,
            deid_min: num_field(s, &format!("{sc_path}.minimum"), sc.get("minimum"))?,
            deid_max: num_field(s, &format!("{sc_path}.maximum"), sc.get("maximum"))?,
            additional_generic_notes: sc.get("additional_generic_notes").and_then(str_of),
            extra: sc_extra.to_json(),
            ..Default::default()
        };
        let mut rows = Vec::new();
        if let Some(payers) = sc.get("payers_information").and_then(Value::as_array) {
            for (k, p) in payers.iter().enumerate() {
                let p_path = format!("{sc_path}.payers_information[{k}]");
                match p.as_object() {
                    Some(p) => rows.push(payer_charge(s, &p_path, p, &base, &sc_extra)?),
                    None => s.reject(&p_path, "payer entry is not an object", Some(p))?,
                }
            }
        }
        if rows.is_empty() {
            rows.push(base); // no payer rates: one null-payer row
        }
        s.sink.write(&item, &rows)?;
    }
    Ok(())
}

fn payer_charge(
    s: &mut State,
    path: &str,
    p: &Map<String, Value>,
    base: &Charge,
    sc_extra: &Extras,
) -> Result<Charge, HospitalError> {
    for k in ["median_amount", "estimated_amount"] {
        if p.contains_key(k) && !s.keys.contains(k) {
            s.keys.insert(k.to_string());
        }
    }
    let num = |s: &mut State, key: &str| num_field(s, &format!("{path}.{key}"), p.get(key));
    // Unknown keys from the standard_charges entry, then from the payer.
    let mut extra = sc_extra.clone();
    extra.extend(&unknown_keys(p, PAYER_KEYS));
    Ok(Charge {
        extra: extra.to_json(),
        payer_name_raw: p.get("payer_name").and_then(str_of),
        plan_name_raw: p.get("plan_name").and_then(str_of),
        negotiated_dollar: num(s, "standard_charge_dollar")?,
        negotiated_percentage: num(s, "standard_charge_percentage")?,
        negotiated_algorithm: p.get("standard_charge_algorithm").and_then(str_of),
        methodology: p.get("methodology").and_then(str_of),
        median_amount: num(s, "median_amount")?,
        p10_amount: num(s, "10th_percentile")?,
        p90_amount: num(s, "90th_percentile")?,
        // Text in the spec ("1 through 10"); a JSON number is kept as text.
        allowed_count: p.get("count").and_then(str_of),
        estimated_amount: num(s, "estimated_amount")?,
        additional_payer_notes: p.get("additional_payer_notes").and_then(str_of),
        ..base.clone()
    })
}

fn code_list(
    s: &mut State,
    path: &str,
    obj: &Map<String, Value>,
) -> Result<Vec<Code>, HospitalError> {
    let mut out = Vec::new();
    match obj.get("code_information") {
        None | Some(Value::Null) => {}
        Some(Value::Array(codes)) => {
            for (k, c) in codes.iter().enumerate() {
                match c.as_object() {
                    Some(c) => {
                        let code = Code {
                            code: c.get("code").and_then(str_of),
                            code_type: c.get("type").and_then(str_of),
                        };
                        if code.code.is_some() || code.code_type.is_some() {
                            out.push(code);
                        }
                    }
                    None => s.reject(
                        &format!("{path}.code_information[{k}]"),
                        "code entry is not an object",
                        Some(c),
                    )?,
                }
            }
        }
        Some(other) => s.reject(
            &format!("{path}.code_information"),
            "code_information is not an array",
            Some(other),
        )?,
    }
    Ok(out)
}

// --- modifier_information[] -----------------------------------------------

/// Each modifier becomes an item with `modifiers` = its code and no billing
/// codes, plus one charge per payer whose notes hold the payer-specific
/// description. This mirrors how the CSV layouts encode modifier rows.
fn modifiers(s: &mut State, v: &Value) -> Result<(), HospitalError> {
    let Some(list) = v.as_array() else {
        return s.reject("$.modifier_information", "not an array", Some(v));
    };
    for (i, m) in list.iter().enumerate() {
        let path = format!("$.modifier_information[{i}]");
        let Some(m) = m.as_object() else {
            s.reject(&path, "modifier entry is not an object", Some(m))?;
            continue;
        };
        let item = Item {
            extra: unknown_keys(m, MODIFIER_KEYS).to_json(),
            description: m.get("description").and_then(str_of),
            setting: m.get("setting").and_then(str_of),
            modifiers: m.get("code").and_then(modifier_text),
            ..Default::default()
        };
        if item.modifiers.is_none() {
            s.reject(
                &path,
                "modifier entry has no code",
                Some(&Value::Object(m.clone())),
            )?;
            continue;
        }
        let mut rows: Vec<Charge> = m
            .get("modifier_payer_information")
            .and_then(Value::as_array)
            .map(|ps| {
                ps.iter()
                    .filter_map(Value::as_object)
                    .map(|p| Charge {
                        payer_name_raw: p.get("payer_name").and_then(str_of),
                        plan_name_raw: p.get("plan_name").and_then(str_of),
                        additional_payer_notes: p.get("description").and_then(str_of),
                        extra: unknown_keys(p, MODIFIER_PAYER_KEYS).to_json(),
                        ..Default::default()
                    })
                    .collect()
            })
            .unwrap_or_default();
        if rows.is_empty() {
            rows.push(Charge::default());
        }
        s.sink.write(&item, &rows)?;
    }
    Ok(())
}

// --- keys outside the spec ---------------------------------------------------

/// Keys each object level maps to columns; anything else goes to `extra`.
const ITEM_KEYS: &[&str] = &[
    "description",
    "code_information",
    "drug_information",
    "standard_charges",
];
const STANDARD_CHARGE_KEYS: &[&str] = &[
    "setting",
    "minimum",
    "maximum",
    "gross_charge",
    "discounted_cash",
    "payers_information",
    "additional_generic_notes",
    "modifier_code",
    "billing_class",
];
const PAYER_KEYS: &[&str] = &[
    "payer_name",
    "plan_name",
    "standard_charge_dollar",
    "standard_charge_percentage",
    "standard_charge_algorithm",
    "methodology",
    "median_amount",
    "10th_percentile",
    "90th_percentile",
    "count",
    "estimated_amount",
    "additional_payer_notes",
];
const MODIFIER_KEYS: &[&str] = &[
    "description",
    "code",
    "setting",
    "modifier_payer_information",
];
const MODIFIER_PAYER_KEYS: &[&str] = &["payer_name", "plan_name", "description"];

fn unknown_keys(obj: &Map<String, Value>, known: &[&str]) -> Extras {
    let mut e = Extras::default();
    for (k, v) in obj {
        if !known.contains(&k.as_str()) {
            e.push(k, v.clone());
        }
    }
    e
}

// --- value helpers --------------------------------------------------------

/// String value, trimmed; numbers and booleans are rendered as text (some
/// files write codes as numbers). Empty strings and null → `None`.
fn str_of(v: &Value) -> Option<String> {
    match v {
        Value::String(t) => text(t),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Array of strings, or a single `|`-separated string.
fn str_list(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(items) => {
            let out: Vec<String> = items.iter().filter_map(str_of).collect();
            (!out.is_empty()).then_some(out)
        }
        Value::String(t) => split_multi(t),
        _ => None,
    }
}

/// Modifier codes may be a string or an array; arrays are joined with `|`,
/// matching the CSV encoding of multiple modifiers.
fn modifier_text(v: &Value) -> Option<String> {
    match v {
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(str_of).collect();
            (!parts.is_empty()).then(|| parts.join("|"))
        }
        other => str_of(other),
    }
}

/// Numeric field: JSON number, or a string parsed like a CSV money cell.
/// Anything else → reject, field null.
fn num_field(s: &mut State, path: &str, v: Option<&Value>) -> Result<Option<f64>, HospitalError> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => match n.as_f64() {
            Some(f) if f.is_finite() => Ok(Some(f)),
            _ => {
                s.reject(path, "number out of range (field set to null)", v)?;
                Ok(None)
            }
        },
        Some(Value::String(t)) => match parse_money(t) {
            Ok(f) => Ok(f),
            Err(reason) => {
                s.reject(path, &format!("{reason} (field set to null)"), v)?;
                Ok(None)
            }
        },
        Some(other) => {
            s.reject(path, "expected a number (field set to null)", Some(other))?;
            Ok(None)
        }
    }
}
