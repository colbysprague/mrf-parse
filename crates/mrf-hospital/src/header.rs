//! Hospital CSV header tokenizer and mapper.
//!
//! CMS CSV headers are `|`-separated token lists: `code | 1 | type`,
//! `standard_charge|Platform Health Insurance|PPO|negotiated_dollar`. The
//! spec's own examples disagree on spacing around `|`, and real files vary
//! case, use `-`/spaces for `_`, and occasionally leak literal quotes.
//!
//! Two kinds of token:
//! * **Keyword tokens** (`standard_charge`, `negotiated_dollar`, `code`, ...)
//!   are normalized: trimmed, lowercased, whitespace/`-` runs → `_`.
//! * **Name tokens** (payer, plan, license state) are only trimmed; their
//!   case and inner spacing are kept raw, because normalizing names is a
//!   later pipeline step.
//!
//! Accepted spellings follow the CMS data dictionary (v2.x and v3.0), plus a
//! few tolerant variants (optional quotes, extra/missing plan segments,
//! `standard_charges` prefix) for files that drift from the spec.

use std::fmt;

/// Strip a UTF-8 BOM, surrounding whitespace, and literal quote characters
/// some publishers bake into header cells (`"standard_charge|gross"`).
pub fn clean_cell(raw: &str) -> &str {
    raw.trim_start_matches('\u{feff}')
        .trim()
        .trim_matches(|c| {
            matches!(
                c,
                '"' | '\'' | '\u{201c}' | '\u{201d}' | '\u{2018}' | '\u{2019}'
            )
        })
        .trim()
}

/// Split a header on `|` and trim each part. Parts keep their raw case.
pub fn split_header(raw: &str) -> Vec<&str> {
    clean_cell(raw).split('|').map(str::trim).collect()
}

