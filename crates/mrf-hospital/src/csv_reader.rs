//! Hospital CSV reader for both the tall and wide layouts.
//!
//! CMS CSV structure:
//! * row 1: metadata headers (`hospital_name`, `license_number|CA`, ...)
//! * row 2: metadata values
//! * row 3: data headers
//! * row 4+: one row per item (wide) or per item × payer × plan (tall)
//!
//! The two layouts share everything except how charges come out of a row:
//! tall rows carry `payer_name`/`plan_name` columns and become one charge
//! each; wide rows carry payer/plan in the column headers and are unpivoted
//! into one charge per payer/plan that has any value.
//!
//! Rows are streamed one at a time with a reused `ByteRecord`, so memory
//! does not grow with file size.

use std::collections::BTreeMap;
use std::io::Read;

use csv::{ByteRecord, StringRecord};
use mrf_core::RejectSink;

use crate::header::{map_meta_header, norm_token, ChargeField, DataColumn, DataHeader, MetaColumn};
use crate::model::{
    parse_bool, parse_money, split_multi, text, Charge, Code, Extras, FileMeta, HospitalSink, Item,
    Layout,
};
use crate::HospitalError;

/// What a CSV read produced besides the rows it wrote.
#[derive(Debug)]
pub struct CsvResult {
    pub meta: FileMeta,
    pub layout: Layout,
    /// Unknown column headers (metadata and data), for reporting.
    pub unknown_columns: Vec<String>,
    /// Major CMS version implied by which headers are present, if any.
    pub shape_major: Option<u32>,
}

/// Read a hospital CSV, writing items/codes/charges to `sink` and bad
/// rows/fields to `rejects`.
pub fn read_csv<R: Read>(
    input: R,
    sink: &mut HospitalSink,
    rejects: &mut RejectSink,
) -> Result<CsvResult, HospitalError> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        // Rows may have more or fewer cells than the header; we check that
        // ourselves so one bad row does not abort the file.
        .flexible(true)
        .from_reader(input);

    let mut raw = ByteRecord::new();
    let mut row_no: u64 = 0; // 1-based CSV row (record) number
    let mut next = |raw: &mut ByteRecord, row_no: &mut u64| -> Result<bool, HospitalError> {
        let more = rdr.read_byte_record(raw)?;
        if more {
            *row_no += 1;
        }
        Ok(more)
    };

    // Rows 1–3.
    let mut head_rows: Vec<StringRecord> = Vec::with_capacity(3);
    while head_rows.len() < 3 && next(&mut raw, &mut row_no)? {
        head_rows.push(to_strings(&raw, row_no, rejects)?);
    }
    if head_rows.len() < 3 {
        return Err(HospitalError::UnknownLayout(format!(
            "CSV has {} rows; expected metadata header, metadata values, and data header",
            head_rows.len()
        )));
    }

    let mut unknown_columns = Vec::new();
    let meta = parse_meta(&head_rows[0], &head_rows[1], rejects, &mut unknown_columns)?;
    let header = DataHeader::parse(&head_rows[2].iter().collect::<Vec<_>>());
    unknown_columns.extend(header.unknown().map(String::from));
    for u in header.unknown() {
        tracing::warn!(
            column = u,
            "column not in the CMS spec (kept in hospital_charge.extra)"
        );
    }

    let layout = detect_csv_layout(&header)?;
    let shape_major = crate::shape_major(
        head_rows[0]
            .iter()
            .chain(head_rows[2].iter())
            .map(|h| norm_token(crate::header::split_header(h)[0])),
    );
    let plan = ColumnPlan::new(&header);

    // Data rows.
    while next(&mut raw, &mut row_no)? {
        let rec = to_strings(&raw, row_no, rejects)?;
        if rec.iter().all(|c| c.trim().is_empty()) {
            continue; // blank line / padding row
        }
        let loc = format!("row {row_no}");
        if rec.len() > header.columns.len()
            && rec
                .iter()
                .skip(header.columns.len())
                .any(|c| !c.trim().is_empty())
        {
            rejects.reject(
                &loc,
                &format!(
                    "row has {} cells but header has {}",
                    rec.len(),
                    header.columns.len()
                ),
                Some(&join_record(&rec)),
            )?;
            continue;
        }
        let parsed = plan.parse_row(&rec, layout, &header, row_no, rejects)?;
        match parsed {
            Some((item, charges)) => {
                sink.write(&item, &charges)?;
            }
            None => {
                rejects.reject(
                    &loc,
                    "row has no description, code, or modifiers",
                    Some(&join_record(&rec)),
                )?;
            }
        }
    }

    Ok(CsvResult {
        meta,
        layout,
        unknown_columns,
        shape_major,
    })
}

