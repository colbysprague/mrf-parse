mod common;

use common::{fixture, parse, rows};
use mrf_core::{Source, Table};
use mrf_hospital::{parse_file, HospitalError, HospitalOptions, Layout};

#[test]
fn cms_v3_json_example() {
    let out = tempfile::tempdir().unwrap();
    let s = parse("v3/example.json", out.path());
    assert_eq!(s.layout, Layout::Json);
    assert_eq!(s.mrf_version.as_deref(), Some("3.0.0"));
    assert_eq!(s.counts.rejects, 0);

    let file = &rows(out.path(), Table::HospitalFile)[0];
    assert_eq!(file.get("layout"), Some("json"));
    assert_eq!(file.get("license_state"), Some("CA"));
    assert_eq!(file.get("license_number"), Some("50056"));
    assert_eq!(file.get("attestation"), Some("true"));
    assert_eq!(file.get("attester_name"), Some("Leigh Attester"));
    assert_eq!(
        file.get("type_2_npis"),
        Some("[0000000001, 0000000002, 0000000003]")
    );
    // Structured provisions kept as JSON text.
    let gcp = file.get("general_contract_provisions").unwrap();
    assert!(gcp.starts_with('[') && gcp.contains("Region Health Insurance"));

    let items = rows(out.path(), Table::HospitalItem);
    let mri = items
        .iter()
        .find(|r| r.get("description") == Some("MRI of brain (no contrast)"))
        .unwrap();
    assert_eq!(mri.get("setting"), Some("outpatient"));
    let charges: Vec<_> = rows(out.path(), Table::HospitalCharge)
        .into_iter()
        .filter(|r| r.get("item_id") == mri.get("item_id"))
        .collect();
    assert_eq!(charges.len(), 2);
    assert!(charges.iter().all(|c| c.get("gross") == Some("1200")
        && c.get("deid_min") == Some("250")
        && c.get("deid_max") == Some("400")));

    // modifier_information → items with modifiers and no codes.
    let m = items
        .iter()
        .find(|r| r.get("modifiers") == Some("50|62"))
        .expect("modifier item");
    assert_eq!(m.get("setting"), Some("both"));

    // Drug information.
    assert!(items
        .iter()
        .any(|r| r.get("drug_type_of_measurement") == Some("ML")
            && r.get("drug_unit_of_measurement") == Some("100")));
}

#[test]
fn cms_v2_json_example() {
    let out = tempfile::tempdir().unwrap();
    let s = parse("v2/example.json", out.path());
    assert_eq!(s.layout, Layout::Json);
    assert_eq!(s.mrf_version.as_deref(), Some("2.0.0"));
    assert_eq!(s.counts.rejects, 0);
    let file = &rows(out.path(), Table::HospitalFile)[0];
    // v2 `hospital_location` and `affirmation`.
    assert_eq!(
        file.get("location_names"),
        Some("[West Mercy Hospital, West Mercy Surgical Center]")
    );
    assert_eq!(file.get("attestation"), Some("true"));
    let charges = rows(out.path(), Table::HospitalCharge);
    assert!(charges
        .iter()
        .any(|r| r.get("estimated_amount") == Some("22243.34")));
}

#[test]
fn metadata_after_items_and_malformed_elements() {
    let out = tempfile::tempdir().unwrap();
    let s = parse("edge/json_meta_after.json", out.path());
    let file = &rows(out.path(), Table::HospitalFile)[0];
    assert_eq!(file.get("hospital_name"), Some("Edge JSON Hospital"));
    assert_eq!(file.get("license_state"), Some("NY"));
    assert_eq!(file.get("license_number"), None);

    let items = rows(out.path(), Table::HospitalItem);
    let descs: Vec<_> = items
        .iter()
        .map(|r| r.get("description").unwrap())
        .collect();
    assert_eq!(descs, vec!["Office visit", "Lab", "Gross only"]);
    assert_eq!(
        items[1].get("modifiers"),
        Some("50|62"),
        "array modifier_code"
    );
    let codes = rows(out.path(), Table::HospitalItemCode);
    assert_eq!(
        codes[0].get("code"),
        Some("99213"),
        "numeric code kept as text"
    );

    let charges = rows(out.path(), Table::HospitalCharge);
    assert_eq!(charges.len(), 3);
    assert_eq!(charges[0].get("gross"), Some("1234.5"));
    assert_eq!(charges[1].get("gross"), None, "N/A → null");
    assert_eq!(charges[1].get("negotiated_dollar"), Some("75"), "row kept");
    assert_eq!(charges[2].get("payer_name_raw"), None);

    let rejects = rows(out.path(), Table::Rejects);
    let locs: Vec<_> = rejects.iter().map(|r| r.get("location").unwrap()).collect();
    assert_eq!(
        locs,
        vec![
            "$.standard_charge_information[1]",
            "$.standard_charge_information[2]",
            "$.standard_charge_information[3].standard_charges[0].gross_charge",
            "$.standard_charge_information[4]",
        ]
    );
    assert_eq!(s.counts.rejects, 4);
}

#[test]
fn truncated_json_fails_the_file() {
    let out = tempfile::tempdir().unwrap();
    let err = parse_file(
        &Source::Path(fixture("edge/json_truncated.json")),
        out.path(),
        &HospitalOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, HospitalError::Json(_)), "{err}");
}

#[test]
fn json_without_items_is_unknown_layout() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.json");
    std::fs::write(&p, br#"{"in_network": []}"#).unwrap();
    let err = parse_file(
        &Source::Path(p),
        &dir.path().join("o"),
        &HospitalOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, HospitalError::UnknownLayout(_)), "{err}");
}