/// Normalize a keyword token: lowercase, whitespace and `-` runs → `_`.
pub fn norm_token(tok: &str) -> String {
    let mut out = String::with_capacity(tok.len());
    let mut pending_sep = false;
    for c in clean_cell(tok).chars() {
        if c.is_whitespace() || c == '-' || c == '_' {
            pending_sep = true;
        } else {
            if pending_sep && !out.is_empty() {
                out.push('_');
            }
            pending_sep = false;
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// A rate/charge field, shared by tall columns, wide payer columns, and JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChargeField {
    // Item-level standard charges (same for every payer).
    Gross,
    DiscountedCash,
    Min,
    Max,
    // Payer/plan-specific.
    NegotiatedDollar,
    NegotiatedPercentage,
    NegotiatedAlgorithm,
    Methodology,
    Median,
    P10,
    P90,
    Count,
    /// v2 only.
    EstimatedAmount,
    AdditionalPayerNotes,
}

impl ChargeField {
    pub fn is_payer_specific(self) -> bool {
        !matches!(
            self,
            ChargeField::Gross | ChargeField::DiscountedCash | ChargeField::Min | ChargeField::Max
        )
    }

    /// Suffix after `standard_charge|` (tall) or `standard_charge|payer|plan|`
    /// (wide). Also accepts observed variants.
    fn from_standard_charge_suffix(norm: &str) -> Option<ChargeField> {
        Some(match norm {
            "gross" | "gross_charge" => ChargeField::Gross,
            "discounted_cash" | "discounted_cash_price" => ChargeField::DiscountedCash,
            "min" | "minimum" => ChargeField::Min,
            "max" | "maximum" => ChargeField::Max,
            "negotiated_dollar" | "negotiated_rate_dollar" => ChargeField::NegotiatedDollar,
            "negotiated_percentage"
            | "negotiated_percent"
            | "negotiated_rate_percentage"
            | "negotiated_rate_percent" => ChargeField::NegotiatedPercentage,
            // The v3 data dictionary itself writes `...| algorithm` once.
            "negotiated_algorithm" | "negotiated_rate_algorithm" | "algorithm" => {
                ChargeField::NegotiatedAlgorithm
            }
            "methodology" => ChargeField::Methodology,
            _ => return None,
        })
    }

    /// Standalone fields that are not under `standard_charge|`: tall form
    /// alone (`median_amount`), wide form with `|payer|plan`.
    fn from_bare(norm: &str) -> Option<ChargeField> {
        Some(match norm {
            "median_amount" => ChargeField::Median,
            "10th_percentile" => ChargeField::P10,
            "90th_percentile" => ChargeField::P90,
            "count" => ChargeField::Count,
            "estimated_amount" | "estimate_amount" => ChargeField::EstimatedAmount,
            "additional_payer_notes" => ChargeField::AdditionalPayerNotes,
            _ => return None,
        })
    }
}

/// What a data column (row 3 onward) holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataColumn {
    Description,
    Setting,
    DrugUnitOfMeasurement,
    DrugTypeOfMeasurement,
    Modifiers,
    BillingClass,
    AdditionalGenericNotes,
    /// `code|n` (1-based `n`).
    Code(u32),
    /// `code|n|type`.
    CodeType(u32),
    /// Tall only.
    PayerName,
    /// Tall only.
    PlanName,
    /// A charge column without payer/plan in the header: item-level charges
    /// in both layouts, plus payer-specific fields in tall.
    Charge(ChargeField),
    /// Wide only: payer/plan are in the header. `plan` is `None` when the
    /// header has no plan segment (`standard_charge|Triwest|negotiated_dollar`).
    PayerCharge {
        payer: String,
        plan: Option<String>,
        field: ChargeField,
    },
    /// Not recognized. Kept so callers can report it; never fatal.
    Unknown(String),
}

/// Map one data header cell.
pub fn map_data_header(raw: &str) -> DataColumn {
    let parts = split_header(raw);
    let head = norm_token(parts[0]);

    if parts.len() == 1 {
        return match head.as_str() {
            "description" => DataColumn::Description,
            "setting" => DataColumn::Setting,
            "drug_unit_of_measurement" => DataColumn::DrugUnitOfMeasurement,
            "drug_type_of_measurement" => DataColumn::DrugTypeOfMeasurement,
            "modifiers" => DataColumn::Modifiers,
            "billing_class" => DataColumn::BillingClass,
            "additional_generic_notes" => DataColumn::AdditionalGenericNotes,
            "payer_name" => DataColumn::PayerName,
            "plan_name" => DataColumn::PlanName,
            "code" => DataColumn::Code(1),
            other => match ChargeField::from_bare(other) {
                Some(f) => DataColumn::Charge(f),
                None => DataColumn::Unknown(clean_cell(raw).to_string()),
            },
        };
    }

    // code|n and code|n|type
    if head == "code" {
        if let Ok(n) = parts[1].parse::<u32>() {
            match parts.len() {
                2 => return DataColumn::Code(n),
                3 if norm_token(parts[2]) == "type" => return DataColumn::CodeType(n),
                _ => {}
            }
        }
        return DataColumn::Unknown(clean_cell(raw).to_string());
    }

    // Some files put an empty segment after the prefix
    // (`standard_charge||PAYER|PLAN|negotiated_dollar`); drop it.
    let mut parts = parts;
    if parts.len() >= 3 && parts[1].is_empty() {
        parts.remove(1);
    }

    if matches!(
        head.as_str(),
        "standard_charge" | "standard_charges" | "standard_charged"
    ) {
        let suffix = norm_token(parts[parts.len() - 1]);
        if let Some(field) = ChargeField::from_standard_charge_suffix(&suffix) {
            if parts.len() == 2 {
                return DataColumn::Charge(field);
            }
            if field.is_payer_specific() {
                if let Some(col) = payer_column(&parts[1..parts.len() - 1], field) {
                    return col;
                }
            }
        }
        return DataColumn::Unknown(clean_cell(raw).to_string());
    }

    if let Some(field) = ChargeField::from_bare(&head) {
        if let Some(col) = payer_column(&parts[1..], field) {
            return col;
        }
    }
    DataColumn::Unknown(clean_cell(raw).to_string())
}

/// Build a wide payer column from the `payer|plan...` segments. Extra
/// segments beyond payer and plan (e.g. a product line) are kept in the plan,
/// rejoined with `|`, so nothing from the header is lost.
fn payer_column(segments: &[&str], field: ChargeField) -> Option<DataColumn> {
    let (payer, rest) = segments.split_first()?;
    if payer.is_empty() {
        return None;
    }
    let plan = rest.join("|");
    Some(DataColumn::PayerCharge {
        payer: payer.to_string(),
        plan: (!plan.is_empty()).then_some(plan),
        field,
    })
}

/// Metadata header (row 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaColumn {
    HospitalName,
    LastUpdatedOn,
    Version,
    /// v3 `location_name`, v2 `hospital_location`.
    LocationName,
    HospitalAddress,
    /// `license_number|<state>`; state is kept raw (trimmed).
    LicenseNumber {
        state: Option<String>,
    },
    Type2Npi,
    AttesterName,
    /// The header *is* the attestation (v3) / affirmation (v2) sentence.
    Attestation,
    GeneralContractProvisions,
    FinancialAidPolicy,
    /// Trailing empty header cells (CMS examples pad row 1 with commas).
    Blank,
    Unknown(String),
}

