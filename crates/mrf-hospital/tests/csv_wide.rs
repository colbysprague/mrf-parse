mod common;

use common::{parse, rows};
use mrf_core::Table;
use mrf_hospital::Layout;

#[test]
fn cms_v3_wide_example_unpivots() {
    let out = tempfile::tempdir().unwrap();
    let s = parse("v3/wide.csv", out.path());
    assert_eq!(s.layout, Layout::CsvWide);
    assert_eq!(s.mrf_version.as_deref(), Some("3.0.0"));
    assert_eq!(s.counts.rejects, 0);

    let file = &rows(out.path(), Table::HospitalFile)[0];
    assert_eq!(
        file.get("hospital_name"),
        Some("West Mercy Hospital"),
        "BOM"
    );
    // Trailing spaces in the type_2_npi cell are trimmed.
    assert_eq!(
        file.get("type_2_npis"),
        Some("[0000000001, 0000000002, 0000000003]")
    );

    let items = rows(out.path(), Table::HospitalItem);
    let charges = rows(out.path(), Table::HospitalCharge);
    let mri_id = items
        .iter()
        .find(|r| r.get("description") == Some("MRI of brain (no contrast)"))
        .unwrap()
        .get("item_id")
        .unwrap()
        .to_string();
    let mri: Vec<_> = charges
        .iter()
        .filter(|r| r.get("item_id") == Some(mri_id.as_str()))
        .collect();
    assert_eq!(mri.len(), 2, "one charge per payer/plan");
    for c in &mri {
        // Item-level charges copied onto each payer row.
        assert_eq!(c.get("gross"), Some("1200"));
        assert_eq!(c.get("deid_min"), Some("250"));
        assert_eq!(c.get("deid_max"), Some("400"));
    }
    let hmo = mri
        .iter()
        .find(|r| r.get("payer_name_raw") == Some("Region Health Insurance"))
        .unwrap();
    assert_eq!(hmo.get("plan_name_raw"), Some("HMO"));
    assert_eq!(hmo.get("negotiated_dollar"), Some("250"));
    assert_eq!(hmo.get("methodology"), Some("fee schedule"));

    // Payer-specific notes come from `additional_payer_notes|payer|plan`.
    assert!(charges.iter().any(|r| r.get("additional_payer_notes")
        == Some("This service paid at 140% of the OPPS APC rate.")));
    // Allowed-amount fields per payer.
    assert!(charges
        .iter()
        .any(|r| r.get("allowed_count") == Some("1 through 10")
            && r.get("median_amount") == Some("11000")));

    // Every item has at least one charge; none lost in the unpivot.
    for it in &items {
        let id = it.get("item_id").unwrap();
        assert!(
            charges.iter().any(|c| c.get("item_id") == Some(id)),
            "{it:?}"
        );
    }
}

#[test]
fn cms_v2_wide_example() {
    let out = tempfile::tempdir().unwrap();
    let s = parse("v2/wide.csv", out.path());
    assert_eq!(s.layout, Layout::CsvWide);
    // Payer names are raw (the v2 wide example uses underscores).
    let charges = rows(out.path(), Table::HospitalCharge);
    assert!(charges
        .iter()
        .any(|r| r.get("payer_name_raw") == Some("Platform_Health_Insurance")));
    assert!(charges
        .iter()
        .any(|r| r.get("estimated_amount") == Some("23145.98")));
    assert!(charges.iter().all(|r| r.get("median_amount").is_none()));

    // The example contains a Windows-1252 em dash (byte 0x97): decoded,
    // rows kept, one reject per affected row (rows 12 and 13).
    let items = rows(out.path(), Table::HospitalItem);
    assert!(items
        .iter()
        .any(|r| r.get("description").is_some_and(|d| d.contains('\u{2014}'))));
    let rejects = rows(out.path(), Table::Rejects);
    assert_eq!(rejects.len(), 2, "{rejects:?}");
    assert!(rejects
        .iter()
        .all(|r| r.get("reason").unwrap().contains("Windows-1252")));
}

#[test]
fn wide_row_without_payer_values_gets_null_payer_charge() {
    let dir = tempfile::tempdir().unwrap();
    let csv = "hospital_name,version\nH,3.0.0\n\
        description,code|1,code|1|type,standard_charge|gross,standard_charge|A|P|negotiated_dollar,standard_charge|B|Q|negotiated_dollar\n\
        Only gross,1,RC,100,,\n\
        One payer,2,RC,100,50,\n";
    let p = dir.path().join("w.csv");
    std::fs::write(&p, csv).unwrap();
    let out = dir.path().join("out");
    let s = mrf_hospital::parse_file(
        &mrf_core::Source::Path(p),
        &out,
        &mrf_hospital::HospitalOptions::default(),
    )
    .unwrap();
    assert_eq!(s.layout, Layout::CsvWide);
    let charges = rows(&out, Table::HospitalCharge);
    assert_eq!(charges.len(), 2);
    assert_eq!(charges[0].get("payer_name_raw"), None);
    assert_eq!(charges[0].get("gross"), Some("100"));
    assert_eq!(charges[1].get("payer_name_raw"), Some("A"));
    assert_eq!(charges[1].get("plan_name_raw"), Some("P"));
    assert_eq!(charges[1].get("negotiated_dollar"), Some("50"));
}
