use std::path::{Path, PathBuf};

use mrf_core::read::read_rows;
use mrf_core::testutil::TestServer;
use mrf_core::{Source, Table, ValueFilter};
use mrf_tic::{parse_in_network_file, TicError, TicOptions, TicSummary};

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/tic")
        .join(rel)
}

fn parse(rel: &str, out: &Path, opts: &TicOptions) -> TicSummary {
    parse_in_network_file(&Source::Path(fixture(rel)), out, opts)
        .unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Rows of `table` as maps from column name to value.
fn rows(out: &Path, table: Table) -> Vec<std::collections::BTreeMap<String, Option<String>>> {
    let names: Vec<String> = table
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    read_rows(out, table)
        .unwrap()
        .into_iter()
        .map(|r| names.iter().cloned().zip(r).collect())
        .collect()
}

fn col<'a>(r: &'a std::collections::BTreeMap<String, Option<String>>, c: &str) -> Option<&'a str> {
    r.get(c)
        .unwrap_or_else(|| panic!("no column {c}"))
        .as_deref()
}

#[test]
fn every_cms_example_parses_with_expected_counts() {
    // (file, in_network items, rates, provider rows, bundle codes); counts
    // computed independently from the JSON (prices × provider_references).
    let cases = [
        (
            "in-network-rates-all-negotiated-types-sample.json",
            6,
            10,
            6,
            0,
        ),
        (
            "in-network-rates-bundle-single-plan-sample.json",
            1,
            2,
            15,
            2,
        ),
        (
            "in-network-rates-capitation-single-plan-sample.json",
            1,
            2,
            15,
            2,
        ),
        (
            "in-network-rates-fee-for-service-single-plan-sample.json",
            2,
            5,
            15,
            0,
        ),
        ("in-network-rates-multiple-plans-sample.json", 2, 6, 15, 0),
        ("in-network-rates-no-npi.json", 1, 1, 1, 0),
    ];
    for (f, items, rates, providers, bundles) in cases {
        let out = tempfile::tempdir().unwrap();
        let s = parse(f, out.path(), &TicOptions::default());
        assert_eq!(s.counts.in_network_items, items, "{f}");
        assert_eq!(s.counts.rates, rates, "{f}");
        assert_eq!(s.counts.providers, providers, "{f}");
        assert_eq!(s.counts.bundle_codes, bundles, "{f}");
        assert_eq!(s.counts.rejects, 0, "{f}");
        assert_eq!(s.passes, 1);
    }
}

#[test]
fn all_negotiated_types_and_tic_file_preserved_raw() {
    let out = tempfile::tempdir().unwrap();
    parse(
        "in-network-rates-all-negotiated-types-sample.json",
        out.path(),
        &TicOptions::default(),
    );
    let rates = rows(out.path(), Table::TicRate);
    let mut types: Vec<_> = rates
        .iter()
        .map(|r| col(r, "negotiated_type").unwrap().to_string())
        .collect();
    types.sort();
    types.dedup();
    assert_eq!(
        types,
        vec![
            "derived",
            "fee schedule",
            "negotiated",
            "per diem",
            "percentage"
        ]
    );
    let first = &rates[0];
    assert_eq!(col(first, "negotiated_rate"), Some("150"));
    assert_eq!(col(first, "service_codes"), Some("[11]"));
    assert_eq!(col(first, "provider_group_id"), Some("1"));
    assert_eq!(col(first, "in_network_index"), Some("0"));

    let file = &rows(out.path(), Table::TicFile)[0];
    assert!(col(file, "reporting_entity_name").is_some());
    assert!(col(file, "plan_sponsor_name").is_some());
    assert!(col(file, "schema_version").is_some());

    // Providers: business_name kept, network names list.
    let prov = rows(out.path(), Table::TicProvider);
    assert_eq!(
        col(&prov[0], "tin_business_name"),
        Some("Premier Medical Associates")
    );
    assert_eq!(
        col(&prov[0], "network_names"),
        Some("[Comprehensive Health Network]")
    );
}

#[test]
fn no_npi_example_keeps_tin_type_npi() {
    let out = tempfile::tempdir().unwrap();
    parse(
        "in-network-rates-no-npi.json",
        out.path(),
        &TicOptions::default(),
    );
    let prov = rows(out.path(), Table::TicProvider);
    assert_eq!(col(&prov[0], "tin_type"), Some("npi"));
}