/// Both the v2 affirmation and v3 attestation statements start this way.
const ATTESTATION_PREFIX: &str = "to the best of its knowledge";

pub fn map_meta_header(raw: &str) -> MetaColumn {
    let cleaned = clean_cell(raw);
    if cleaned.is_empty() {
        return MetaColumn::Blank;
    }
    if cleaned.to_lowercase().starts_with(ATTESTATION_PREFIX) {
        return MetaColumn::Attestation;
    }
    let parts = split_header(cleaned);
    match norm_token(parts[0]).as_str() {
        "license_number" if parts.len() <= 2 => MetaColumn::LicenseNumber {
            state: parts
                .get(1)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
        },
        _ if parts.len() > 1 => MetaColumn::Unknown(cleaned.to_string()),
        "hospital_name" => MetaColumn::HospitalName,
        "last_updated_on" => MetaColumn::LastUpdatedOn,
        "version" => MetaColumn::Version,
        "location_name" | "hospital_location" => MetaColumn::LocationName,
        "hospital_address" => MetaColumn::HospitalAddress,
        "type_2_npi" => MetaColumn::Type2Npi,
        "attester_name" => MetaColumn::AttesterName,
        "general_contract_provisions" => MetaColumn::GeneralContractProvisions,
        "financial_aid_policy" => MetaColumn::FinancialAidPolicy,
        _ => MetaColumn::Unknown(cleaned.to_string()),
    }
}

/// A mapped data header row.
#[derive(Debug, Clone)]
pub struct DataHeader {
    pub columns: Vec<DataColumn>,
    /// Distinct (payer, plan) pairs from wide columns, in first-seen order.
    pub payer_plans: Vec<(String, Option<String>)>,
}

impl DataHeader {
    pub fn parse<S: AsRef<str>>(cells: &[S]) -> DataHeader {
        let columns: Vec<DataColumn> = cells.iter().map(|c| map_data_header(c.as_ref())).collect();
        let mut payer_plans: Vec<(String, Option<String>)> = Vec::new();
        for c in &columns {
            if let DataColumn::PayerCharge { payer, plan, .. } = c {
                if !payer_plans.iter().any(|(p, q)| p == payer && q == plan) {
                    payer_plans.push((payer.clone(), plan.clone()));
                }
            }
        }
        DataHeader {
            columns,
            payer_plans,
        }
    }

    pub fn has(&self, col: &DataColumn) -> bool {
        self.columns.contains(col)
    }

    /// Wide files carry payer/plan in headers.
    pub fn is_wide(&self) -> bool {
        !self.payer_plans.is_empty()
    }