/// Tall if there is a `payer_name` column, wide if any header carries a
/// payer/plan. A file with only item-level charge columns (no payer data at
/// all) is valid in either layout; we call it wide. Anything without a
/// description or code column is not a hospital data header.
pub fn detect_csv_layout(header: &DataHeader) -> Result<Layout, HospitalError> {
    let has_item_cols = header
        .columns
        .iter()
        .any(|c| matches!(c, DataColumn::Description | DataColumn::Code(_)));
    if !has_item_cols {
        return Err(HospitalError::UnknownLayout(
            "row 3 has no `description` or `code|n` column".into(),
        ));
    }
    match (header.is_tall(), header.is_wide()) {
        (true, true) => {
            tracing::warn!("CSV has both payer_name and payer-specific headers; reading as tall");
            Ok(Layout::CsvTall)
        }
        (true, false) => Ok(Layout::CsvTall),
        _ => Ok(Layout::CsvWide),
    }
}

/// Convert a byte record to strings. Rows that are not valid UTF-8 are
/// decoded as Windows-1252 instead (the usual culprit: hospital exports from
/// Excel; even the CMS v2 wide example contains a cp1252 em dash). The row
/// is kept and a reject records that a fallback decoding was used.
fn to_strings(
    raw: &ByteRecord,
    row_no: u64,
    rejects: &mut RejectSink,
) -> Result<StringRecord, HospitalError> {
    match StringRecord::from_byte_record(raw.clone()) {
        Ok(r) => Ok(r),
        Err(e) => {
            let bytes = e.into_byte_record();
            let rec: StringRecord = bytes
                .iter()
                .map(|f| match std::str::from_utf8(f) {
                    Ok(s) => s.to_string(),
                    Err(_) => mrf_core::text::decode_cp1252(f),
                })
                .collect();
            rejects.reject(
                &format!("row {row_no}"),
                "invalid UTF-8; decoded as Windows-1252 (row kept)",
                Some(&join_record(&rec)),
            )?;
            Ok(rec)
        }
    }
}

fn join_record(rec: &StringRecord) -> String {
    rec.iter().collect::<Vec<_>>().join(",")
}

fn parse_meta(
    names: &StringRecord,
    values: &StringRecord,
    rejects: &mut RejectSink,
    unknown: &mut Vec<String>,
) -> Result<FileMeta, HospitalError> {
    let mut m = FileMeta::default();
    let mut extra = Extras::default();
    for (i, name) in names.iter().enumerate() {
        let v = values.get(i).unwrap_or("");
        match map_meta_header(name) {
            MetaColumn::HospitalName => m.hospital_name = text(v),
            MetaColumn::LastUpdatedOn => m.last_updated_on = text(v),
            MetaColumn::Version => m.mrf_version = text(v),
            MetaColumn::LocationName => m.location_names = split_multi(v),
            MetaColumn::HospitalAddress => m.hospital_addresses = split_multi(v),
            MetaColumn::LicenseNumber { state } => {
                m.license_number = text(v);
                m.license_state = state;
            }
            MetaColumn::Type2Npi => m.type_2_npis = split_multi(v),
            MetaColumn::AttesterName => m.attester_name = text(v),
            MetaColumn::Attestation => match parse_bool(v) {
                Ok(b) => m.attestation = b,
                Err(reason) => rejects.reject("row 2, attestation", &reason, Some(v))?,
            },
            MetaColumn::GeneralContractProvisions => m.general_contract_provisions = text(v),
            MetaColumn::FinancialAidPolicy => m.financial_aid_policy = text(v),
            MetaColumn::Blank => {
                if !v.trim().is_empty() {
                    rejects.reject(
                        &format!("row 2, column {}", i + 1),
                        "metadata value under a blank header",
                        Some(v),
                    )?;
                }
            }
            MetaColumn::Unknown(h) => {
                tracing::warn!(column = %h, "metadata column not in the CMS spec (kept in hospital_file.extra)");
                extra.push_text(&h, v);
                unknown.push(h);
            }
        }
    }
    m.extra = extra.to_json();
    Ok(m)
}

/// What to do with each column, precomputed from the header so the per-row
/// loop is a simple match.
struct ColumnPlan {
    /// For wide payer columns: index into `header.payer_plans`.
    payer_idx: Vec<Option<usize>>,
}

impl ColumnPlan {
    fn new(header: &DataHeader) -> ColumnPlan {
        let payer_idx = header
            .columns
            .iter()
            .map(|c| match c {
                DataColumn::PayerCharge { payer, plan, .. } => header
                    .payer_plans
                    .iter()
                    .position(|(p, q)| p == payer && q == plan),
                _ => None,
            })
            .collect();
        ColumnPlan { payer_idx }
    }

