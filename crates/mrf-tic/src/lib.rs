//! Transparency in Coverage (TiC) readers: table of contents, provider
//! references, and the streaming in-network reader.

pub mod in_network;
pub mod json;
pub mod provider;
pub mod toc;

use mrf_core::{InputError, SinkError};

pub use in_network::{
    parse_all, parse_in_network_file, parse_staged, TicCounts, TicOptions, TicSummary,
};
pub use provider::RemoteRefCache;

#[derive(Debug, thiserror::Error)]
pub enum TicError {
    #[error(transparent)]
    Input(#[from] InputError),
    #[error(transparent)]
    Sink(#[from] SinkError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(String),
    #[error("unknown layout: {0}")]
    UnknownLayout(String),
}
