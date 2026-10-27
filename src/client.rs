use std::path::PathBuf;
use std::time::Duration;

use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Client, Response};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::cache::Cache;
use crate::error::GeneBearError;
use crate::models::{AnnotateOptions, ApiResponse, Genome, Record, Variant};
use crate::rate_limiter::RateLimiter;

pub(crate) const BASE_URL: &str = "https://api.genebe.net/cloud/api-public/v1";
/// Maximum number of variants the API accepts per request.
const MAX_BATCH: usize = 1_000;

/// Configuration options for the client
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub email: Option<String>,
    pub api_key: Option<String>,
    /// Enables DuckDB caching at the given file path.
    pub cache_path: Option<PathBuf>,
    /// Sustained request rate in requests per second.
    /// Defaults to `3.0`.
    pub rate_per_second: f64,
    /// Maximum burst size (tokens that can accumulate while idle).
    /// Defaults to `5`.
    pub burst: u32,
    /// Override the API base URL. Defaults to `https://api.genebe.net/cloud/api-public/v1`.
    pub base_url: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            email: None,
            api_key: None,
            cache_path: None,
            rate_per_second: 3.0,
            burst: 5,
            base_url: None,
        }
    }
}

impl ClientConfig {
    pub fn with_credentials(email: impl Into<String>, api_key: impl Into<String>) -> Self {
        ClientConfig {
            email: Some(email.into()),
            api_key: Some(api_key.into()),
            ..Default::default()
        }
    }

    /// Enable the DuckDB cache at the given path.
    pub fn with_cache(mut self, path: impl Into<PathBuf>) -> Self {
        self.cache_path = Some(path.into());
        self
    }

    /// Override the rate limit.
    pub fn with_rate_limit(mut self, rate_per_second: f64, burst: u32) -> Self {
        self.rate_per_second = rate_per_second;
        self.burst = burst;
        self
    }
}

/// The main genebears client.
///
/// Construct with [`GeneBears::new`], then call [`GeneBears::annotate`].
pub struct GeneBears {
    http: Client,
    cache: Option<Cache>,
    rate_limiter: RateLimiter,
    base_url: String,
    email: Option<String>,
    api_key: Option<String>,
}

