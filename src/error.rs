use thiserror::Error;

#[derive(Debug, Error)]
pub enum GeneBearError {
    /// HTTP / network errors from reqwest.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    /// JSON serialization / deserialization errors.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// DuckDB cache errors.
    #[error("Cache error: {0}")]
    Cache(#[from] duckdb::Error),
    /// The GeneBe API returned a non-2xx status code.
    #[error("API error (HTTP {status}): {message}")]
    Api { status: u16, message: String },
    /// Requested batch exceeds the API limit of 1 000 variants.
    #[error("Batch too large: {requested} variants requested, maximum is 1 000")]
    BatchTooLarge { requested: usize },
    /// Catch-all for miscellaneous errors.
    #[error("{0}")]
    Other(String),
}