#[test]
fn references_after_in_network_and_malformed_objects() {
    let out = tempfile::tempdir().unwrap();
    let s = parse(
        "edge/refs_after_in_network.json",
        out.path(),
        &TicOptions::default(),
    );
    assert_eq!(s.passes, 1, "no filters: single pass");
    assert_eq!(s.counts.in_network_items, 4);
    // rate0: 2 refs × 2 prices; inline: 1; undefined ref 99: 1 (kept, id
    // reported); element 2 (rates before header): 1.
    assert_eq!(s.counts.rates, 7);
    assert_eq!(s.counts.providers, 4); // group 1 (2 NPIs), group 2 (null NPI), inline
    let rates = rows(out.path(), Table::TicRate);

    let inline = rates
        .iter()
        .find(|r| col(r, "negotiated_type") == Some("fee schedule"))
        .unwrap();
    let inline_id = col(inline, "provider_group_id").unwrap();
    assert!(inline_id.starts_with("inline-"));
    assert_eq!(
        col(inline, "service_codes"),
        Some("[11]"),
        "string service_code"
    );
    let prov = rows(out.path(), Table::TicProvider);
    assert!(prov
        .iter()
        .any(|p| col(p, "provider_group_id") == Some(inline_id)
            && col(p, "npi") == Some("7777777777")));

    // Rates that came before their element's header got the header.
    let room = rates
        .iter()
        .find(|r| col(r, "billing_code") == Some("0120"))
        .unwrap();
    assert_eq!(col(room, "name"), Some("Room and board"));
    assert_eq!(col(room, "in_network_index"), Some("2"));
    assert_eq!(col(room, "negotiated_rate"), Some("2500"));

    let pct = rates
        .iter()
        .find(|r| col(r, "negotiated_type") == Some("percentage"))
        .unwrap();
    assert_eq!(
        col(pct, "negotiated_rate"),
        Some("80"),
        "percentage kept, not converted"
    );

    let rejects = rows(out.path(), Table::Rejects);
    let locs: Vec<_> = rejects
        .iter()
        .map(|r| col(r, "location").unwrap())
        .collect();
    assert_eq!(
        locs,
        vec![
            "$.in_network[0].negotiated_rates[3].negotiated_prices[0]",
            "$.in_network[0].negotiated_rates[4]",
            "$.in_network[1]",
            "$.in_network[3]",
            "$.provider_references",
        ],
        "{rejects:?}"
    );
    assert!(col(&rejects[4], "reason").unwrap().contains("99"));
}

#[test]
fn npi_filter_two_pass_with_late_references() {
    let out = tempfile::tempdir().unwrap();
    let opts = TicOptions {
        npis: ValueFilter::from_values(["2222222222"]),
        ..Default::default()
    };
    let s = parse("edge/refs_after_in_network.json", out.path(), &opts);
    assert_eq!(s.passes, 2);
    assert_eq!(s.counts.rates, 2, "only group 1 × 2 prices");
    assert_eq!(s.counts.providers, 1, "only the matching NPI row");
    let rates = rows(out.path(), Table::TicRate);
    assert!(rates
        .iter()
        .all(|r| col(r, "provider_group_id") == Some("1")));
    let prov = rows(out.path(), Table::TicProvider);
    assert_eq!(col(&prov[0], "npi"), Some("2222222222"));
}

#[test]
fn tin_filter() {
    let out = tempfile::tempdir().unwrap();
    let opts = TicOptions {
        tins: ValueFilter::from_values(["3333333333"]),
        ..Default::default()
    };
    let s = parse("edge/refs_after_in_network.json", out.path(), &opts);
    // group 2 only: rate0 (2 prices) + element 2 (1 price).
    assert_eq!(s.counts.rates, 3);
    let rates = rows(out.path(), Table::TicRate);
    assert!(rates
        .iter()
        .all(|r| col(r, "provider_group_id") == Some("2")));
}

#[test]
fn code_filter_skips_items_without_materializing() {
    let out = tempfile::tempdir().unwrap();
    let opts = TicOptions {
        codes: ValueFilter::from_values(["0120"]),
        ..Default::default()
    };
    let s = parse("edge/refs_after_in_network.json", out.path(), &opts);
    assert_eq!(s.passes, 1);
    assert_eq!(s.counts.rates, 1);
    assert_eq!(s.counts.in_network_filtered, 1);
    let rates = rows(out.path(), Table::TicRate);
    assert_eq!(col(&rates[0], "billing_code"), Some("0120"));
}

#[test]
fn gzip_with_json_extension() {
    let out = tempfile::tempdir().unwrap();
    let s = parse(
        "edge/refs_after_in_network_gz.json",
        out.path(),
        &TicOptions::default(),
    );
    assert_eq!(s.counts.rates, 7);
}

#[test]
fn remote_location_references_via_http() {
    let refs = br#"{"version": "1.0.0", "provider_groups": [
        {"npi": [4444444444], "tin": {"type": "ein", "value": "44-4444444"}}]}"#
        .to_vec();
    let server = TestServer::start(vec![("/providers/a.json".into(), refs)]);
    let url = server.url("/providers/a.json");
    let doc = format!(
        r#"{{"reporting_entity_name": "R", "reporting_entity_type": "t", "last_updated_on": "2026-01-01", "version": "1.0.0",
        "provider_references": [
            {{"provider_group_id": 10, "location": "{url}"}},
            {{"provider_group_id": 11, "location": "{url}"}}
        ],
        "in_network": [{{"negotiation_arrangement": "ffs", "name": "n", "billing_code_type": "CPT",
            "billing_code_type_version": "2026", "billing_code": "1", "description": "d",
            "negotiated_rates": [{{"provider_references": [10, 11], "negotiated_prices": [
                {{"negotiated_type": "negotiated", "negotiated_rate": 5, "expiration_date": "9999-12-31", "billing_class": "professional", "setting": "outpatient"}}]}}]}}]}}"#
    );
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("in.json");
    std::fs::write(&p, doc).unwrap();
    let out = dir.path().join("out");
    let opts = TicOptions {
        npis: ValueFilter::from_values(["4444444444"]),
        ..Default::default()
    };
    let s = parse_in_network_file(&Source::Path(p), &out, &opts).unwrap();
    assert_eq!(server.hits(), 1, "fetched once, cached by URL");
    assert_eq!(s.counts.providers, 2);
    assert_eq!(s.counts.rates, 2);
    assert_eq!(s.counts.rejects, 0);
}

