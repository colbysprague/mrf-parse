//! Arrow schemas for every output table, defined in one place.
//!
//! Type choices:
//! * Money and rates are `Float64`. Inputs are decimal text like `1234.50`;
//!   f64 round-trips every value with <= 15 significant digits exactly, which
//!   covers any realistic price.
//! * Identifiers that look numeric (NPI, TIN, codes) are `Utf8`, because
//!   leading zeros and odd formatting matter and we never do math on them.
//! * Dates are kept as the raw `Utf8` text from the file (lossless; files use
//!   inconsistent formats and normalizing them is a later step).
//! * Multi-valued fields are `List<Utf8>`.

use std::sync::Arc;

use arrow_schema::{DataType, Field, Schema, SchemaRef};

/// Every table this project writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Table {
    HospitalFile,
    HospitalItem,
    HospitalItemCode,
    HospitalCharge,
    TicFile,
    TicProvider,
    TicRate,
    TicBundleCode,
    Rejects,
}

impl Table {
    pub const ALL: [Table; 9] = [
        Table::HospitalFile,
        Table::HospitalItem,
        Table::HospitalItemCode,
        Table::HospitalCharge,
        Table::TicFile,
        Table::TicProvider,
        Table::TicRate,
        Table::TicBundleCode,
        Table::Rejects,
    ];

    /// Directory name under the output root.
    pub fn name(self) -> &'static str {
        match self {
            Table::HospitalFile => "hospital_file",
            Table::HospitalItem => "hospital_item",
            Table::HospitalItemCode => "hospital_item_code",
            Table::HospitalCharge => "hospital_charge",
            Table::TicFile => "tic_file",
            Table::TicProvider => "tic_provider",
            Table::TicRate => "tic_rate",
            Table::TicBundleCode => "tic_bundle_code",
            Table::Rejects => "rejects",
        }
    }

    pub fn from_name(name: &str) -> Option<Table> {
        Table::ALL.into_iter().find(|t| t.name() == name)
    }

    pub fn schema(self) -> SchemaRef {
        let fields = match self {
            Table::HospitalFile => vec![
                req("file_id"),
                req("source_uri"),
                req("sha256"),
                req("layout"),
                opt("mrf_version"),
                opt("last_updated_on"),
                opt("hospital_name"),
                list("location_names"),
                list("hospital_addresses"),
                opt("license_number"),
                opt("license_state"),
                list("type_2_npis"),
                opt("attester_name"),
                Field::new("attestation", DataType::Boolean, true),
                // CSV: the cell text. JSON: the array of
                // `{payer_name, plan_name, provisions}` objects as JSON text.
                opt("general_contract_provisions"),
                // Optional CMS field; kept so it is not silently dropped.
                opt("financial_aid_policy"),
                // Columns/keys not in the CMS spec, as a JSON object of
                // name -> raw value (null when there are none).
                opt("extra"),
            ],
            Table::HospitalItem => vec![
                req("item_id"),
                req("file_id"),
                opt("description"),
                opt("setting"),
                money("drug_unit_of_measurement"),
                opt("drug_type_of_measurement"),
                opt("modifiers"),
                // Optional CMS field (`professional` / `facility` / `both`).
                opt("billing_class"),
                // Columns/keys not in the CMS spec, as a JSON object of
                // name -> raw value (null when there are none).
                opt("extra"),
            ],
            Table::HospitalItemCode => vec![req("item_id"), opt("code"), opt("code_type")],
            Table::HospitalCharge => vec![
                req("item_id"),
                opt("payer_name_raw"),
                opt("plan_name_raw"),
                money("gross"),
                money("discounted_cash"),
                money("deid_min"),
                money("deid_max"),
                money("negotiated_dollar"),
                money("negotiated_percentage"),
                opt("negotiated_algorithm"),
                opt("methodology"),
                money("median_amount"),
                money("p10_amount"),
                money("p90_amount"),
                // v3 `count` is text in the spec (e.g. "1 through 10" is
                // allowed for small counts), so it is kept as raw text.
                opt("allowed_count"),
                // v2 only (`estimated_amount`); null for v3.
                money("estimated_amount"),
                opt("additional_payer_notes"),
                opt("additional_generic_notes"),
                // Columns/keys not in the CMS spec, as a JSON object of
                // name -> raw value (null when there are none).
                opt("extra"),
            ],
            Table::TicFile => vec![
                req("file_id"),
                req("source_uri"),
                req("sha256"),
                opt("schema_version"),
                opt("last_updated_on"),
                opt("reporting_entity_name"),
                opt("reporting_entity_type"),
                opt("plan_name"),
                opt("issuer_name"),
                opt("plan_sponsor_name"),
                opt("plan_id_type"),
                opt("plan_id"),
                opt("plan_market_type"),
            ],
            Table::TicProvider => vec![
                req("file_id"),
                req("provider_group_id"),
                list("network_names"),
                opt("tin_type"),
                opt("tin_value"),
                opt("npi"),
                // `tin.business_name` (optional in the TiC schema); kept so
                // it is not dropped.
                opt("tin_business_name"),
            ],
            Table::TicRate => vec![
                req("file_id"),
                // Position of the parent `in_network[]` element. Not in the
                // original column list; added so `tic_rate` joins to
                // `tic_bundle_code`.
                Field::new("in_network_index", DataType::Int64, false),
                req("provider_group_id"),
                opt("negotiation_arrangement"),
                opt("name"),
                opt("billing_code"),
                opt("billing_code_type"),
                opt("billing_code_type_version"),
                opt("description"),
                // `in_network[].severity_of_illness` (optional, DRG-style
                // codes); kept so it is not dropped.
                opt("severity_of_illness"),
                opt("setting"),
                opt("negotiated_type"),
                money("negotiated_rate"),
                opt("expiration_date"),
                opt("billing_class"),
                list("service_codes"),
                list("billing_code_modifiers"),
                opt("additional_information"),
            ],
            Table::TicBundleCode => vec![
                req("file_id"),
                Field::new("in_network_index", DataType::Int64, false),
                opt("billing_code"),
                opt("billing_code_type"),
                // Present on every bundled/covered code in the TiC schema;
                // kept so it is not dropped.
                opt("billing_code_type_version"),
                opt("description"),
            ],
            Table::Rejects => vec![
                req("file_id"),
                req("location"),
                req("reason"),
                opt("raw_snippet"),
            ],
        };
        Arc::new(Schema::new(fields))
    }
}

fn req(name: &str) -> Field {
    Field::new(name, DataType::Utf8, false)
}

fn opt(name: &str) -> Field {
    Field::new(name, DataType::Utf8, true)
}

fn money(name: &str) -> Field {
    Field::new(name, DataType::Float64, true)
}

/// `List<Utf8>`, nullable list with nullable items. The item field must be
/// named `item` to match what `ListBuilder` produces by default.
fn list(name: &str) -> Field {
    Field::new(
        name,
        DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_are_unique() {
        for t in Table::ALL {
            assert_eq!(Table::from_name(t.name()), Some(t));
            assert!(!t.schema().fields().is_empty());
        }
    }
}