impl GeneBears {
    /// Build a new client from [`ClientConfig`].
    pub fn new(config: ClientConfig) -> Result<Self, GeneBearError> {
        let mut default_headers = HeaderMap::new();
        default_headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));

        let http = Client::builder()
            .default_headers(default_headers)
            .connect_timeout(Duration::from_secs(30))
            // A stalled request would otherwise never end.
            .read_timeout(Duration::from_secs(60))
            .build()?;

        let cache = if let Some(ref path) = config.cache_path {
            info!("Using DuckDB cache at {:?}", path);
            Some(Cache::open(path)?)
        } else {
            None
        };

        Ok(GeneBears {
            http,
            cache,
            rate_limiter: RateLimiter::new(config.rate_per_second, config.burst),
            base_url: config.base_url.unwrap_or_else(|| BASE_URL.to_string()),
            email: config.email,
            api_key: config.api_key,
        })
    }

    /// Annotate variants from the cache or, for the others, the API. Results are returned in
    /// the same order as `variants`.
    ///
    /// The records of the API are deserialized into `T`, which is either
    /// [`AnnotatedVariant`](crate::AnnotatedVariant) with all fields or a struct with just
    /// the fields you need.
    pub async fn annotate<T: DeserializeOwned>(
        &self,
        variants: &[Variant],
        genome: Genome,
        opts: AnnotateOptions,
    ) -> Result<Vec<T>, GeneBearError> {
        let keys: Vec<String> = variants.iter().map(|v| v.cache_key(genome, opts)).collect();
        let mut annotations: Vec<Option<T>> = variants.iter().map(|_| None).collect();

        if let Some(cache) = &self.cache {
            for (annotations, keys) in annotations
                .chunks_mut(MAX_BATCH)
                .zip(keys.chunks(MAX_BATCH))
            {
                let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
                let cached = cache.get_batch(&key_refs)?;
                for (annotation, key) in annotations.iter_mut().zip(keys) {
                    if let Some(record) = cached.get(key) {
                        *annotation = Some(serde_json::from_value(Value::Object(record.clone()))?);
                    }
                }
            }
        }

        let misses: Vec<usize> = (0..variants.len())
            .filter(|&i| annotations[i].is_none())
            .collect();
        debug!(
            cached = variants.len() - misses.len(),
            asked = misses.len(),
            "looked up variants"
        );
        for batch in misses.chunks(MAX_BATCH) {
            let selected: Vec<&Variant> = batch.iter().map(|&i| &variants[i]).collect();
            let records = self.fetch(&selected, genome, opts).await?;
            if let Some(cache) = &self.cache {
                let entries: Vec<(&str, &Record)> = batch
                    .iter()
                    .map(|&i| keys[i].as_str())
                    .zip(&records)
                    .collect();
                if let Err(e) = cache.store_batch(&entries) {
                    warn!("Failed to write batch to cache: {e}");
                }
            }
            for (&i, record) in batch.iter().zip(records) {
                annotations[i] = Some(serde_json::from_value(Value::Object(record))?);
            }
        }
        Ok(annotations.into_iter().flatten().collect())
    }

    /// Ask the API for at most [`MAX_BATCH`] variants.
    async fn fetch(
        &self,
        variants: &[&Variant],
        genome: Genome,
        opts: AnnotateOptions,
    ) -> Result<Vec<Record>, GeneBearError> {
        let mut query = vec![("genome", genome.as_str())];
        query.extend(opts.params());

        self.rate_limiter.acquire().await;

        let mut req = self.http.post(format!("{}/variants", self.base_url));
        if let (Some(email), Some(key)) = (&self.email, &self.api_key) {
            req = req.basic_auth(email, Some(key));
        }
        let resp = check(req.query(&query).json(variants).send().await?).await?;

        let fetched = resp.json::<ApiResponse>().await?.variants;
        if fetched.len() != variants.len() {
            return Err(GeneBearError::Other(format!(
                "GeneBe returned {} annotations for {} variants",
                fetched.len(),
                variants.len()
            )));
        }
        Ok(fetched)
    }

    /// Number of variants currently in the cache, or `None` if caching is disabled.
    pub fn cache_count(&self) -> Option<Result<u64, GeneBearError>> {
        self.cache.as_ref().map(Cache::count)
    }

    /// Wipe the cache.
    pub fn cache_clear(&self) -> Result<(), GeneBearError> {
        if let Some(cache) = &self.cache {
            cache.clear()?;
        }
        Ok(())
    }
}

