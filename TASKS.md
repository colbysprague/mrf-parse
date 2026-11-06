# TASKS

Work one task at a time. After each task: `cargo test --workspace` (and from task 2 on, `make lint`) must pass before checking it off.

- [x] **1. Workspace skeleton, input opening, SHA-256, file_id**
  - Cargo workspace with crates `mrf-core`, `mrf-hospital`, `mrf-tic`, `mrf-cli` (binary `mrf`); `Makefile` with `test`, `lint`, `bench` targets; `LICENSE-MIT`, `LICENSE-APACHE`.
  - `mrf_core::input::open(source)` accepts a local path or `https://`/`http://` URL and returns a streaming `BufRead`.
  - Gzip detected by magic bytes (`1f 8b`), never by extension; plain input passes through untouched.
  - HTTP bodies are streamed (no full download into memory).
  - `mrf_core::ids::sha256_of(source)` streams the raw bytes; `file_id` = first 16 hex chars of the SHA-256.
  - Tests: plain file, gzip file with `.json` extension, gzip file with no extension, empty file, missing file → error, local HTTP server serving plain + gzip, known SHA-256 vector.

- [x] **2. Parquet sink + `rejects` table**
  - Generic batching Parquet writer (snappy) writing `out/<table>/file_id=<id>/part-0.parquet`; flushes every N rows so memory stays bounded.
  - Arrow schemas for every table defined once in `mrf-core::schema`.
  - `RejectSink` records `file_id, location, reason, raw_snippet` (snippet truncated to 1KB on a char boundary).
  - Deterministic output: no timestamps/random values in files; same input → identical bytes (test writes twice and compares).

- [x] **3. Hospital header tokenizer/mapper**
  - Normalizes header tokens: trim around `|`, case-insensitive, collapse whitespace, strip BOM.
  - Maps metadata headers (rows 1–2) incl. `license_number|<state>` and attestation-by-prefix.
  - Maps data headers: `code|n`, `code|n|type`, tall standard-charge columns, wide `standard_charge|<payer>|<plan>|<field>`, `median_amount|<payer>|<plan>` etc. (payer/plan case and spacing preserved raw).
  - Unknown columns are tolerated and reported (logged), never fatal.
  - Unit tests covering spaced vs unspaced pipes, v2 and v3 names, unknown columns.

- [x] **4. Hospital CSV tall reader**
  - Reads rows 1–2 as file metadata, row 3 as data header; writes `hospital_file`, `hospital_item`, `hospital_item_code`, `hospital_charge`.
  - Money parsing: strip `$`/`,`; empty → null; unparseable → field null + reject, row kept.
  - Multi-value metadata cells split on `|`.
  - Items keyed deterministically; tall rows for the same item/payer/plan grouped correctly; items with no payer get one charge row with null payer.
  - Malformed row mid-file → reject, parsing continues. BOM, CRLF, extra/missing optional columns handled.
  - Tests on CMS v2 + v3 tall examples and edge-case fixtures.

- [x] **5. Hospital CSV wide reader (unpivot)**
  - Unpivots payer/plan columns into one `hospital_charge` row per item × payer × plan (only where the payer/plan has any value) plus a null-payer row when none.
  - Tests on CMS v2 + v3 wide examples.

- [x] **6. Hospital JSON reader (streaming)**
  - Streams `standard_charge_information[]` one element at a time (never whole-file deserialize); metadata may appear before or after the array.
  - Handles `modifier_information`, `general_contract_provisions`, v2 and v3 field names.
  - Malformed elements → reject with JSON path; continue.
  - Tests on CMS v2 + v3 JSON examples.

- [x] **7. Layout detection + equivalence tests**
  - Detect `csv_tall` / `csv_wide` / `json` from content (first non-whitespace byte, header shape), never from file name.
  - Unknown layout fails the file with a clear error.
  - Declared `version` recorded; mismatch with detected shape logged, not fatal.
  - Equivalence test: v3 tall, wide, JSON produce equal sorted sets of item / item_code / charge rows (ignoring item_id); same for v2.

- [x] **8. TiC table-of-contents reader**
  - Streams `reporting_structure[]`; emits in-network file URLs with plan metadata; `--plan-filter` substring match on plan name.
  - Tests on CMS TOC examples.

- [x] **9. TiC provider-reference handling**
  - Inline `provider_groups`, top-level `provider_references[]` with inline groups, and remote `location` URLs (fetched, cached by URL).
  - Inline groups get deterministic synthetic `provider_group_id` (hash of contents).
  - Writes `tic_provider` (one row per NPI; NPI null when none). NPI/TIN filters applied.
  - Tests incl. local HTTP server for `location`.

- [x] **10. TiC in-network streaming reader**
  - Streams `in_network[]` element by element; writes `tic_file`, `tic_rate`.
  - Filters (NPIs, TINs, billing codes) applied during parse; filtered rows never materialized.
  - Out-of-order strategy for `provider_references` after `in_network` (documented in code).
  - Every `negotiated_type` and `tin.type` preserved raw. Malformed objects → rejects with JSON path.
  - Tests on all CMS in-network JSON examples + reference-after-in_network fixture.

- [x] **11. Bundles/capitation table**
  - `tic_bundle_code` (file_id, in_network_index, billing_code, billing_code_type, description) from `bundled_codes` / `covered_services`.
  - Tests on CMS bundle + capitation examples.

- [x] **12. CLI**
  - `mrf hospital`, `mrf tic`, `mrf toc`; parallel per-file processing (rayon), single-threaded within a file.
  - Per-file summary: layout, rows per table, rejects, elapsed, peak RSS.
  - One file failing does not stop the others; non-zero exit if any file failed.

- [x] **13. `validate` command**
  - Reads an output dir; prints per table row counts, per-column null rates, reject counts per file.

- [x] **14. Memory test + benchmarks**
  - `tests/gen/` script produces a synthetic multi-GB TiC file; test parses it and asserts peak RSS < 512MB (ignored by default, run via `make memtest`).
  - Criterion benches for each reader on mid-size fixtures; throughput (MB/s, rows/s) reported in README.

- [x] **15. README**
  - Usage, every table and column documented, design decisions (streaming, out-of-order refs, ids, determinism), known gaps; NOTICE crediting ported sources if any.
