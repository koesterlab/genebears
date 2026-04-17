use std::path::PathBuf;

use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::Client;
use tracing::{debug, info, warn};

use crate::cache::Cache;
use crate::error::GeneBearError;
use crate::models::{AnnotateOptions, AnnotatedVariant, ApiResponse, Genome, Variant};
use crate::rate_limiter::RateLimiter;

const BASE_URL: &str = "https://api.genebe.net/cloud/api-public/v1";
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
/// Construct with [`GeneBears::new`], then call [`GeneBears::annotate_variant`]
/// or [`GeneBears::annotate_variants`].
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

        let http = Client::builder().default_headers(default_headers).build()?;

        let cache = if let Some(ref path) = config.cache_path {
            info!("Using DuckDB cache at {:?}", path);
            Some(Cache::open(path)?)
        } else {
            None
        };

        let rate_limiter = RateLimiter::new(config.rate_per_second, config.burst);
        let base_url = config
            .base_url
            .clone()
            .unwrap_or_else(|| BASE_URL.to_string());

        Ok(GeneBears {
            http,
            cache,
            rate_limiter,
            base_url,
            email: config.email,
            api_key: config.api_key,
        })
    }

    /// Annotate a **single** variant.
    ///
    /// Uses the cache on hit; otherwise calls the GeneBe batch endpoint with
    /// a single-element list
    pub async fn annotate_variant(
        &self,
        variant: &Variant,
        genome: Genome,
        opts: AnnotateOptions,
    ) -> Result<AnnotatedVariant, GeneBearError> {
        let mut results = self
            .annotate_variants(std::slice::from_ref(variant), genome, opts)
            .await?;
        results
            .pop()
            .ok_or_else(|| GeneBearError::Other("Empty response from GeneBe API".into()))
    }

    /// Annotate a **batch** of variants (up to 1 000).
    pub async fn annotate_variants(
        &self,
        variants: &[Variant],
        genome: Genome,
        opts: AnnotateOptions,
    ) -> Result<Vec<AnnotatedVariant>, GeneBearError> {
        if variants.len() > MAX_BATCH {
            return Err(GeneBearError::BatchTooLarge {
                requested: variants.len(),
            });
        }
        if variants.is_empty() {
            return Ok(vec![]);
        }

        let keys: Vec<String> = variants.iter().map(|v| v.cache_key(genome)).collect();

        let mut results: Vec<Option<AnnotatedVariant>> = vec![None; variants.len()];
        let mut miss_indices: Vec<usize> = Vec::new();

        if let Some(cache) = &self.cache {
            let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            let cached = cache.get_batch(&key_refs)?;

            for (i, key) in keys.iter().enumerate() {
                if let Some(ann) = cached.get(key) {
                    debug!(key, "cache hit");
                    results[i] = Some(ann.clone());
                } else {
                    miss_indices.push(i);
                }
            }
        } else {
            miss_indices = (0..variants.len()).collect();
        }

        if miss_indices.is_empty() {
            info!(count = variants.len(), "all variants served from cache");
            return Ok(results.into_iter().flatten().collect());
        }

        let miss_variants: Vec<&Variant> = miss_indices.iter().map(|&i| &variants[i]).collect();

        let body: Vec<serde_json::Value> = miss_variants
            .iter()
            .map(|v| {
                serde_json::json!({
                    "chr": v.chr,
                    "pos": v.pos,
                    "ref": v.ref_allele,
                    "alt": v.alt_allele,
                })
            })
            .collect();

        let mut query: Vec<(&str, &str)> = vec![("genome", genome.as_str())];
        if opts.use_refseq == Some(true) {
            query.push(("useRefseq", "true"));
        }
        if opts.use_ensembl == Some(true) {
            query.push(("useEnsembl", "true"));
        }
        if opts.omit_acmg {
            query.push(("omitAcmg", "true"));
        }
        if opts.omit_csq {
            query.push(("omitCsq", "true"));
        }
        if opts.omit_basic {
            query.push(("omitBasic", "true"));
        }
        if opts.omit_advanced {
            query.push(("omitAdvanced", "true"));
        }
        if opts.all_genes {
            query.push(("allGenes", "true"));
        }

        self.rate_limiter.acquire().await;

        let url = format!("{}/variants", self.base_url);
        let mut req = self.http.post(&url);

        if let (Some(email), Some(key)) = (&self.email, &self.api_key) {
            req = req.basic_auth(email, Some(key));
        }

        let resp = req.query(&query).json(&body).send().await?;

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(GeneBearError::Api {
                status: status.as_u16(),
                message: msg,
            });
        }

        let api_response: ApiResponse = resp.json().await?;
        let fetched = api_response.variants;

        for (local_idx, &original_idx) in miss_indices.iter().enumerate() {
            if let Some(ann) = fetched.get(local_idx) {
                results[original_idx] = Some(ann.clone());
            }
        }

        if let Some(cache) = &self.cache {
            let mut pairs: Vec<(String, AnnotatedVariant)> = Vec::new();
            for &original_idx in &miss_indices {
                if let Some(Some(ann)) = results.get(original_idx) {
                    pairs.push((keys[original_idx].clone(), ann.clone()));
                }
            }
            let ref_pairs: Vec<(&str, &AnnotatedVariant)> =
                pairs.iter().map(|(k, v)| (k.as_str(), v)).collect();
            if let Err(e) = cache.store_batch(&ref_pairs) {
                warn!("Failed to write batch to cache: {e}");
            }
        }

        Ok(results.into_iter().flatten().collect())
    }

    /// Annotate an arbitrarily large list of variants, automatically splitting
    /// into batches of 1 000 and respecting the rate limiter between batches.
    pub async fn annotate_variants_chunked(
        &self,
        variants: &[Variant],
        genome: Genome,
        opts: AnnotateOptions,
    ) -> Result<Vec<AnnotatedVariant>, GeneBearError> {
        let mut all = Vec::with_capacity(variants.len());
        for chunk in variants.chunks(MAX_BATCH) {
            let mut batch = self.annotate_variants(chunk, genome, opts.clone()).await?;
            all.append(&mut batch);
        }
        Ok(all)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_client() {
        let client = GeneBears::new(ClientConfig::default()).unwrap();
        let variants = vec![
            Variant::new("22", 28_695_868, "AG", "A"),
            Variant::new("6", 160_585_140, "T", "G"),
        ];

        let results = client
            .annotate_variants(&variants, Genome::Hg38, AnnotateOptions::default())
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        for result in &results {
            assert!(result.chr.is_some());
            assert!(result.pos.is_some());
        }
    }

    #[tokio::test]
    async fn cache_hit_skips_http() {
        use tempfile::tempdir;
        use wiremock::matchers::any;
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("cache.duckdb");
        let variant = Variant::new("22", 12345, "A", "T");
        let cache = Cache::open(&db_path).unwrap();
        let key = variant.cache_key(Genome::Hg38);

        let ann = AnnotatedVariant::default();

        cache.store(&key, &ann).unwrap();

        let config = ClientConfig {
            cache_path: Some(db_path.clone()),
            base_url: Some(server.uri()),
            ..Default::default()
        };

        let client = GeneBears::new(config).unwrap();
        let _res = client
            .annotate_variant(&variant, Genome::Hg38, AnnotateOptions::default())
            .await
            .unwrap();
        server.verify().await;
    }
}
