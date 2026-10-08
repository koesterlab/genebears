use thiserror::Error;

use crate::hub::DatabaseId;
use crate::models::{Field, Genome};

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GeneBearError {
    /// HTTP / network errors from reqwest.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    /// JSON serialization / deserialization errors.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// DuckDB errors of the cache or of Hub database lookups.
    #[error("DuckDB error: {0}")]
    DuckDb(#[from] duckdb::Error),
    /// The GeneBe API rejected the request (4xx) — likely bad input.
    #[error("API client error (HTTP {status}): {message}")]
    ApiClientError { status: u16, message: String },
    /// The GeneBe API failed to process the request (5xx) — e.g. unknown contig.
    #[error("API server error (HTTP {status}): {message}")]
    ApiServerError { status: u16, message: String },
    /// A requested API field or Hub database column does not exist.
    #[error("Unknown field {field}")]
    UnknownField { field: Field },
    /// A requested Hub database is not installed.
    #[error("{database} is not installed")]
    NotInstalled { database: DatabaseId },
    /// A Hub database is for another genome than requested.
    #[error("{database} is not for {}", genome.as_str())]
    GenomeMismatch {
        database: DatabaseId,
        genome: Genome,
    },
    /// A Hub database id is not of the form `owner/name[:version]`.
    #[error("Invalid GeneBe Hub database id {0}, expected owner/name[:version]")]
    InvalidDatabaseId(String),
    /// I/O errors, e.g. of the local GeneBe Hub store.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A file downloaded from the GeneBe Hub does not match its size or checksum.
    #[error("Downloaded file {file} does not match its size or checksum")]
    Checksum { file: String },
    /// The GeneBe Hub database requires accepting its license, which genebears does not
    /// support.
    #[error("{id} requires accepting its license, which is not supported")]
    LicenseNotAccepted { id: String },
    /// Catch-all for miscellaneous errors.
    #[error("{0}")]
    Other(String),
}
