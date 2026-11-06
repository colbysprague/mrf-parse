//! The hospital output model shared by all three readers, and the sink that
//! writes it to the four hospital tables.
//!
//! Readers turn their input into `FileMeta`, `Item`, and `Charge` values;
//! `HospitalSink` assigns stable item ids, de-duplicates items, applies the
//! code filter, and writes rows.

use std::collections::HashSet;
use std::path::Path;

use mrf_core::{stable_id, Cell, SinkError, StagedInput, Table, TableWriter, ValueFilter};

/// File-level metadata (CSV rows 1–2, JSON top-level keys).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileMeta {
    pub mrf_version: Option<String>,
    pub last_updated_on: Option<String>,
    pub hospital_name: Option<String>,
    pub location_names: Option<Vec<String>>,
    pub hospital_addresses: Option<Vec<String>>,
    pub license_number: Option<String>,
    pub license_state: Option<String>,
    pub type_2_npis: Option<Vec<String>>,
    pub attester_name: Option<String>,
    pub attestation: Option<bool>,
    pub general_contract_provisions: Option<String>,
    pub financial_aid_policy: Option<String>,
    /// Metadata columns/keys not in the CMS spec (JSON object text).
    pub extra: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Code {
    pub code: Option<String>,
    pub code_type: Option<String>,
}

/// A service line: description + codes + setting + drug/modifier details.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Item {
    pub description: Option<String>,
    pub setting: Option<String>,
    pub drug_unit_of_measurement: Option<f64>,
    pub drug_type_of_measurement: Option<String>,
    pub modifiers: Option<String>,
    pub billing_class: Option<String>,
    pub codes: Vec<Code>,
    /// Item-level keys not in the CMS spec (JSON only; JSON object text).
    pub extra: Option<String>,
}

impl Item {
    /// Deterministic id: hash of the file id and every field that defines
    /// the item. Same item text in the same file → same id, so tall rows for
    /// one item (one row per payer/plan) all point at one `hospital_item`.
    pub fn id(&self, file_id: &str) -> String {
        let unit = self.drug_unit_of_measurement.map(|u| u.to_string());
        let mut fields: Vec<Option<&str>> = vec![
            Some(file_id),
            self.description.as_deref(),
            self.setting.as_deref(),
            unit.as_deref(),
            self.drug_type_of_measurement.as_deref(),
            self.modifiers.as_deref(),
            self.billing_class.as_deref(),
            self.extra.as_deref(),
        ];
        for c in &self.codes {
            fields.push(c.code.as_deref());
            fields.push(c.code_type.as_deref());
        }
        stable_id(fields)
    }

    pub fn is_empty(&self) -> bool {
        self.description.is_none() && self.codes.is_empty() && self.modifiers.is_none()
    }
}

/// One `hospital_charge` row: item-level charges plus (optionally) one
/// payer/plan's negotiated terms.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Charge {
    pub payer_name_raw: Option<String>,
    pub plan_name_raw: Option<String>,
    pub gross: Option<f64>,
    pub discounted_cash: Option<f64>,
    pub deid_min: Option<f64>,
    pub deid_max: Option<f64>,
    pub negotiated_dollar: Option<f64>,
    pub negotiated_percentage: Option<f64>,
    pub negotiated_algorithm: Option<String>,
    pub methodology: Option<String>,
    pub median_amount: Option<f64>,
    pub p10_amount: Option<f64>,
    pub p90_amount: Option<f64>,
    pub allowed_count: Option<String>,
    pub estimated_amount: Option<f64>,
    pub additional_payer_notes: Option<String>,
    pub additional_generic_notes: Option<String>,
    /// Columns/keys not in the CMS spec for this row (JSON object text).
    pub extra: Option<String>,
}

impl Charge {
    /// True if any payer/plan-specific field has a value.
    pub fn has_payer_terms(&self) -> bool {
        self.negotiated_dollar.is_some()
            || self.negotiated_percentage.is_some()
            || self.negotiated_algorithm.is_some()
            || self.methodology.is_some()
            || self.median_amount.is_some()
            || self.p10_amount.is_some()
            || self.p90_amount.is_some()
            || self.allowed_count.is_some()
            || self.estimated_amount.is_some()
            || self.additional_payer_notes.is_some()
    }
}

/// Which reader produced the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    CsvTall,
    CsvWide,
    Json,
}

impl Layout {
    pub fn as_str(self) -> &'static str {
        match self {
            Layout::CsvTall => "csv_tall",
            Layout::CsvWide => "csv_wide",
            Layout::Json => "json",
        }
    }
}

/// Row counts per table for one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HospitalCounts {
    pub items: u64,
    pub item_codes: u64,
    pub charges: u64,
    pub rejects: u64,
}

/// Writes the item/code/charge tables for one file. `hospital_file` is
/// written by `finish`, once the metadata is known (JSON metadata may come
/// after the items).
pub struct HospitalSink {
    file_id: String,
    items: TableWriter,
    codes: TableWriter,
    charges: TableWriter,
    /// Ids of items already written. Bounded by distinct items, not rows;
    /// 8 bytes each (a few MB even for very large chargemasters).
    seen: HashSet<u64>,
    code_filter: ValueFilter,
    filtered_out: u64,
}

