//! End-to-end tests of the `mrf` binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture(rel: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(rel)
        .display()
        .to_string()
}

fn mrf(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mrf"))
        .args(args)
        .output()
        .expect("run mrf")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn out_dir(d: &tempfile::TempDir) -> String {
    let p: PathBuf = d.path().join("out");
    p.display().to_string()
}

#[test]
fn hospital_parallel_with_one_failure() {
    let d = tempfile::tempdir().unwrap();
    let out = out_dir(&d);
    let tall = fixture("hospital/v3/tall.csv");
    let json = fixture("hospital/v3/example.json");
    let o = mrf(&[
        "hospital",
        &tall,
        "/no/such/file.csv",
        &json,
        "--out",
        &out,
        "--jobs",
        "2",
    ]);
    assert_eq!(o.status.code(), Some(1), "non-zero when any file fails");
    let s = stdout(&o);
    // Summary is in input order regardless of completion order.
    let ok_tall = s.find(&format!("OK    {tall}")).expect(&s);
    let fail = s.find("FAIL  /no/such/file.csv").expect(&s);
    let ok_json = s.find(&format!("OK    {json}")).expect(&s);
    assert!(ok_tall < fail && fail < ok_json, "{s}");
    assert!(s.contains("layout=csv_tall") && s.contains("layout=json"));
    assert!(s.contains("charges=45"));
    assert!(s.contains("peak_rss="));
    assert!(s.contains("3 file(s), 1 failed"));
    // The failure did not stop the other files' output.
    assert!(Path::new(&out).join("hospital_item").is_dir());
}

#[test]
fn tic_with_filters_and_inputs_from_file() {
    let d = tempfile::tempdir().unwrap();
    let out = out_dir(&d);
    let npis = d.path().join("npis.txt");
    std::fs::write(&npis, "# one NPI\n2222222222\n").unwrap();
    let list = d.path().join("inputs.txt");
    std::fs::write(
        &list,
        format!("{}\n", fixture("tic/edge/refs_after_in_network.json")),
    )
    .unwrap();
    let o = mrf(&[
        "tic",
        "--inputs-from",
        &list.display().to_string(),
        "--npis",
        &npis.display().to_string(),
        "--out",
        &out,
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let s = stdout(&o);
    assert!(s.contains("passes=2"), "{s}");
    assert!(s.contains("rates=2"), "{s}");
}

#[test]
fn toc_prints_distinct_urls_with_plan_filter() {
    let toc = fixture("tic/table-of-contents-sample.json");
    let o = mrf(&["toc", &toc]);
    assert!(o.status.success());
    assert_eq!(stdout(&o).lines().count(), 3);
    let o = mrf(&["toc", &toc, "--plan-filter", "CHIP"]);
    assert_eq!(
        stdout(&o).trim(),
        "https://www.some_site.com/files/chip-in-network-file.json"
    );
}

#[test]
fn validate_reports_tables_nulls_and_rejects() {
    let d = tempfile::tempdir().unwrap();
    let out = out_dir(&d);
    let edge = fixture("hospital/edge/tall_edge.csv");
    assert!(mrf(&["hospital", &edge, "--out", &out]).status.success());
    let o = mrf(&["validate", &out]);
    assert!(o.status.success());
    let s = stdout(&o);
    assert!(
        s.contains("hospital_charge: 4 rows in 1 file partition(s)"),
        "{s}"
    );
    assert!(s.contains("payer_name_raw"), "{s}");
    assert!(s.contains("rejects per file:"), "{s}");
    assert!(
        s.lines()
            .any(|l| l.contains("file_id=") && l.trim_end().ends_with(" 3")),
        "{s}"
    );

    let missing = mrf(&["validate", "/no/such/dir"]);
    assert!(!missing.status.success());
}

#[test]
fn zip_input_expands_and_warnings_name_the_file() {
    use std::io::Write;
    let d = tempfile::tempdir().unwrap();
    let out = out_dir(&d);
    let zip_path = d.path().join("download");
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        for (name, rel) in [
            ("a_tall.csv", "hospital/v3/tall.csv"),
            ("b_extra.csv", "hospital/edge/tall_extra_cols.csv"),
        ] {
            z.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            z.write_all(&std::fs::read(fixture(rel)).unwrap()).unwrap();
        }
        z.finish().unwrap();
    }
    std::fs::write(&zip_path, buf.into_inner()).unwrap();
    let zp = zip_path.display().to_string();
    let o = mrf(&["hospital", &zp, "--out", &out]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let s = stdout(&o);
    assert!(s.contains(&format!("OK    {zp}!/a_tall.csv")), "{s}");
    assert!(s.contains(&format!("OK    {zp}!/b_extra.csv")), "{s}");
    assert!(s.contains("2 file(s), 0 failed"), "{s}");

    // The unknown-column warning carries the entry it came from.
    let err = String::from_utf8_lossy(&o.stderr);
    let warn = err
        .lines()
        .find(|l| l.contains("count_of_compared_rates"))
        .expect(&err);
    assert!(warn.contains("b_extra.csv"), "{warn}");
}