    /// Parse one data row. Returns `None` if the row has nothing that
    /// identifies an item.
    fn parse_row(
        &self,
        rec: &StringRecord,
        layout: Layout,
        header: &DataHeader,
        row_no: u64,
        rejects: &mut RejectSink,
    ) -> Result<Option<(Item, Vec<Charge>)>, HospitalError> {
        let mut item = Item::default();
        let mut codes: BTreeMap<u32, Code> = BTreeMap::new();
        let mut base = Charge::default();
        // Cells under columns that are not in the CMS spec. Row-level, so
        // they go on every charge this row produces.
        let mut extra = Extras::default();
        // One charge per wide payer/plan; filled only for wide files.
        let mut payer_charges: Vec<Charge> = if layout == Layout::CsvWide {
            header
                .payer_plans
                .iter()
                .map(|(p, q)| Charge {
                    payer_name_raw: Some(p.clone()),
                    plan_name_raw: q.clone(),
                    ..Default::default()
                })
                .collect()
        } else {
            Vec::new()
        };

        for (i, col) in header.columns.iter().enumerate() {
            let cell = rec.get(i).unwrap_or("");
            if cell.trim().is_empty() {
                continue;
            }
            match col {
                DataColumn::Description => item.description = text(cell),
                DataColumn::Setting => item.setting = text(cell),
                DataColumn::DrugUnitOfMeasurement => {
                    item.drug_unit_of_measurement =
                        money_field(cell, row_no, &rec_header(header, i), rejects)?
                }
                DataColumn::DrugTypeOfMeasurement => item.drug_type_of_measurement = text(cell),
                DataColumn::Modifiers => item.modifiers = text(cell),
                DataColumn::BillingClass => item.billing_class = text(cell),
                DataColumn::AdditionalGenericNotes => base.additional_generic_notes = text(cell),
                DataColumn::Code(n) => codes.entry(*n).or_default().code = text(cell),
                DataColumn::CodeType(n) => codes.entry(*n).or_default().code_type = text(cell),
                DataColumn::PayerName => base.payer_name_raw = text(cell),
                DataColumn::PlanName => base.plan_name_raw = text(cell),
                DataColumn::Charge(f) => {
                    set_field(&mut base, *f, cell, row_no, &rec_header(header, i), rejects)?
                }
                DataColumn::PayerCharge { field, .. } => {
                    if let Some(idx) = self.payer_idx[i] {
                        if let Some(c) = payer_charges.get_mut(idx) {
                            set_field(c, *field, cell, row_no, &rec_header(header, i), rejects)?;
                        }
                    }
                }
                DataColumn::Unknown(name) => extra.push_text(name, cell),
            }
        }

        base.extra = extra.to_json();

        // Codes in `n` order; skip slots where both code and type are empty.
        item.codes = codes
            .into_values()
            .filter(|c| c.code.is_some() || c.code_type.is_some())
            .collect();
        if item.is_empty() {
            return Ok(None);
        }

        let charges = match layout {
            Layout::CsvTall | Layout::Json => vec![base],
            Layout::CsvWide => {
                // Unpivot: copy item-level charges onto each payer/plan that
                // has at least one value.
                let mut out: Vec<Charge> = payer_charges
                    .into_iter()
                    .filter(Charge::has_payer_terms)
                    .map(|mut c| {
                        c.gross = base.gross;
                        c.discounted_cash = base.discounted_cash;
                        c.deid_min = base.deid_min;
                        c.deid_max = base.deid_max;
                        c.additional_generic_notes = base.additional_generic_notes.clone();
                        c.extra = base.extra.clone();
                        c
                    })
                    .collect();
                if out.is_empty() {
                    out.push(base); // no payer rates: one null-payer row
                }
                out
            }
        };
        Ok(Some((item, charges)))
    }
}

fn rec_header(header: &DataHeader, i: usize) -> String {
    match &header.columns[i] {
        DataColumn::PayerCharge {
            payer, plan, field, ..
        } => format!(
            "{field}|{payer}{}",
            plan.as_deref().map(|p| format!("|{p}")).unwrap_or_default()
        ),
        other => format!("{other:?}"),
    }
}

fn money_field(
    cell: &str,
    row_no: u64,
    column: &str,
    rejects: &mut RejectSink,
) -> Result<Option<f64>, HospitalError> {
    match parse_money(cell) {
        Ok(v) => Ok(v),
        Err(reason) => {
            rejects.reject(
                &format!("row {row_no}, column {column}"),
                &format!("{reason} (field set to null, row kept)"),
                Some(cell),
            )?;
            Ok(None)
        }
    }
}

/// Store one charge field on `c`, parsing numbers where the field is numeric.
pub(crate) fn set_field(
    c: &mut Charge,
    field: ChargeField,
    cell: &str,
    row_no: u64,
    column: &str,
    rejects: &mut RejectSink,
) -> Result<(), HospitalError> {
    let mut num = || money_field(cell, row_no, column, rejects);
    match field {
        ChargeField::Gross => c.gross = num()?,
        ChargeField::DiscountedCash => c.discounted_cash = num()?,
        ChargeField::Min => c.deid_min = num()?,
        ChargeField::Max => c.deid_max = num()?,
        ChargeField::NegotiatedDollar => c.negotiated_dollar = num()?,
        ChargeField::NegotiatedPercentage => c.negotiated_percentage = num()?,
        ChargeField::Median => c.median_amount = num()?,
        ChargeField::P10 => c.p10_amount = num()?,
        ChargeField::P90 => c.p90_amount = num()?,
        ChargeField::EstimatedAmount => c.estimated_amount = num()?,
        ChargeField::NegotiatedAlgorithm => c.negotiated_algorithm = text(cell),
        ChargeField::Methodology => c.methodology = text(cell),
        ChargeField::Count => c.allowed_count = text(cell),
        ChargeField::AdditionalPayerNotes => c.additional_payer_notes = text(cell),
    }
    Ok(())
}
