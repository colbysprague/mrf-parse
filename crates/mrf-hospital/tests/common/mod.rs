//! Shared helpers for hospital integration tests.
#![allow(dead_code)] // each test binary uses a different subset

use std::path::{Path, PathBuf};

use mrf_core::{read::read_rows, Source, Table};
use mrf_hospital::{parse_file, HospitalOptions, HospitalSummary};

pub fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/hospital")
        .join(rel)
}

pub fn parse(rel: &str, out: &Path) -> HospitalSummary {
    parse_with(rel, out, &HospitalOptions::default())
}

pub fn parse_with(rel: &str, out: &Path, opts: &HospitalOptions) -> HospitalSummary {
    parse_file(&Source::Path(fixture(rel)), out, opts)
        .unwrap_or_else(|e| panic!("parse {rel}: {e}"))
}

/// Table rows as `column name -> value` maps, for readable assertions.
pub fn rows(out: &Path, table: Table) -> Vec<Row> {
    let schema = table.schema();
    read_rows(out, table)
        .unwrap()
        .into_iter()
        .map(|values| Row {
            names: schema.fields().iter().map(|f| f.name().clone()).collect(),
            values,
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct Row {
    names: Vec<String>,
    pub values: Vec<Option<String>>,
}

impl Row {
    pub fn get(&self, col: &str) -> Option<&str> {
        let i = self
            .names
            .iter()
            .position(|n| n == col)
            .unwrap_or_else(|| panic!("no column {col}"));
        self.values[i].as_deref()
    }
}