    /// Tall files have a `payer_name` column.
    pub fn is_tall(&self) -> bool {
        self.has(&DataColumn::PayerName)
    }

    /// Unknown header cells, excluding blank padding.
    pub fn unknown(&self) -> impl Iterator<Item = &str> {
        self.columns.iter().filter_map(|c| match c {
            DataColumn::Unknown(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        })
    }
}

impl fmt::Display for ChargeField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ChargeField::*;

    fn pc(payer: &str, plan: Option<&str>, field: ChargeField) -> DataColumn {
        DataColumn::PayerCharge {
            payer: payer.into(),
            plan: plan.map(Into::into),
            field,
        }
    }

    #[test]
    fn norm_token_variants() {
        assert_eq!(norm_token(" Standard Charge "), "standard_charge");
        assert_eq!(norm_token("negotiated-dollar"), "negotiated_dollar");
        assert_eq!(norm_token("\u{feff}hospital_name"), "hospital_name");
        assert_eq!(norm_token("10th  Percentile"), "10th_percentile");
    }

    #[test]
    fn spaced_and_unspaced_pipes_are_equivalent() {
        for h in [
            "code | 1 | type",
            "code|1|type",
            "CODE |1| Type",
            "\"code|1|type\"",
        ] {
            assert_eq!(map_data_header(h), DataColumn::CodeType(1), "{h}");
        }
        assert_eq!(map_data_header("code | 12"), DataColumn::Code(12));
        assert_eq!(
            map_data_header("standard_charge | gross"),
            map_data_header("standard_charge|gross")
        );
        assert_eq!(
            map_data_header("standard_charge | Aetna | PPO | negotiated_dollar"),
            map_data_header("standard_charge|Aetna|PPO|negotiated_dollar")
        );
    }

    #[test]
    fn tall_columns_v2_and_v3() {
        use DataColumn as D;
        let cases = [
            ("description", D::Description),
            ("setting", D::Setting),
            ("payer_name", D::PayerName),
            ("plan_name", D::PlanName),
            ("modifiers", D::Modifiers),
            ("billing_class", D::BillingClass),
            ("drug_unit_of_measurement", D::DrugUnitOfMeasurement),
            ("drug_type_of_measurement", D::DrugTypeOfMeasurement),
            ("standard_charge|gross", D::Charge(Gross)),
            (
                "standard_charge | discounted_cash",
                D::Charge(DiscountedCash),
            ),
            ("standard_charge|min", D::Charge(Min)),
            ("standard_charge|max", D::Charge(Max)),
            (
                "standard_charge|negotiated_dollar",
                D::Charge(NegotiatedDollar),
            ),
            (
                "standard_charge|negotiated_percentage",
                D::Charge(NegotiatedPercentage),
            ),
            (
                "standard_charge|negotiated_algorithm",
                D::Charge(NegotiatedAlgorithm),
            ),
            ("standard_charge | methodology", D::Charge(Methodology)),
            ("estimated_amount", D::Charge(EstimatedAmount)), // v2
            ("median_amount", D::Charge(Median)),             // v3
            ("10th_percentile", D::Charge(P10)),
            ("90th_percentile", D::Charge(P90)),
            ("count", D::Charge(Count)),
            ("additional_generic_notes", D::AdditionalGenericNotes),
        ];
        for (h, want) in cases {
            assert_eq!(map_data_header(h), want, "{h}");
        }
    }

    #[test]
    fn wide_columns_keep_payer_and_plan_raw() {
        assert_eq!(
            map_data_header("standard_charge|Platform Health Insurance|PPO|negotiated_dollar"),
            pc("Platform Health Insurance", Some("PPO"), NegotiatedDollar)
        );
        // v2 example uses underscores in payer names; kept as-is.
        assert_eq!(
            map_data_header("standard_charge|Platform_Health_Insurance|PPO|methodology"),
            pc("Platform_Health_Insurance", Some("PPO"), Methodology)
        );
        assert_eq!(
            map_data_header("median_amount | Region Health Insurance | HMO"),
            pc("Region Health Insurance", Some("HMO"), Median)
        );
        assert_eq!(
            map_data_header("10th_percentile|A|B"),
            pc("A", Some("B"), P10)
        );
        assert_eq!(map_data_header("count|A|B"), pc("A", Some("B"), Count));
        assert_eq!(
            map_data_header("estimated_amount|A|B"),
            pc("A", Some("B"), EstimatedAmount)
        );
        assert_eq!(
            map_data_header("additional_payer_notes|A|B"),
            pc("A", Some("B"), AdditionalPayerNotes)
        );
    }

