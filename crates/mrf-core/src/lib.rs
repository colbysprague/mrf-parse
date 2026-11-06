//! Shared building blocks for the MRF parsers: opening inputs (local path or
//! URL, plain or gzip), stable ids, table schemas, the Parquet sink, and
//! rejects.

pub mod filter;
pub mod ids;
pub mod input;
pub mod mem;
pub mod read;
pub mod schema;
pub mod sink;
pub mod stage;
pub mod text;

#[cfg(any(test, feature = "test-util"))]
pub mod testutil;

pub use filter::{Filters, ValueFilter};
pub use ids::{file_id_from_sha256, sha256_hex_of, stable_id, HashingReader};
pub use input::{open, open_raw, InputError, Source};
pub use schema::Table;
pub use sink::{Cell, RejectSink, SinkError, TableWriter};
pub use stage::StagedInput;
