mod common;

use common::{parse, parse_with, rows};
use mrf_core::{Table, ValueFilter};
use mrf_hospital::{HospitalOptions, Layout};

#[test]
fn cms_v3_tall_example() {
    let out = tempfile::tempdir().unwrap();
    let s = parse("v3/tall.csv", out.path());
    assert_eq!(s.layout, Layout::CsvTall);
    assert_eq!(s.mrf_version.as_deref(), Some("3.0.0"));
    assert_eq!(s.counts.charges, 45, "one charge per data row");
    assert_eq!(s.counts.rejects, 0);

    let file = &rows(out.path(), Table::HospitalFile)[0];
    assert_eq!(file.get("layout"), Some("csv_tall"));
    assert_eq!(file.get("hospital_name"), Some("West Mercy Hospital"));
    assert_eq!(file.get("license_number"), Some("50056"));
    assert_eq!(file.get("license_state"), Some("CA"));
    assert_eq!(file.get("attestation"), Some("true"));
    assert_eq!(file.get("attester_name"), Some("Leigh Attester"));
    assert_eq!(
        file.get("location_names"),
        Some("[West Mercy Hospital, West Mercy Surgical Center]")
    );
    assert_eq!(
        file.get("type_2_npis"),
        Some("[0000000001, 0000000002, 0000000003]")
    );

    // Items de-duplicated across payer rows: MRI appears twice (two payers)
    // but is one item with two codes.
    let items = rows(out.path(), Table::HospitalItem);
    let mri: Vec<_> = items
        .iter()
        .filter(|r| r.get("description") == Some("MRI of brain (no contrast)"))
        .collect();
    assert_eq!(mri.len(), 1);
    let mri_id = mri[0].get("item_id").unwrap();
    let codes: Vec<_> = rows(out.path(), Table::HospitalItemCode)
        .into_iter()
        .filter(|r| r.get("item_id") == Some(mri_id))
        .map(|r| {
            (
                r.get("code").unwrap().to_string(),
                r.get("code_type").unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        codes,
        vec![("611".into(), "RC".into()), ("70551".into(), "CPT".into())]
    );

    let charges = rows(out.path(), Table::HospitalCharge);
    let mri_charges: Vec<_> = charges
        .iter()
        .filter(|r| r.get("item_id") == Some(mri_id))
        .collect();
    assert_eq!(mri_charges.len(), 2);
    let ppo = mri_charges
        .iter()
        .find(|r| r.get("plan_name_raw") == Some("PPO"))
        .unwrap();
    assert_eq!(ppo.get("payer_name_raw"), Some("Platform Health Insurance"));
    assert_eq!(ppo.get("gross"), Some("1200"));
    assert_eq!(ppo.get("discounted_cash"), Some("1080"));
    assert_eq!(ppo.get("negotiated_dollar"), Some("400"));
    assert_eq!(ppo.get("deid_min"), Some("250"));
    assert_eq!(ppo.get("deid_max"), Some("400"));
    assert_eq!(ppo.get("methodology"), Some("fee schedule"));

    // v3 allowed-amount fields, including the textual count.
    let csec = charges
        .iter()
        .find(|r| r.get("allowed_count") == Some("1 through 10"))
        .unwrap();
    assert_eq!(csec.get("median_amount"), Some("11000"));
    assert_eq!(csec.get("p10_amount"), Some("8000"));
    assert_eq!(csec.get("p90_amount"), Some("17000"));
    assert!(csec
        .get("negotiated_algorithm")
        .unwrap()
        .starts_with("Allowed amount"));

    // Modifier rows: no codes, modifiers kept raw.
    assert!(items.iter().any(|r| r.get("modifiers") == Some("50|62")));
}

#[test]
fn cms_v2_tall_example() {
    let out = tempfile::tempdir().unwrap();
    let s = parse("v2/tall.csv", out.path());
    assert_eq!(s.layout, Layout::CsvTall);
    assert_eq!(s.mrf_version.as_deref(), Some("2.0.0"));
    assert_eq!(s.counts.charges, 31);
    assert_eq!(s.counts.rejects, 0);

    let file = &rows(out.path(), Table::HospitalFile)[0];
    // v2 `hospital_location` maps to location_names; no type 2 NPI in v2.0.
    assert_eq!(
        file.get("location_names"),
        Some("[West Mercy Hospital, West Mercy Surgical Center]")
    );
    assert_eq!(file.get("type_2_npis"), None);
    assert_eq!(file.get("attestation"), Some("true"));

    let charges = rows(out.path(), Table::HospitalCharge);
    // v2 has estimated_amount; v3 allowed-amount fields are null.
    assert!(charges
        .iter()
        .any(|r| r.get("estimated_amount") == Some("22243.34")));
    assert!(charges.iter().all(|r| r.get("median_amount").is_none()
        && r.get("p10_amount").is_none()
        && r.get("allowed_count").is_none()));
    // Same item/payer/plan repeated with different algorithms: both kept.
    let hip_algs = charges
        .iter()
        .filter(|r| {
            r.get("negotiated_dollar") == Some("20000") && r.get("plan_name_raw") == Some("PPO")
        })
        .count();
    assert!(hip_algs >= 2, "{hip_algs}");
}

#[test]
fn edge_cases_bom_crlf_money_bad_rows() {
    let out = tempfile::tempdir().unwrap();
    let s = parse("edge/tall_edge.csv", out.path());
    assert_eq!(s.layout, Layout::CsvTall);

    let file = &rows(out.path(), Table::HospitalFile)[0];
    assert_eq!(
        file.get("hospital_name"),
        Some("Edge Hospital"),
        "BOM stripped"
    );
    assert_eq!(file.get("license_state"), Some("NY"), "spaced license pipe");
    assert_eq!(file.get("type_2_npis"), Some("[111, 222]"));
    assert_eq!(file.get("hospital_addresses"), Some("[1 A St, 2 B St]"));

    let items = rows(out.path(), Table::HospitalItem);
    let descs: Vec<_> = items
        .iter()
        .map(|r| r.get("description").unwrap())
        .collect();
    assert_eq!(descs, vec!["Office visit", "Lab", "Gross only"]);
    // CRLF: the last cell of each row has no stray `\r`.
    assert!(items.iter().all(|r| r.get("setting").is_none()));

    let charges = rows(out.path(), Table::HospitalCharge);
    assert_eq!(charges.len(), 4);
    assert_eq!(charges[0].get("gross"), Some("1234.5"), "$1,234.50");
    assert_eq!(charges[0].get("discounted_cash"), Some("1000"));
    assert_eq!(charges[0].get("negotiated_dollar"), Some("800"));
    assert_eq!(charges[0].get("methodology"), Some("fee schedule"));
    // Payer row with no terms is still a row (tall rows are never dropped).
    assert_eq!(charges[1].get("payer_name_raw"), Some("Beta Ins"));
    // Bad money: field null, row kept.
    assert_eq!(charges[2].get("payer_name_raw"), Some("Acme Health"));
    assert_eq!(charges[2].get("negotiated_dollar"), None);
    assert_eq!(charges[2].get("gross"), None, "empty money is null");
    // No payer: null payer row.
    assert_eq!(charges[3].get("payer_name_raw"), None);
    assert_eq!(charges[3].get("gross"), Some("500"));

    let rejects = rows(out.path(), Table::Rejects);
    let locs: Vec<_> = rejects.iter().map(|r| r.get("location").unwrap()).collect();
    assert_eq!(rejects.len(), 3, "{rejects:?}");
    assert!(locs[0].starts_with("row 6, column"), "{locs:?}");
    assert_eq!(rejects[0].get("raw_snippet"), Some("N/A"));
    assert_eq!(locs[1], "row 7");
    assert!(rejects[1].get("reason").unwrap().contains("cells"));
    assert_eq!(locs[2], "row 9");
    assert_eq!(s.counts.rejects, 3);
}

#[test]
fn gzip_csv_with_json_extension() {
    let out = tempfile::tempdir().unwrap();
    let s = parse("edge/tall_v3_gzipped.json", out.path());
    assert_eq!(s.layout, Layout::CsvTall);
    assert_eq!(s.counts.charges, 45);
}

#[test]
fn code_filter_applies_during_parse() {
    let out = tempfile::tempdir().unwrap();
    let opts = HospitalOptions {
        codes: ValueFilter::from_values(["70551", "c1785"]),
        ..Default::default()
    };
    let s = parse_with("v3/tall.csv", out.path(), &opts);
    assert_eq!(s.counts.items, 2);
    assert_eq!(s.counts.charges, 4);
    assert!(s.filtered_out > 0);
}

#[test]
fn deterministic_output() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    parse("v3/tall.csv", a.path());
    parse("v3/tall.csv", b.path());
    for t in [
        Table::HospitalFile,
        Table::HospitalItem,
        Table::HospitalItemCode,
        Table::HospitalCharge,
        Table::Rejects,
    ] {
        let pa = mrf_core::read::table_parts(a.path(), t).unwrap();
        let pb = mrf_core::read::table_parts(b.path(), t).unwrap();
        assert_eq!(pa.len(), 1);
        assert_eq!(
            std::fs::read(&pa[0]).unwrap(),
            std::fs::read(&pb[0]).unwrap(),
            "{t:?}"
        );
    }
}
