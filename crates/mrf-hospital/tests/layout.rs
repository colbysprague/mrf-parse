mod common;

use common::{fixture, parse};
use mrf_core::Source;
use mrf_hospital::{parse_file, HospitalError, HospitalOptions, Layout};

#[test]
fn layout_comes_from_content_not_name() {
    let dir = tempfile::tempdir().unwrap();
    // Copy each example under a misleading name.
    for (src, name, want) in [
        ("v3/tall.csv", "rates.json", Layout::CsvTall),
        ("v3/wide.csv", "tall.txt", Layout::CsvWide),
        ("v3/example.json", "wide.csv", Layout::Json),
    ] {
        let p = dir.path().join(name);
        std::fs::copy(fixture(src), &p).unwrap();
        let s = parse_file(
            &Source::Path(p),
            &dir.path().join("out"),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(s.layout, want, "{src} as {name}");
    }
}

#[test]
fn unknown_layout_fails_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let cases: [(&str, &[u8]); 3] = [
        ("empty", b""),
        ("short", b"a,b\n1,2\n"),
        ("not_hospital", b"x,y\n1,2\nfoo,bar\n1,2\n"),
    ];
    for (name, body) in cases {
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        let err = parse_file(
            &Source::Path(p),
            &dir.path().join("out"),
            &HospitalOptions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, HospitalError::UnknownLayout(_)),
            "{name}: {err}"
        );
    }
}

#[test]
fn version_mismatch_is_logged_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let body = std::fs::read_to_string(fixture("v3/tall.csv"))
        .unwrap()
        .replacen(",3.0.0,", ",2.0.0,", 1);
    let p = dir.path().join("mislabeled.csv");
    std::fs::write(&p, body).unwrap();
    let s = parse_file(
        &Source::Path(p),
        &dir.path().join("out"),
        &Default::default(),
    )
    .unwrap();
    assert_eq!(
        s.mrf_version.as_deref(),
        Some("2.0.0"),
        "declared value recorded"
    );
    assert!(s.version_mismatch.unwrap().contains("v3"));
    assert_eq!(s.counts.charges, 45, "file still parsed");
}

#[test]
fn cms_examples_have_no_version_mismatch() {
    for f in [
        "v3/tall.csv",
        "v3/wide.csv",
        "v3/example.json",
        "v2/tall.csv",
        "v2/wide.csv",
        "v2/example.json",
    ] {
        let out = tempfile::tempdir().unwrap();
        let s = parse(f, out.path());
        assert_eq!(s.version_mismatch, None, "{f}");
    }
}