#[test]
fn non_in_network_file_is_unknown_layout() {
    let out = tempfile::tempdir().unwrap();
    let err = parse_in_network_file(
        &Source::Path(fixture("table-of-contents-sample.json")),
        out.path(),
        &TicOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, TicError::UnknownLayout(_)), "{err}");
}

#[test]
fn deterministic_output() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    for d in [&a, &b] {
        parse(
            "edge/refs_after_in_network.json",
            d.path(),
            &TicOptions::default(),
        );
    }
    for t in [
        Table::TicFile,
        Table::TicProvider,
        Table::TicRate,
        Table::TicBundleCode,
        Table::Rejects,
    ] {
        let pa = mrf_core::read::table_parts(a.path(), t).unwrap();
        let pb = mrf_core::read::table_parts(b.path(), t).unwrap();
        assert_eq!(
            std::fs::read(&pa[0]).unwrap(),
            std::fs::read(&pb[0]).unwrap(),
            "{t:?}"
        );
    }
}

#[test]
fn bundle_and_capitation_codes() {
    for (f, arrangement, parent_code) in [
        (
            "in-network-rates-bundle-single-plan-sample.json",
            "bundle",
            "81.54",
        ),
        (
            "in-network-rates-capitation-single-plan-sample.json",
            "capitation",
            "27447",
        ),
    ] {
        let out = tempfile::tempdir().unwrap();
        parse(f, out.path(), &TicOptions::default());
        let codes = rows(out.path(), Table::TicBundleCode);
        let got: Vec<_> = codes
            .iter()
            .map(|r| {
                (
                    col(r, "in_network_index").unwrap(),
                    col(r, "billing_code").unwrap(),
                    col(r, "billing_code_type").unwrap(),
                    col(r, "billing_code_type_version").unwrap(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![("0", "27447", "CPT", "2020"), ("0", "27446", "CPT", "2020")],
            "{f}"
        );
        assert!(col(&codes[0], "description")
            .unwrap()
            .starts_with("Under Repair"));
        // The parent rate joins on (file_id, in_network_index).
        let rates = rows(out.path(), Table::TicRate);
        assert!(rates
            .iter()
            .all(|r| col(r, "negotiation_arrangement") == Some(arrangement)
                && col(r, "billing_code") == Some(parent_code)
                && col(r, "in_network_index") == Some("0")));
    }
}

#[test]
fn bundle_codes_follow_the_code_filter() {
    let out = tempfile::tempdir().unwrap();
    let opts = TicOptions {
        codes: ValueFilter::from_values(["99999"]),
        ..Default::default()
    };
    let s = parse(
        "in-network-rates-bundle-single-plan-sample.json",
        out.path(),
        &opts,
    );
    assert_eq!(s.counts.rates, 0);
    assert_eq!(s.counts.bundle_codes, 0);
}

#[test]
fn zipped_in_network_file() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("rates.zip");
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        z.start_file("in-network.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        z.write_all(&std::fs::read(fixture("edge/refs_after_in_network.json")).unwrap())
            .unwrap();
        z.finish().unwrap();
    }
    std::fs::write(&p, buf.into_inner()).unwrap();
    let out = dir.path().join("out");
    let s = parse_in_network_file(&Source::Path(p.clone()), &out, &TicOptions::default()).unwrap();
    assert_eq!(s.counts.rates, 7);
    assert_eq!(s.source_uri, format!("{}!/in-network.json", p.display()));
}

#[test]
fn stray_windows_1252_byte_is_decoded_not_fatal() {
    // Real UCare file: a raw 0x96 (cp1252 en dash) inside UTF-8 JSON.
    let out = tempfile::tempdir().unwrap();
    let s = parse("edge/cp1252_byte.json", out.path(), &TicOptions::default());
    assert_eq!(s.counts.rates, 7, "same rows as the clean fixture");
    let rates = rows(out.path(), Table::TicRate);
    assert_eq!(
        col(&rates[0], "description"),
        Some("Est patient \u{2013} level 3")
    );
    let rejects = rows(out.path(), Table::Rejects);
    let fix = rejects
        .iter()
        .find(|r| col(r, "location") == Some("$"))
        .expect("reject for the repaired byte");
    assert!(col(fix, "reason")
        .unwrap()
        .starts_with("1 byte(s) of invalid UTF-8"));
}