/// Turn error responses into [`GeneBearError::ApiClientError`] or
/// [`GeneBearError::ApiServerError`].
pub(crate) async fn check(response: Response) -> Result<Response, GeneBearError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let message = response.text().await.unwrap_or_default();
    Err(if status.is_client_error() {
        GeneBearError::ApiClientError {
            status: status.as_u16(),
            message,
        }
    } else {
        GeneBearError::ApiServerError {
            status: status.as_u16(),
            message,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AnnotatedVariant;
    use serde::Deserialize;
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    /// Answers like the API with a record per requested variant.
    struct Records;

    impl Respond for Records {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let variants: Vec<Value> = serde_json::from_slice(&request.body).unwrap();
            let records: Vec<Value> = variants
                .into_iter()
                .map(|v| json!({"chr": v["chr"], "pos": v["pos"], "ref": v["ref"], "alt": v["alt"], "revel_score": 0.5}))
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "variants": records }))
        }
    }

    /// Mock API that expects `requests` requests.
    async fn api(requests: u64) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(Records)
            .expect(requests)
            .mount(&server)
            .await;
        server
    }

    fn client(server: &MockServer, cache: Option<PathBuf>) -> GeneBears {
        GeneBears::new(ClientConfig {
            base_url: Some(server.uri()),
            cache_path: cache,
            ..Default::default()
        })
        .unwrap()
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Revel {
        pos: u64,
        revel_score: Option<f64>,
    }

    #[tokio::test]
    async fn annotate_deserializes_records_in_variant_order() {
        let server = api(1).await;
        let variants = [
            Variant::new("22", 200, "A", "T"),
            Variant::new("22", 100, "A", "T"),
        ];

        let annotations: Vec<Revel> = client(&server, None)
            .annotate(&variants, Genome::Hg38, AnnotateOptions::default())
            .await
            .unwrap();

        assert_eq!(
            annotations,
            [
                Revel {
                    pos: 200,
                    revel_score: Some(0.5)
                },
                Revel {
                    pos: 100,
                    revel_score: Some(0.5)
                }
            ]
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn annotate_asks_the_api_in_batches() {
        let server = api(3).await;
        let variants: Vec<Variant> = (1..=2500)
            .map(|pos| Variant::new("22", pos, "A", "T"))
            .collect();

        let annotations: Vec<AnnotatedVariant> = client(&server, None)
            .annotate(&variants, Genome::Hg38, AnnotateOptions::default())
            .await
            .unwrap();

        let positions: Vec<u64> = annotations.iter().map(|a| a.pos.unwrap()).collect();
        assert_eq!(positions, (1..=2500).collect::<Vec<_>>());
        server.verify().await;
    }

    #[tokio::test]
    async fn cached_variants_skip_the_api() {
        let server = api(2).await;
        let dir = TempDir::new().unwrap();
        let client = client(&server, Some(dir.path().join("cache.duckdb")));
        let variants = [
            Variant::new("22", 100, "A", "T"),
            Variant::new("22", 100, "A", "T"),
        ];
        let omit_acmg = AnnotateOptions {
            omit_acmg: true,
            ..Default::default()
        };

        for opts in [
            AnnotateOptions::default(),
            AnnotateOptions::default(),
            omit_acmg,
        ] {
            let annotations: Vec<Revel> = client
                .annotate(&variants, Genome::Hg38, opts)
                .await
                .unwrap();
            assert_eq!(annotations.len(), 2);
        }

        // Options change the records, so they are cached separately.
        assert_eq!(client.cache_count().unwrap().unwrap(), 2);
        server.verify().await;
    }

    #[tokio::test]
    async fn missing_annotations_are_an_error() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "variants": [{}] })))
            .mount(&server)
            .await;
        let variants = [
            Variant::new("22", 100, "A", "T"),
            Variant::new("22", 200, "C", "G"),
        ];

        let result: Result<Vec<AnnotatedVariant>, _> = client(&server, None)
            .annotate(&variants, Genome::Hg38, AnnotateOptions::default())
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_client() {
        let client = GeneBears::new(ClientConfig::default()).unwrap();
        let variants = vec![
            Variant::new("22", 28_695_868, "AG", "A"),
            Variant::new("6", 160_585_140, "T", "G"),
        ];

        let results: Vec<AnnotatedVariant> = client
            .annotate(&variants, Genome::Hg38, AnnotateOptions::default())
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        for result in &results {
            assert!(result.chr.is_some());
            assert!(result.pos.is_some());
        }
    }

    #[tokio::test]
    async fn reference_mismatch_is_reported_as_warning() {
        let client = GeneBears::new(ClientConfig::default()).unwrap();
        let variant = Variant::new("7", 140_753_336, "G", "T");

        let results: Vec<AnnotatedVariant> = client
            .annotate(&[variant], Genome::Hg38, AnnotateOptions::default())
            .await
            .unwrap();
        assert!(results[0].warning.is_some());
    }
}
