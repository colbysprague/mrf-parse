# mrf-parse

Rust parsers for US price transparency machine-readable files (MRFs):

- **Hospital** MRFs (45 CFR 180): CSV *tall*, CSV *wide*, and JSON, CMS template v2.x and v3.0.
- **Payer** Transparency in Coverage (TiC) MRFs: table of contents, in-network rates, and provider-reference files (JSON only).

The parsers read raw files and write normalized Parquet. They do no matching, reconciliation, or rate conversion. Those are later pipeline steps.

Every reader streams its input, so memory stays flat regardless of input size: a 2 GB in-network file parses with a 40 MB peak RSS (see [Performance](#performance)).

## Usage

```sh
make build            # target/release/mrf

mrf hospital <input>... --out <dir> [--codes codes.txt]
mrf tic      <input>... --out <dir> [--npis npis.txt] [--tins tins.txt] [--codes codes.txt]
mrf toc      <toc-input> [--plan-filter <substring>]     # prints in-network URLs
mrf validate <dir>                                         # rows, null rates, rejects per file
```

- **Inputs** are local paths or `http(s)://` URLs: plain, gzip, or a zip archive. Gzip (`1f 8b`) and zip (`PK\x03\x04`) are detected from the first bytes, never from the extension; several hospital vendors serve zips from extension-less URLs. Each file inside a zip is parsed as its own input, with its own `file_id` and a `source_uri` of `<archive>!/<entry name>`.
- **Filter files** list one value per line; blank lines and `#` comments are ignored. Matching trims whitespace and ignores ASCII case.
- `--inputs-from <file>` (or `-` for stdin) reads more inputs, one per line. This lets you fan out a TOC without hitting argument-length limits:

  ```sh
  mrf toc https://payer.example/toc.json --plan-filter "acme" \
    | mrf tic --inputs-from - --out out --npis npis.txt
  ```
- `--jobs N` sets how many files run in parallel (default: CPU count). Each file is parsed single-threaded.
- `--tmp-dir` sets where URL downloads are staged.

`hospital` and `tic` print one progress line per file to stderr as it finishes, then a summary in input order to stdout. Each file's summary includes its layout/version, rows per table, rejects, input size, elapsed time, and peak RSS. Warnings carry the file they came from (`file{src=…}: …`). A file that fails is reported and the run continues; the exit status is `1` if any file failed and `2` for usage errors. Log verbosity follows `RUST_LOG` (default `warn`).

## Output

Parquet with snappy compression, one directory per table, partitioned by source file (Hive-style, so DuckDB, Polars, and Spark read it as a dataset):

```
out/<table>/file_id=<id>/part-0.parquet
```

- `file_id` is the first 16 hex characters of the SHA-256 of the input's **raw** bytes. For gzip files that's the compressed bytes, which is what `sha256sum` reports.
- Each partition is written to `part-0.parquet.tmp` and renamed when finished, so a failed file never leaves a valid-looking partial file.
- Re-running a file replaces its partitions.
- Every parsed file gets a partition in every table it touches, even with zero rows, so "parsed, nothing found" is distinguishable from "not parsed".

### Conventions

- **Text is raw.** Values are trimmed of surrounding whitespace and otherwise kept as written: payer and plan names, codes, enum values like `negotiated_type`, and dates. Empty strings become null.
- **Money and rates** are `Float64`. `$` and `,` are stripped (`$1,234.50` → 1234.5), and a trailing `%` is allowed. Text that doesn't parse is **never** coerced to 0: the field is null and a `rejects` row records it, while the row itself is kept.
- **Identifiers** (NPI, TIN, codes) are `Utf8`, even where JSON writes them as numbers.
- **Lists** are `List<Utf8>`.

### Hospital tables

**`hospital_file`**: one row per input file.

| column | type | notes |
|---|---|---|
| file_id | utf8 | partition key |
| source_uri | utf8 | path or URL as given |
| sha256 | utf8 | of the raw bytes |
| layout | utf8 | `csv_tall`, `csv_wide`, or `json` (detected from content) |
| mrf_version | utf8 | declared `version`, as written |
| last_updated_on | utf8 | as written (CMS allows ISO and M/D/YYYY) |
| hospital_name | utf8 | |
| location_names | list | v3 `location_name`, v2 `hospital_location`; CSV cells split on `\|` |
| hospital_addresses | list | split on `\|` in CSV |
| license_number | utf8 | |
| license_state | utf8 | from the `license_number\|<state>` header (CSV) or `license_information.state` (JSON) |
| type_2_npis | list | v3; split on `\|` in CSV |
| attester_name | utf8 | v3 |
| attestation | bool | the CSV attestation/affirmation column (its header is the statement itself), or JSON `confirm_attestation` / `confirm_affirmation` |
| general_contract_provisions | utf8 | CSV: the cell text. JSON: the `[{payer_name, plan_name, provisions}]` array as JSON text |
| financial_aid_policy | utf8 | optional CMS field (added) |
| extra | utf8 | metadata columns / top-level JSON keys not in the CMS spec, as a JSON object (see [Extra columns](#extra-columns)) |

**`hospital_item`**: one row per distinct service line.

| column | type | notes |
|---|---|---|
| item_id | utf8 | stable hash of file_id + every item field + codes |
| file_id | utf8 | |
| description | utf8 | |
| setting | utf8 | `inpatient`, `outpatient`, or `both`, as written |
| drug_unit_of_measurement | float64 | |
| drug_type_of_measurement | utf8 | |
| modifiers | utf8 | raw; multiple modifiers stay `\|`-joined (`50\|62`). JSON arrays are joined the same way |
| billing_class | utf8 | optional CMS field (added) |
| extra | utf8 | JSON only: item-level keys not in the CMS spec (part of `item_id`) |

Tall CSV repeats the item on every payer row; those rows share one `item_id` and the item is written once. In JSON, `setting` and `modifier_code` live on each `standard_charges[]` entry, so each entry is its own item, matching the CSV layouts where setting is a per-row column. JSON `modifier_information[]` entries become items with `modifiers` set and no codes.

**`hospital_item_code`**: one row per code on an item.

| column | type | notes |
|---|---|---|
| item_id | utf8 | |
| code | utf8 | from CSV `code\|n`, or JSON `code_information[].code` |
| code_type | utf8 | from CSV `code\|n\|type`, or JSON `.type` (CPT, NDC, RC, MS-DRG, ...) |

**`hospital_charge`**: one row per item × payer × plan, plus one row with a null payer for items that have no payer rates.

| column | type | notes |
|---|---|---|
| item_id | utf8 | |
| payer_name_raw | utf8 | exactly as written. In wide CSV it comes from the column header |
| plan_name_raw | utf8 | raw. If a wide header has extra segments (`payer\|plan\|product\|field`), they stay in the plan joined by `\|` |
| gross | float64 | `standard_charge\|gross` / `gross_charge` |
| discounted_cash | float64 | |
| deid_min | float64 | `standard_charge\|min` / `minimum` (de-identified minimum) |
| deid_max | float64 | `standard_charge\|max` / `maximum` |
| negotiated_dollar | float64 | |
| negotiated_percentage | float64 | kept as a percentage, not converted |
| negotiated_algorithm | utf8 | |
| methodology | utf8 | `case rate`, `fee schedule`, `percent of total billed charges`, `per diem`, `other`, ... |
| median_amount | float64 | v3; null for v2 |
| p10_amount | float64 | v3 `10th_percentile` |
| p90_amount | float64 | v3 `90th_percentile` |
| allowed_count | utf8 | v3 `count`; text, because the spec allows `"1 through 10"` |
| estimated_amount | float64 | v2 only (added); null for v3 |
| additional_payer_notes | utf8 | wide `additional_payer_notes\|payer\|plan`, JSON `payers_information[].additional_payer_notes` |
| additional_generic_notes | utf8 | |
| extra | utf8 | CSV: cells under columns not in the CMS spec, for the row this charge came from. JSON: unknown keys on the `standard_charges[]` entry, then on the payer entry |

Item-level charges (`gross`, `discounted_cash`, `deid_min`, `deid_max`, generic notes) are copied onto every payer row of the item, as tall CSV already does. In wide CSV a payer/plan only gets a row if at least one of its columns has a value. Tall rows are never merged: the CMS v2 tall example lists the same item/payer/plan three times with different algorithms, and all three are kept.

Tall CSV has no payer-notes column, so a tall file's notes stay in `additional_generic_notes` even when they're payer-specific; the CMS tall examples work this way. Consumers that want "the note for this rate" should use `coalesce(additional_payer_notes, additional_generic_notes)`.

### Extra columns

Real files carry columns the CMS spec doesn't define. Gundersen's vendor, for example, adds `count_of_compared_rates` (alongside the CMS `count`), `footnote`, and an `as_of_date` metadata column. Instead of dropping them, each hospital table has an `extra` column holding them as a JSON object of raw name → raw value, e.g. `{"count_of_compared_rates":"17","footnote":"Subject to stoploss provisions"}`.

- Keys keep file order. A repeated header gets a `#2`, `#3`, ... suffix instead of overwriting.
- Empty cells add nothing; a row with no extra values has `extra = null`.
- CSV cells are strings; JSON values keep their JSON type.
- Each unknown column is also logged once per file, naming where it was kept.
- In wide CSV an unknown column is row-level, so it is copied onto every payer row the CSV row produces.

Query them with any JSON function, e.g. DuckDB `extra->>'footnote'`.

### TiC tables

**`tic_file`**: one row per in-network file.

| column | type | notes |
|---|---|---|
| file_id, source_uri, sha256 | utf8 | as for hospital |
| schema_version | utf8 | top-level `version` |
| last_updated_on | utf8 | |
| reporting_entity_name | utf8 | raw |
| reporting_entity_type | utf8 | |
| plan_name, issuer_name, plan_sponsor_name | utf8 | raw; null in multi-plan files, where plans are listed in the TOC |
| plan_id_type | utf8 | `ein` or `hios` |
| plan_id | utf8 | |
| plan_market_type | utf8 | `group` or `individual` |

**`tic_provider`**: provider groups exploded to one row per NPI.

| column | type | notes |
|---|---|---|
| file_id | utf8 | |
| provider_group_id | utf8 | from `provider_references[].provider_group_id`, or `inline-<hash>` for groups listed inline on a rate |
| network_names | list | `network_name` |
| tin_type | utf8 | `ein` or `npi`, raw |
| tin_value | utf8 | |
| npi | utf8 | null when the entry lists no NPIs |
| tin_business_name | utf8 | `tin.business_name` (added) |

**`tic_rate`**: one row per negotiated price × provider group.

| column | type | notes |
|---|---|---|
| file_id | utf8 | |
| in_network_index | int64 | position in `in_network[]` (added; joins to `tic_bundle_code`) |
| provider_group_id | utf8 | joins `tic_provider` on (file_id, provider_group_id) |
| negotiation_arrangement | utf8 | `ffs`, `bundle`, or `capitation` |
| name | utf8 | |
| billing_code, billing_code_type, billing_code_type_version | utf8 | |
| description | utf8 | |
| severity_of_illness | utf8 | optional TiC field (added) |
| setting | utf8 | |
| negotiated_type | utf8 | `negotiated`, `derived`, `fee schedule`, `percentage`, or `per diem`, raw |
| negotiated_rate | float64 | as written; a percentage stays a percentage |
| expiration_date | utf8 | |
| billing_class | utf8 | `professional`, `institutional`, or `both` |
| service_codes | list | `service_code`; a single string becomes a one-element list |
| billing_code_modifiers | list | `billing_code_modifier` |
| additional_information | utf8 | |

**`tic_bundle_code`**: codes inside bundles and capitation arrangements.

| column | type | notes |
|---|---|---|
| file_id | utf8 | |
| in_network_index | int64 | the parent `in_network[]` element |
| billing_code, billing_code_type | utf8 | from `bundled_codes[]` / `covered_services[]` |
| billing_code_type_version | utf8 | added |
| description | utf8 | |

### Shared

**`rejects`**: malformed rows, objects, and fields. Parsing always continues past them.

| column | type | notes |
|---|---|---|
| file_id | utf8 | |
| location | utf8 | CSV: `row N` or `row N, column <field>`. JSON: a path such as `$.in_network[12].negotiated_rates[3].negotiated_prices[0]` |
| reason | utf8 | what was wrong and what was done (e.g. "field set to null, row kept") |
| raw_snippet | utf8 | offending text, truncated to 1 KB on a UTF-8 boundary |

Only unreadable input (I/O errors, invalid JSON syntax) or an unknown layout fails a whole file.

### Columns added beyond the original spec

Each of these is a field in the CMS spec that would otherwise be silently dropped: `hospital_file.financial_aid_policy`, `hospital_item.billing_class`, `hospital_charge.estimated_amount` (v2), `tic_provider.tin_business_name`, `tic_rate.severity_of_illness`, and `tic_bundle_code.billing_code_type_version`. `tic_rate.in_network_index` was added so bundle codes can be joined to their rates.

## Design decisions

**Streaming.** No reader ever holds a whole file:

- **CSV** is read one record at a time with a reused buffer.
- **JSON** (hospital and TiC) drives `serde_json` with custom `Visitor`s instead of deserializing a document: the top-level object is walked key by key, and large arrays are pulled one element at a time (`crates/mrf-tic/src/json.rs`).
- **TiC `in_network[]` elements** are visited **key by key** too, because one billing code can carry millions of `negotiated_rates`. Once an element's header fields (`billing_code`, `name`, ...) have been read, its rates stream straight to Parquet. If `negotiated_rates` comes before the header (legal JSON, rare in practice), rates are buffered and spill to an anonymous temp file after 4096 entries, then replay when the element ends.
- **Parquet writers** flush every 8192 rows **or** every 2 MB of pending text, whichever comes first, with 65,536-row row groups. The byte cap matters: real UHC files carry 1–30 KB `additional_information` notes per price, and without it each 8192-row batch grows ~15 MB string buffers. The macOS allocator keeps freed blocks of that size resident, which showed up as about 900 MB peak RSS for one writer; with the cap it stays around 50 MB. `make memtest` generates 1.5 KB notes per price to cover this.

Memory per file is roughly one batch plus one row group per table.

**Input staging.** Every row carries `file_id`, so the hash must be known before parsing starts. Local files are hashed in one sequential pass first. URL inputs are streamed to a temp file once, hashing as they download, then parsed from disk. Memory stays flat, nothing is downloaded twice, and a URL input needs disk space equal to its compressed size.

Zip archives are opened from that local copy (zip's directory is at the end of the file, so it needs a seekable file). Each entry is extracted to a temp file while being hashed, so `file_id` is the hash of the entry's own bytes: the same CSV gets the same `file_id` zipped or not. Extraction streams; disk use is the entry's uncompressed size.

**Out-of-order `provider_references` (two-pass, only when needed).** `tic_rate.provider_group_id` is just the id the rate references, so **without NPI/TIN filters** rates are written whether or not their reference has been seen yet. That's one pass; ids still undefined at the end of the file are reported in `rejects`. **With NPI/TIN filters**, whether a rate is kept depends on its groups, so there is a reference-only first pass: it skips everything else with `IgnoredAny` (which tokenizes without allocating) and stops as soon as `provider_references` has been read. When references come first, as is common, that pass ends after a few MB.

Two passes won over "buffer unresolved rates to temp Parquet and join afterwards" for three reasons:
- The input is already local, so the second pass never re-downloads.
- Rows for filtered-out rates are never built or written. Each rate's JSON is still parsed to read its provider references; `--codes` skips non-matching elements without even that.
- There is no join step or temp data to manage.

The cost is re-reading the file when references come last. In the bench, a filtered 68 MB file with references last takes 0.84 s in total, against 0.96 s unfiltered: the extra pass costs less than writing the rows that filtering skips. See `crates/mrf-tic/src/in_network.rs`.

**Remote provider references.** Remote `location` URLs are fetched on first use and cached by URL for the whole run, shared across worker threads. Failed fetches are cached too, so a dead URL isn't retried for every reference to it, and each failure is a reject rather than a failed file.

**Filtering during parse.** For TiC, the `--codes` filter is checked against an element's `billing_code` before any of its rates are materialized; non-matching rates are skipped with `IgnoredAny`. NPI/TIN filters are applied per `tic_provider` row. A group passes if any of its rows passes, and only rates for passing groups are written. For hospital files, `--codes` keeps items with at least one matching code.

**Stable ids and determinism.**
- `item_id` and inline `provider_group_id` are SHA-256 based and length-prefixed, so `["ab","c"]` and `["a","bc"]` hash differently.
- Inline group ids hash the group's canonical JSON with object keys sorted, so the id doesn't depend on key order in the file or on serde_json's `preserve_order` feature.
- Rows are written in file order.
- Parquet output has no timestamps or random values.

Running the same input twice gives byte-identical files; tests check this.

**Hospital layout detection** works from content, never file names:
- The first non-whitespace byte after an optional BOM is `{` → JSON.
- Otherwise the file is CSV. A row-3 `payer_name` column means tall; `standard_charge|<payer>|<plan>|…` style headers mean wide. A file with only item-level charge columns is read as wide (it has no payers).
- No `description`/`code|n` column means an unknown layout, which fails the file.

The declared `version` is recorded as written. If it disagrees with the file's shape (v2 markers like `hospital_location`/`estimated_amount` vs v3 markers like `location_name`/`median_amount`), a warning is logged and returned in the summary, but the file is still parsed.

**Header tolerance.** Header keywords are matched after normalizing case, spacing around `|`, `-`/space vs `_`, a BOM, and literal quotes. This covers `code | 1 | type` and `code|1|type`, `standard_charges|…`, `…|algorithm`, `standard_charge||payer|plan|…`, and wide headers with no plan segment. Payer, plan, and state tokens are only trimmed, never re-cased. Unknown columns are logged and ignored.

**Encoding.** A CSV row that isn't valid UTF-8 is decoded as Windows-1252 and kept, with a reject noting the fallback. Even the CMS v2 wide example contains a cp1252 em dash.

**Errors don't cross serde.** Visitor methods can only return serde errors, so errors from our own code (I/O, sinks) are stashed in a side slot and the deserializer is unwound with a sentinel error. The caller checks the slot first, so the real error is reported.

## Testing

```sh
make test      # unit + integration tests (fixtures, equivalence, CLI end-to-end)
make lint      # rustfmt --check + clippy -D warnings
make memtest   # synthetic multi-GB TiC parse, asserts peak RSS < 512 MB
make bench     # criterion benchmarks for every reader
```

- **Fixtures** (`tests/fixtures/`):
  - every CMS hospital example (v2 + v3 × tall/wide/JSON) and every CMS TiC JSON example;
  - hand-made edge cases: BOM, CRLF, spaced pipes, `$1,234.50`, empty and `N/A` money, unknown and missing columns, a malformed row mid-file, gzip under a `.json` name, metadata after items, a truncated JSON file, `provider_references` after `in_network`, inline provider groups, rates before their element's header, malformed objects, and an undefined reference id.
- **Remote provider references** are tested against a tiny std-only HTTP server (`mrf_core::testutil`), including caching, which is checked by counting requests.
- **Equivalence** (`crates/mrf-hospital/tests/equivalence.rs`): the CMS tall, wide, and JSON examples must produce the same items, codes, and charges as sorted multisets. The CMS files don't fully agree with each other, so the test normalizes two layout-inherent differences (tall's lack of a payer-notes column, and JSON modifiers' lack of rate fields) and then asserts that what still differs is **exactly** a documented list of upstream discrepancies. Examples: v3 tall says `min=150` where wide/JSON say 125; v2 wide uses `Platform_Health_Insurance`; v2 tall splits a per-diem into day ranges. Any parser change that alters output fails the test.
- **Memory**: `tests/gen/gen_tic.rs` writes a synthetic in-network file with large elements, some rates-before-header elements, and references last. A 4 MB version runs in `make test` and checks exact row counts in both one-pass and filtered two-pass modes; `make memtest` runs the multi-GB version (`MRF_MEMTEST_GB`, default 2). To make a file by hand: `cargo run --release -p mrf-tic --example gen_tic -- out.json 5`.

### Real files

Five real hospital MRFs (2026), from five systems and four vendors, have been checked: an ElevatePFS wide CSV (NY, 75 MB), Carilion Giles (tall, 37 MB), Sanford Canton-Inwood (tall, CRLF, 36 MB), and Gundersen Palmer and Boscobel (tall, served as zips, 61 and 79 MB). An independent stdlib-Python parse of the raw CSVs matched our output exactly on all five: row counts, distinct payers, and the sum and non-null count of every price column (including the wide file's 25 payers with no plan segment). All parse with zero rejects in under a second each, at about 75 MB peak RSS. 
Real TiC files from UnitedHealthcare's public index (`transparency-in-coverage.uhc.com`, November 2026) were checked:
- **Tables of contents:** a 2 KB one, and a 15.5 MB Oxford one covering 52,635 plans.
- **In-network files:** five, from 984 bytes to 1.5 GB uncompressed (gzip), including state exchange networks for North Carolina and Colorado.

An independent stdlib-Python pass over the raw JSON matched our output exactly on every file: TOC URLs (same order), `in_network` element counts, `tic_rate` rows, the sum of `negotiated_rate`, `tic_provider` rows, and rows per `negotiated_type`. A real NPI filter (two passes) also matched Python's expected subset exactly. The 1.5 GB file parses in 5.2 s with zero rejects at 54 MB peak RSS.

Three more payers were then checked, chosen because they use different publishing software:
- **CareFirst:** BlueCard files from Highmark and Anthem as well as CareFirst's own.
- **HMSA** (Hawaii's Blue plan): files from Anthem California and Highmark.
- **UCare** (Minnesota): its own JSON.

That covers six TOCs and twelve in-network files: TiC schema v1.2.0 and v2.0.0, gzip and plain JSON, up to 2.46 GB uncompressed and 14.1M provider rows in one file. A streaming check with Python's `ijson` (C backend) matched our output exactly on all twelve: elements, rate rows, provider rows, bundle codes, rate sums, and rows per `negotiated_type` (including `derived`). It also matched an NPI filter on a CareFirst file. Every file stays under 92 MB peak RSS when parsed alone; the 2.46 GB HMSA file takes 20 s at 62 MB.

What these payers exposed:
- **The `*.mrf.bcbs.com` CDN returns 403 to requests without a `User-Agent`.** The client now sends `mrf-parse/<version>`, and the local test server rejects requests without one, so this can't regress.
- **A raw Windows-1252 byte inside UTF-8 JSON** (UCare, an en dash `0x96`) made strict parsers fail the whole file; Python's `json` and `ijson` reject it too. JSON input now goes through `mrf_core::text::Utf8Repair`, which passes valid UTF-8 through untouched and decodes stray bytes as Windows-1252. One reject per file records how many bytes were converted.
- **CareFirst publishes some TOCs as a top-level array** of complete TOC objects. `mrf toc` reads each in turn.
- **UCare's own TOC is invalid JSON** (a missing comma). Like Python, `mrf toc` fails it with the line and column. Its in-network files are fine.
- **Florida Blue's TOC links are signed URLs that had already expired**, so every download is denied. That's a publisher problem; no client can fetch those files.
- **Varied conventions, all kept raw:** `last_updated_on` as `09/29/2026`, `plan_id_type` as `EIN`, and a TOC `version` of `CAREFIRST`.

## Performance

Measured on an Apple M5 (arm64), release build, one file on one thread:

| reader | input | MB/s | rows/s |
|---|---|---|---|
| hospital csv_tall | 17.6 MB (90k charge rows) | 88 | 451k charge rows |
| hospital csv_wide | 12.2 MB (90k charge rows) | 73 | 543k charge rows |
| hospital json | 10.9 MB (31k charge rows) | 74–79 | 227k charge rows |
| tic in_network | 67.9 MB (1.1M rate rows) | 68–70 | 1.13M rate rows |
| tic in_network, NPI filter (2 passes) | 67.9 MB | 77–78 | 213k negotiated_rates/s scanned (1.1k rows kept) |
| tic toc | 13.5 MB (50k structures) | 234 | 865k entries |
| **memtest**: tic in_network, short notes | **2.0 GB** (35.1M rate rows) | **70** | **1.14M rate rows**, peak RSS **39.6 MB** |
| **memtest**: tic in_network, 1.5 KB notes per price | **2.0 GB** (3.8M rate rows) | **326** | 570k rate rows, peak RSS **64.3 MB** |
| real UHC in-network (Colorado exchange) | 1.5 GB uncompressed (2.3M rate rows) | ~290 | 440k rate rows, peak RSS 54 MB |

Hospital inputs are the CMS v3 examples scaled up with `tests/gen/gen_hospital.rs`. Parallelism is across files (`--jobs`), so aggregate throughput scales with cores when you parse many files.

## Known gaps

- **TiC JSON throughput (~70 MB/s)** is limited by building a `serde_json::Value` per negotiated rate. A typed borrowed deserializer would be faster; the structure (streaming visitors) wouldn't change.
- **TiC `--codes`** matches an element's own `billing_code`, not codes inside `bundled_codes` / `covered_services`.
- **`severity_of_illness`** written after an element's `negotiated_rates` isn't applied to rows already streamed (a warning is logged). Every other header field is required before rates stream, so this is the only one that can arrive late.
- **Duplicate `provider_group_id`s** in one file: the first definition wins; later ones are ignored.
- **In-memory sets** grow with distinct values, not input size: hospital item ids (8 bytes each), TiC defined/passing group ids, `mrf toc`'s seen URLs, and the remote provider-reference cache (shared across a run).
- **JSON syntax errors** (truncation, bad tokens) fail the file; a streaming parser can't resynchronize. Semantic problems are rejects.
- **HTTP** uses a blocking client with a 30 s connect timeout and no read timeout, so a stalled server hangs that worker. HTTPS is configured (rustls) but the tests use plain HTTP against a local server.
- **Zip:** archives nested inside a zip are not expanded, and only stored/deflate compression is supported (what every vendor file seen so far uses).
- **Hospital CSV `extra`** is stored on charge rows, so for wide CSV an item-level vendor column is repeated on each payer row.
- **Payers tested:** UnitedHealthcare, CareFirst (including Highmark and Anthem BlueCard files), HMSA, and UCare. Aetna, Cigna, and Excellus publish through JavaScript apps whose file lists weren't scraped, and BCBS Minnesota blocks automated requests, so those are untested. None of the tested files used remote `location` provider references or inline `provider_groups`; those paths are covered by fixtures only.
- **Not implemented:** the optional `mrfkit` row-count cross-check script; XML TiC, allowed-amounts and prescription-drug files, and normalization, matching, and rate conversion (out of scope).

## Workspace

```
crates/
  mrf-core/      input open (path/URL, gzip sniffing), staging + SHA-256 ids, filters,
                 table schemas, batching Parquet sink, rejects, reading output back
  mrf-hospital/  header tokenizer/mapper, CSV tall/wide reader, streaming JSON reader, layout detection
  mrf-tic/       streaming JSON helpers, TOC reader, provider references, in-network reader
  mrf-cli/       the `mrf` binary (parallel runs, summaries, validate)
tests/fixtures/  CMS examples + hand-made edge cases
tests/gen/       synthetic input generators (memtest, benches)
```

Dependencies beyond the suggested set:
- `serde` / `serde_json`: streaming JSON via visitors.
- `rayon`: bounded file-level parallelism.
- `tempfile`: URL staging and rate spill files, with cleanup on drop.
- `hex`: digest formatting.
- `nix`: a safe `getrusage` wrapper for peak RSS (the workspace forbids `unsafe`).
- `criterion` (dev only): benchmarks.

## License

Dual-licensed under MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)), at your option.

All code in this repository was written for this project from the CMS specifications and examples. No code was ported from the reference implementations (mrfkit, mrf-etl, mrfparse), so no NOTICE file is needed. The CMS example files in `tests/fixtures/` are US government works from [CMSgov/hospital-price-transparency](https://github.com/CMSgov/hospital-price-transparency) and [CMSgov/price-transparency-guide](https://github.com/CMSgov/price-transparency-guide).