impl HospitalSink {
    pub fn create(
        out: &Path,
        file_id: &str,
        code_filter: ValueFilter,
    ) -> Result<HospitalSink, SinkError> {
        Ok(HospitalSink {
            file_id: file_id.to_string(),
            items: TableWriter::create(out, Table::HospitalItem, file_id)?,
            codes: TableWriter::create(out, Table::HospitalItemCode, file_id)?,
            charges: TableWriter::create(out, Table::HospitalCharge, file_id)?,
            seen: HashSet::new(),
            code_filter,
            filtered_out: 0,
        })
    }

    pub fn file_id(&self) -> &str {
        &self.file_id
    }

    /// Items skipped by the code filter.
    pub fn filtered_out(&self) -> u64 {
        self.filtered_out
    }

    /// Write `item` (once per distinct item) and its `charges`. Returns
    /// `false` if the code filter excluded the item, in which case nothing
    /// is written.
    pub fn write(&mut self, item: &Item, charges: &[Charge]) -> Result<bool, SinkError> {
        if !self
            .code_filter
            .matches_any(item.codes.iter().filter_map(|c| c.code.as_deref()))
        {
            self.filtered_out += 1;
            return Ok(false);
        }
        let item_id = item.id(&self.file_id);
        let key = u64::from_str_radix(&item_id, 16).expect("stable_id is 16 hex chars");
        if self.seen.insert(key) {
            self.write_item(&item_id, item)?;
        }
        for c in charges {
            self.write_charge(&item_id, c)?;
        }
        Ok(true)
    }

    fn write_item(&mut self, item_id: &str, item: &Item) -> Result<(), SinkError> {
        self.items.append(&[
            Cell::Str(Some(item_id)),
            Cell::Str(Some(&self.file_id)),
            Cell::s(&item.description),
            Cell::s(&item.setting),
            Cell::F64(item.drug_unit_of_measurement),
            Cell::s(&item.drug_type_of_measurement),
            Cell::s(&item.modifiers),
            Cell::s(&item.billing_class),
            Cell::s(&item.extra),
        ])?;
        for c in &item.codes {
            self.codes.append(&[
                Cell::Str(Some(item_id)),
                Cell::s(&c.code),
                Cell::s(&c.code_type),
            ])?;
        }
        Ok(())
    }

    fn write_charge(&mut self, item_id: &str, c: &Charge) -> Result<(), SinkError> {
        self.charges.append(&[
            Cell::Str(Some(item_id)),
            Cell::s(&c.payer_name_raw),
            Cell::s(&c.plan_name_raw),
            Cell::F64(c.gross),
            Cell::F64(c.discounted_cash),
            Cell::F64(c.deid_min),
            Cell::F64(c.deid_max),
            Cell::F64(c.negotiated_dollar),
            Cell::F64(c.negotiated_percentage),
            Cell::s(&c.negotiated_algorithm),
            Cell::s(&c.methodology),
            Cell::F64(c.median_amount),
            Cell::F64(c.p10_amount),
            Cell::F64(c.p90_amount),
            Cell::s(&c.allowed_count),
            Cell::F64(c.estimated_amount),
            Cell::s(&c.additional_payer_notes),
            Cell::s(&c.additional_generic_notes),
            Cell::s(&c.extra),
        ])
    }

    /// Close the item tables and write the single `hospital_file` row.
    pub fn finish(
        self,
        out: &Path,
        input: &StagedInput,
        layout: Layout,
        meta: &FileMeta,
    ) -> Result<HospitalCounts, SinkError> {
        let mut f = TableWriter::create(out, Table::HospitalFile, &self.file_id)?;
        let uri = input.uri.clone();
        f.append(&[
            Cell::Str(Some(&self.file_id)),
            Cell::Str(Some(&uri)),
            Cell::Str(Some(&input.sha256)),
            Cell::Str(Some(layout.as_str())),
            Cell::s(&meta.mrf_version),
            Cell::s(&meta.last_updated_on),
            Cell::s(&meta.hospital_name),
            Cell::StrList(meta.location_names.as_deref()),
            Cell::StrList(meta.hospital_addresses.as_deref()),
            Cell::s(&meta.license_number),
            Cell::s(&meta.license_state),
            Cell::StrList(meta.type_2_npis.as_deref()),
            Cell::s(&meta.attester_name),
            Cell::Bool(meta.attestation),
            Cell::s(&meta.general_contract_provisions),
            Cell::s(&meta.financial_aid_policy),
            Cell::s(&meta.extra),
        ])?;
        f.finish()?;
        let codes_rows = self.codes.rows();
        Ok(HospitalCounts {
            items: self.items.finish()?,
            item_codes: {
                self.codes.finish()?;
                codes_rows
            },
            charges: self.charges.finish()?,
            rejects: 0,
        })
    }
}

/// Builder for an `extra` column: fields that are not in the CMS spec,
/// kept so nothing in the file is lost. Keys keep file order; a repeated
/// key gets a `#2`, `#3`, ... suffix instead of overwriting. Null and empty
/// values are skipped.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extras(Vec<(String, serde_json::Value)>);

