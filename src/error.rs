use thiserror::Error;

#[derive(Debug, Error)]
pub enum GeneBearError {
    /// HTTP / network errors from reqwest.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    /// JSON serialization / deserialization errors.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// DuckDB errors of the cache or of Hub database lookups.
    #[error("DuckDB error: {0}")]
    Cache(#[from] duckdb::Error),
    /// The GeneBe API rejected the request (4xx) — likely bad input.
    #[error("API client error (HTTP {status}): {message}")]
    ApiClientError { status: u16, message: String },
    /// The GeneBe API failed to process the request (5xx) — e.g. unknown contig.
    #[error("API server error (HTTP {status}): {message}")]
    ApiServerError { status: u16, message: String },
    /// Requested batch exceeds the API limit of 1 000 variants.
    #[error("Batch too large: {requested} variants requested, maximum is 1 000")]
    BatchTooLarge { requested: usize },
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
