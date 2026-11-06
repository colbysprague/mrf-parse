//! Columns/keys outside the CMS spec are kept in `extra`, and zip archives
//! expand to one parsed file per entry. Both found on real hospital files
//! (Gundersen: `count_of_compared_rates`, `footnote`, `as_of_date`, zipped).

mod common;

use std::io::Write;

use common::{fixture, parse, rows};
use mrf_core::{Source, Table};
use mrf_hospital::{parse_all, parse_file, HospitalOptions, Layout};

#[test]
fn unknown_csv_columns_kept_in_extra() {
    let out = tempfile::tempdir().unwrap();
    parse("edge/tall_extra_cols.csv", out.path());

    let file = &rows(out.path(), Table::HospitalFile)[0];
    assert_eq!(file.get("extra"), Some(r#"{"as_of_date":"2026-03-01"}"#));

    let charges = rows(out.path(), Table::HospitalCharge);
    assert_eq!(
        charges[0].get("extra"),
        Some(
            r#"{"count_of_compared_rates":"17","footnote":"Subject to stoploss","footnote#2":"Second note"}"#
        ),
        "file order kept; duplicate header not overwritten"
    );
    assert_eq!(
        charges[0].get("allowed_count"),
        Some("0"),
        "CMS count still mapped"
    );
    assert_eq!(charges[1].get("extra"), None, "empty cells add nothing");
}

#[test]
fn wide_unknown_columns_copied_to_each_payer_row() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.csv");
    std::fs::write(
        &p,
        "hospital_name,version\nH,3.0.0\n\
         description,code|1,code|1|type,standard_charge|A|P|negotiated_dollar,standard_charge|B|Q|negotiated_dollar,vendor_flag\n\
         X,1,RC,10,20,yes\n",
    )
    .unwrap();
    let out = dir.path().join("out");
    let s = parse_file(&Source::Path(p), &out, &HospitalOptions::default()).unwrap();
    assert_eq!(s.layout, Layout::CsvWide);
    let charges = rows(&out, Table::HospitalCharge);
    assert_eq!(charges.len(), 2);
    assert!(charges
        .iter()
        .all(|c| c.get("extra") == Some(r#"{"vendor_flag":"yes"}"#)));
}

#[test]
fn unknown_json_keys_kept_at_each_level() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.json");
    std::fs::write(
        &p,
        r#"{"hospital_name": "H", "version": "3.0.0", "vendor_build": 7,
            "standard_charge_information": [{
              "description": "MRI", "internal_id": "A1",
              "code_information": [{"code": "70551", "type": "CPT"}],
              "standard_charges": [{"setting": "outpatient", "gross_charge": 100, "cdm_dept": "RAD",
                "payers_information": [
                  {"payer_name": "Acme", "plan_name": "PPO", "methodology": "fee schedule",
                   "standard_charge_dollar": 80, "footnote": "stoploss"}]}]}]}"#,
    )
    .unwrap();
    let out = dir.path().join("out");
    parse_file(&Source::Path(p), &out, &HospitalOptions::default()).unwrap();
    let file = &rows(&out, Table::HospitalFile)[0];
    assert_eq!(file.get("extra"), Some(r#"{"vendor_build":7}"#));
    let item = &rows(&out, Table::HospitalItem)[0];
    assert_eq!(item.get("extra"), Some(r#"{"internal_id":"A1"}"#));
    let charge = &rows(&out, Table::HospitalCharge)[0];
    assert_eq!(
        charge.get("extra"),
        Some(r#"{"cdm_dept":"RAD","footnote":"stoploss"}"#),
        "standard_charges keys then payer keys"
    );
}

#[test]
fn cms_examples_have_no_extra() {
    for f in [
        "v3/tall.csv",
        "v3/wide.csv",
        "v3/example.json",
        "v2/tall.csv",
        "v2/wide.csv",
        "v2/example.json",
    ] {
        let out = tempfile::tempdir().unwrap();
        parse(f, out.path());
        for t in [
            Table::HospitalFile,
            Table::HospitalItem,
            Table::HospitalCharge,
        ] {
            assert!(
                rows(out.path(), t).iter().all(|r| r.get("extra").is_none()),
                "{f} {t:?}"
            );
        }
    }
}

fn zip_of(entries: &[(&str, std::path::PathBuf)]) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, path) in entries {
            z.start_file(*name, opts).unwrap();
            z.write_all(&std::fs::read(path).unwrap()).unwrap();
        }
        z.finish().unwrap();
    }
    buf.into_inner()
}

#[test]
fn zip_archive_parses_every_entry() {
    let dir = tempfile::tempdir().unwrap();
    // Extension-less name, like the Gundersen download URLs.
    let p = dir.path().join("MRFDownload");
    std::fs::write(
        &p,
        zip_of(&[
            ("tall.csv", fixture("v3/tall.csv")),
            ("nested/example.json", fixture("v3/example.json")),
        ]),
    )
    .unwrap();
    let out = dir.path().join("out");
    let results = parse_all(&Source::Path(p.clone()), &out, &HospitalOptions::default());
    assert_eq!(results.len(), 2);
    let (uri0, r0) = &results[0];
    let (uri1, r1) = &results[1];
    assert_eq!(uri0, &format!("{}!/tall.csv", p.display()));
    assert_eq!(uri1, &format!("{}!/nested/example.json", p.display()));
    let (s0, s1) = (r0.as_ref().unwrap(), r1.as_ref().unwrap());
    assert_eq!(s0.layout, Layout::CsvTall);
    assert_eq!(s1.layout, Layout::Json);
    assert_eq!(s0.counts.charges, 45);
    assert_ne!(s0.file_id, s1.file_id);

    // Entry file_id equals the file_id of the same bytes outside the zip.
    let plain = tempfile::tempdir().unwrap();
    let direct = parse("v3/tall.csv", plain.path());
    assert_eq!(s0.file_id, direct.file_id);

    let files = rows(&out, Table::HospitalFile);
    let uris: Vec<_> = files.iter().map(|r| r.get("source_uri").unwrap()).collect();
    assert!(uris.iter().all(|u| u.contains("MRFDownload!/")), "{uris:?}");

    // parse_file refuses a multi-entry archive with a clear error.
    let err = parse_file(&Source::Path(p), &out, &HospitalOptions::default()).unwrap_err();
    assert!(err.to_string().contains("holds 2 files"), "{err}");
}