    #[test]
    fn wide_irregular_shapes() {
        // No plan segment.
        assert_eq!(
            map_data_header("standard_charge|Triwest|negotiated_dollar"),
            pc("Triwest", None, NegotiatedDollar)
        );
        // Extra product segment stays in the plan, raw.
        assert_eq!(
            map_data_header("standard_charge|Aetna|PPO|MEDICARE|negotiated_dollar"),
            pc("Aetna", Some("PPO|MEDICARE"), NegotiatedDollar)
        );
        // Leading empty segment.
        assert_eq!(
            map_data_header("standard_charge||Aetna|PPO|negotiated_percentage"),
            pc("Aetna", Some("PPO"), NegotiatedPercentage)
        );
        // Spec's own `| algorithm` spelling.
        assert_eq!(
            map_data_header("standard_charge | A | B | algorithm"),
            pc("A", Some("B"), NegotiatedAlgorithm)
        );
        // Item-level suffix with a payer is not valid.
        assert!(matches!(
            map_data_header("standard_charge|A|B|gross"),
            DataColumn::Unknown(_)
        ));
    }

    #[test]
    fn unknown_columns_are_reported_not_fatal() {
        let h = DataHeader::parse(&["description", "my_custom_col", "code|x", "", "code|1"]);
        let unknown: Vec<&str> = h.unknown().collect();
        assert_eq!(unknown, vec!["my_custom_col", "code|x"]);
        assert_eq!(h.columns[4], DataColumn::Code(1));
    }

    #[test]
    fn header_shape_detection() {
        let tall = DataHeader::parse(&["description", "payer_name", "plan_name"]);
        assert!(tall.is_tall() && !tall.is_wide());
        let wide = DataHeader::parse(&[
            "description",
            "standard_charge|A|P1|negotiated_dollar",
            "standard_charge|A|P1|methodology",
            "standard_charge|B|P2|negotiated_dollar",
        ]);
        assert!(wide.is_wide() && !wide.is_tall());
        assert_eq!(
            wide.payer_plans,
            vec![
                ("A".to_string(), Some("P1".to_string())),
                ("B".to_string(), Some("P2".to_string()))
            ]
        );
    }

    #[test]
    fn meta_headers() {
        use MetaColumn as M;
        assert_eq!(map_meta_header("\u{feff}hospital_name"), M::HospitalName);
        assert_eq!(map_meta_header("location_name"), M::LocationName);
        assert_eq!(map_meta_header("hospital_location"), M::LocationName); // v2
        assert_eq!(
            map_meta_header("license_number|CA"),
            M::LicenseNumber {
                state: Some("CA".into())
            }
        );
        assert_eq!(
            map_meta_header("license_number | ca "),
            M::LicenseNumber {
                state: Some("ca".into())
            }
        );
        assert_eq!(
            map_meta_header("license_number"),
            M::LicenseNumber { state: None }
        );
        assert_eq!(
            map_meta_header(
                "To the best of its knowledge and belief, this hospital has included all..."
            ),
            M::Attestation
        );
        assert_eq!(
            map_meta_header("To the best of its knowledge and belief, the hospital has ..."),
            M::Attestation
        );
        assert_eq!(map_meta_header("type_2_npi"), M::Type2Npi);
        assert_eq!(map_meta_header(""), M::Blank);
        assert_eq!(map_meta_header("foo"), M::Unknown("foo".into()));
    }
}