impl Extras {
    pub fn push(&mut self, key: &str, v: serde_json::Value) {
        use serde_json::Value;
        match &v {
            Value::Null => return,
            Value::String(s) if s.trim().is_empty() => return,
            _ => {}
        }
        let mut k = key.to_string();
        let mut n = 1;
        while self.0.iter().any(|(e, _)| *e == k) {
            n += 1;
            k = format!("{key}#{n}");
        }
        self.0.push((k, v));
    }

    /// Push a CSV cell (as a trimmed JSON string).
    pub fn push_text(&mut self, key: &str, raw: &str) {
        self.push(key, serde_json::Value::String(raw.trim().to_string()));
    }

    pub fn extend(&mut self, other: &Extras) {
        for (k, v) in &other.0 {
            self.push(k, v.clone());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// JSON object text, or `None` when empty. Serialized by hand so key
    /// order is file order regardless of serde_json's map features.
    pub fn to_json(&self) -> Option<String> {
        if self.0.is_empty() {
            return None;
        }
        let mut s = String::from("{");
        for (i, (k, v)) in self.0.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&serde_json::Value::String(k.clone()).to_string());
            s.push(':');
            s.push_str(&v.to_string());
        }
        s.push('}');
        Some(s)
    }
}

/// Parse a money/numeric cell.
///
/// * empty / whitespace → `Ok(None)`
/// * `$` and `,` are stripped (`$1,234.50` → 1234.5); a trailing `%` is
///   allowed for percentages
/// * anything else unparseable → `Err` so the caller can reject the field
///   (the row is kept with the field null). Never coerced to 0.
pub fn parse_money(raw: &str) -> Result<Option<f64>, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let cleaned: String = t
        .chars()
        .filter(|c| !matches!(c, '$' | ',') && !c.is_whitespace())
        .collect();
    let cleaned = cleaned.strip_suffix('%').unwrap_or(&cleaned);
    match cleaned.parse::<f64>() {
        Ok(v) if v.is_finite() => Ok(Some(v)),
        _ => Err(format!("not a number: {t:?}")),
    }
}

/// Trimmed, non-empty text or `None`.
pub fn text(raw: &str) -> Option<String> {
    let t = raw.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Split a `|`-separated multi-value cell; trims parts and drops empties.
/// Returns `None` for an empty cell.
pub fn split_multi(raw: &str) -> Option<Vec<String>> {
    let parts: Vec<String> = raw
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    (!parts.is_empty()).then_some(parts)
}

/// Parse a boolean cell (`true`/`false`, case-insensitive; also `yes`/`no`,
/// `y`/`n`, `1`/`0`). Empty → `Ok(None)`.
pub fn parse_bool(raw: &str) -> Result<Option<bool>, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => Ok(None),
        "true" | "yes" | "y" | "1" => Ok(Some(true)),
        "false" | "no" | "n" | "0" => Ok(Some(false)),
        _ => Err(format!("not a boolean: {:?}", raw.trim())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money() {
        assert_eq!(parse_money("$1,234.50"), Ok(Some(1234.5)));
        assert_eq!(parse_money(" 400 "), Ok(Some(400.0)));
        assert_eq!(parse_money("0.75"), Ok(Some(0.75)));
        assert_eq!(parse_money("80%"), Ok(Some(80.0)));
        assert_eq!(parse_money(""), Ok(None));
        assert_eq!(parse_money("   "), Ok(None));
        assert!(parse_money("N/A").is_err());
        assert!(parse_money("call for price").is_err());
        assert!(parse_money("inf").is_err());
    }

    #[test]
    fn multi_and_bool() {
        assert_eq!(
            split_multi("0000000001 | 0000000002 | 0000000003  "),
            Some(vec![
                "0000000001".into(),
                "0000000002".into(),
                "0000000003".into()
            ])
        );
        assert_eq!(split_multi(" "), None);
        assert_eq!(parse_bool("TRUE"), Ok(Some(true)));
        assert_eq!(parse_bool(""), Ok(None));
        assert!(parse_bool("maybe").is_err());
    }

    #[test]
    fn extras_keep_order_and_duplicates() {
        let mut e = Extras::default();
        assert_eq!(e.to_json(), None);
        e.push_text("zeta", " 1 ");
        e.push_text("alpha", "");
        e.push_text("footnote", "a");
        e.push_text("footnote", "b");
        e.push("n", serde_json::json!(5));
        assert_eq!(
            e.to_json().unwrap(),
            r#"{"zeta":"1","footnote":"a","footnote#2":"b","n":5}"#
        );
    }

    #[test]
    fn item_id_depends_on_content() {
        let a = Item {
            description: Some("MRI".into()),
            codes: vec![Code {
                code: Some("70551".into()),
                code_type: Some("CPT".into()),
            }],
            ..Default::default()
        };
        let mut b = a.clone();
        assert_eq!(a.id("f"), b.id("f"));
        assert_ne!(a.id("f"), a.id("g"));
        b.setting = Some("outpatient".into());
        assert_ne!(a.id("f"), b.id("f"));
    }
}
