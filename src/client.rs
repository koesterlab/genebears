use std::path::PathBuf;
use std::sync::Arc;

use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Client, Response};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::cache::Cache;
use crate::error::GeneBearError;
use crate::hub::{DatabaseId, Hub};
use crate::models::{
    AnnotateOptions, AnnotatedVariant, Annotation, ApiResponse, Field, Genome, Record, Variant,
    Warning,
};
use crate::rate_limiter::RateLimiter;
use crate::store::{self, Installed, Store};

const BASE_URL: &str = "https://api.genebe.net/cloud/api-public/v1";
/// Maximum number of variants the API accepts per request.
const MAX_BATCH: usize = 1_000;

/// Configuration options for the client
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub email: Option<String>,
    pub api_key: Option<String>,
    /// Enables DuckDB caching at the given file path.
    pub cache_path: Option<PathBuf>,
    /// GeneBe Hub databases to annotate from before asking the API.
    pub store: Option<Store>,
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
            store: None,
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

    /// Annotate from the GeneBe Hub databases installed in the given store.
    pub fn with_store(mut self, store: Store) -> Self {
        self.store = Some(store);
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
/// Construct with [`GeneBears::new`], then call [`GeneBears::annotate_variants`] for selected
/// fields or [`GeneBears::annotate_api`] for complete API records.
pub struct GeneBears {
    http: Client,
    cache: Option<Cache>,
    store: Option<Store>,
    rate_limiter: RateLimiter,
    base_url: String,
    email: Option<String>,
    api_key: Option<String>,
}

/// Where the requested fields come from.
#[derive(Default)]
struct Sources {
    /// Hub columns by database, with the index of the field they fill.
    hub: Vec<(Installed, Vec<(usize, String)>)>,
    /// Hub columns that API fields are copied from. They are read separately from `hub`, so
    /// that their values do not depend on the other requested columns.
    copies: Vec<(Installed, Vec<(usize, String)>)>,
    /// The copied API fields, which the API fills for variants that cannot be looked up
    /// locally.
    copied: Vec<(usize, String)>,
    /// API fields that are not available locally.
    api: Vec<(usize, String)>,
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
            store: config.store,
            rate_limiter,
            base_url,
            email: config.email,
            api_key: config.api_key,
        })
    }

    /// Annotate variants with the given fields. Results are returned in the same order as
    /// `variants`.
    ///
    /// Hub columns are read from the store, as are API fields that GeneBe takes from
    /// installed Hub databases. Other API fields come from the cache or, if missing there,
    /// from the API.
    pub async fn annotate_variants(
        &self,
        variants: &[Variant],
        genome: Genome,
        fields: &[Field],
        opts: AnnotateOptions,
    ) -> Result<Vec<Annotation>, GeneBearError> {
        let sources = self.sources(fields, genome)?;
        let keys: Vec<_> = variants.iter().map(Variant::to_spdi).collect();
        let mut values = vec![vec![Value::Null; fields.len()]; variants.len()];
        let mut warnings = vec![Vec::new(); variants.len()];

        let local: Vec<_> = sources.hub.iter().chain(&sources.copies).cloned().collect();
        if !local.is_empty() {
            // Reading parquet files blocks.
            let lookup_keys = keys.clone();
            let found = tokio::task::spawn_blocking(move || store::lookup(&local, &lookup_keys))
                .await
                .map_err(|e| GeneBearError::Other(e.to_string()))??;
            for (variant, field, value) in found {
                values[variant][field] = value;
            }
        }
        if !sources.hub.is_empty() {
            for (variant, key) in keys.iter().enumerate() {
                if key.is_none() {
                    warnings[variant].push(Warning::NotLookedUp);
                }
            }
        }

        let asked: Vec<usize> = if !sources.api.is_empty() {
            (0..variants.len()).collect()
        } else if !sources.copied.is_empty() {
            (0..variants.len()).filter(|&i| keys[i].is_none()).collect()
        } else {
            Vec::new()
        };
        if !asked.is_empty() {
            let names: Vec<&str> = sources
                .api
                .iter()
                .chain(&sources.copied)
                .map(|(_, name)| name.as_str())
                .collect();
            let selected: Vec<&Variant> = asked.iter().map(|&i| &variants[i]).collect();
            let records = self.records(&selected, genome, opts, &names).await?;
            for (&variant, record) in asked.iter().zip(records) {
                let copied = if keys[variant].is_none() {
                    &sources.copied[..]
                } else {
                    &[]
                };
                for (field, name) in sources.api.iter().chain(copied) {
                    values[variant][*field] = record.get(name).cloned().unwrap_or_default();
                }
                if let Some(warning) = record.get("warning").and_then(Value::as_str) {
                    warnings[variant].push(Warning::Api(warning.to_string()));
                }
            }
        }

        let fields: Arc<[Field]> = fields.into();
        Ok(values
            .into_iter()
            .zip(warnings)
            .map(|(values, warnings)| Annotation {
                fields: fields.clone(),
                values,
                warnings,
            })
            .collect())
    }

    /// Annotate variants with the complete records of the API, from the cache or the API.
    /// Results are returned in the same order as `variants`.
    pub async fn annotate_api(
        &self,
        variants: &[Variant],
        genome: Genome,
        opts: AnnotateOptions,
    ) -> Result<Vec<AnnotatedVariant>, GeneBearError> {
        let variants: Vec<&Variant> = variants.iter().collect();
        self.records(&variants, genome, opts, &[])
            .await?
            .into_iter()
            .map(|record| Ok(serde_json::from_value(Value::Object(record))?))
            .collect()
    }

    /// Decide where each field comes from.
    fn sources(&self, fields: &[Field], genome: Genome) -> Result<Sources, GeneBearError> {
        let mut sources = Sources::default();
        for (index, field) in fields.iter().enumerate() {
            match field {
                Field::Api(name) => {
                    let copy = match &self.store {
                        Some(store) => store.api_field(name, genome).unwrap_or_else(|e| {
                            warn!("Cannot read {name} from the store, asking the API: {e}");
                            None
                        }),
                        None => None,
                    };
                    match copy {
                        Some((installed, column)) => {
                            group(&mut sources.copies, installed, index, column);
                            sources.copied.push((index, name.clone()));
                        }
                        None => sources.api.push((index, name.clone())),
                    }
                }
                Field::Hub { database, column } => {
                    let id: DatabaseId = database.parse()?;
                    let installed = self
                        .store
                        .as_ref()
                        .map(|store| store.find(&id))
                        .transpose()?
                        .flatten()
                        .ok_or(GeneBearError::NotInstalled { database: id })?;
                    store::check_column(&installed, column, genome)?;
                    group(&mut sources.hub, installed, index, column);
                }
            }
        }
        for (installed, _) in sources.hub.iter().chain(&sources.copies) {
            debug!("Annotating from {}", installed.database.id());
        }
        Ok(sources)
    }

    /// API records of the variants, from the cache or, for the others, from the API. Fails if
    /// the API does not know one of `names`.
    async fn records(
        &self,
        variants: &[&Variant],
        genome: Genome,
        opts: AnnotateOptions,
        names: &[&str],
    ) -> Result<Vec<Record>, GeneBearError> {
        let keys: Vec<String> = variants.iter().map(|v| v.cache_key(genome, opts)).collect();
        let mut records: Vec<Option<Record>> = vec![None; variants.len()];

        if let Some(cache) = &self.cache {
            for (records, keys) in records.chunks_mut(MAX_BATCH).zip(keys.chunks(MAX_BATCH)) {
                let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
                let cached = cache.get_batch(&key_refs)?;
                for (record, key) in records.iter_mut().zip(keys) {
                    // Records cached by older versions may lack fields, and unknown fields
                    // should be reported by the API.
                    *record = cached
                        .get(key)
                        .filter(|record| names.iter().all(|name| record.contains_key(*name)))
                        .cloned();
                }
            }
        }

        let misses: Vec<usize> = (0..variants.len())
            .filter(|&i| records[i].is_none())
            .collect();
        debug!(
            cached = variants.len() - misses.len(),
            asked = misses.len(),
            "looked up API records"
        );
        for batch in misses.chunks(MAX_BATCH) {
            let selected: Vec<&Variant> = batch.iter().map(|&i| variants[i]).collect();
            let fetched = self.fetch(&selected, genome, opts).await?;
            if let Some(record) = fetched.first() {
                if let Some(name) = names.iter().find(|name| !record.contains_key(**name)) {
                    return Err(GeneBearError::UnknownField {
                        field: Field::api(*name),
                    });
                }
            }
            if let Some(cache) = &self.cache {
                let entries: Vec<(&str, &Record)> = batch
                    .iter()
                    .map(|&i| keys[i].as_str())
                    .zip(&fetched)
                    .collect();
                if let Err(e) = cache.store_batch(&entries) {
                    warn!("Failed to write batch to cache: {e}");
                }
            }
            for (&i, record) in batch.iter().zip(fetched) {
                records[i] = Some(record);
            }
        }
        Ok(records.into_iter().flatten().collect())
    }

    /// Ask the API for at most [`MAX_BATCH`] variants.
    async fn fetch(
        &self,
        variants: &[&Variant],
        genome: Genome,
        opts: AnnotateOptions,
    ) -> Result<Vec<Record>, GeneBearError> {
        let body: Vec<Value> = variants
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
        let mut query = vec![("genome", genome.as_str())];
        query.extend(opts.params());

        self.rate_limiter.acquire().await;

        let mut req = self.http.post(format!("{}/variants", self.base_url));
        if let (Some(email), Some(key)) = (&self.email, &self.api_key) {
            req = req.basic_auth(email, Some(key));
        }
        let resp = check(req.query(&query).json(&body).send().await?).await?;

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

    /// Access the GeneBe Hub with the credentials and base URL of this client.
    pub fn hub(&self) -> Hub {
        Hub {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            credentials: self.email.clone().zip(self.api_key.clone()),
        }
    }
}

/// Add a column to the columns of its database.
fn group(
    groups: &mut Vec<(Installed, Vec<(usize, String)>)>,
    installed: Installed,
    field: usize,
    column: &str,
) {
    let column = (field, column.to_string());
    match groups
        .iter_mut()
        .find(|(other, _)| other.path == installed.path)
    {
        Some((_, columns)) => columns.push(column),
        None => groups.push((installed, vec![column])),
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
    use crate::store::tests::{install, install_revel};
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    /// Answers like the API with a copy of `record` for each requested variant.
    struct Records(Value);

    impl Respond for Records {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let variants: Vec<Value> = serde_json::from_slice(&request.body).unwrap();
            let records: Vec<Value> = variants
                .into_iter()
                .map(|variant| {
                    let mut record = self.0.clone();
                    for key in ["chr", "pos", "ref", "alt"] {
                        record[key] = variant[key].clone();
                    }
                    record
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "variants": records }))
        }
    }

    fn record() -> Value {
        json!({
            "chr": null, "pos": null, "ref": null, "alt": null,
            "acmg_score": 3.0,
            "gene_symbol": "BRCA1",
            "revel_score": 0.75,
            "warning": null,
        })
    }

    /// Mock API that expects `requests` requests.
    async fn api(record: Value, requests: u64) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(Records(record))
            .expect(requests)
            .mount(&server)
            .await;
        server
    }

    fn client(server: &MockServer, config: ClientConfig) -> GeneBears {
        GeneBears::new(ClientConfig {
            base_url: Some(server.uri()),
            ..config
        })
        .unwrap()
    }

    fn snv() -> Variant {
        Variant::new("22", 100, "A", "T")
    }

    async fn annotate(
        client: &GeneBears,
        variants: &[Variant],
        fields: &[Field],
    ) -> Result<Vec<Annotation>, GeneBearError> {
        client
            .annotate_variants(variants, Genome::Hg38, fields, AnnotateOptions::default())
            .await
    }

    #[tokio::test]
    async fn annotate_variants_asks_api() {
        let server = api(record(), 1).await;
        let client = client(&server, ClientConfig::default());
        let (acmg, gene) = (Field::api("acmg_score"), Field::api("gene_symbol"));

        let annotations = annotate(&client, &[snv()], &[acmg.clone(), gene.clone()])
            .await
            .unwrap();

        assert_eq!(annotations[0].f64(&acmg), Some(3.0));
        assert_eq!(annotations[0].str(&gene), Some("BRCA1"));
        assert!(annotations[0].warnings.is_empty());
        server.verify().await;
    }

    #[tokio::test]
    async fn annotate_variants_rejects_unknown_api_fields() {
        let server = api(record(), 1).await;
        let client = client(&server, ClientConfig::default());

        let result = annotate(&client, &[snv()], &[Field::api("acme_score")]).await;

        assert!(matches!(result, Err(GeneBearError::UnknownField { .. })));
        server.verify().await;
    }

    #[tokio::test]
    async fn annotate_variants_splits_large_requests() {
        let server = api(record(), 3).await;
        let client = client(&server, ClientConfig::default());
        let variants: Vec<_> = (1..=2500)
            .map(|pos| Variant::new("1", pos, "A", "T"))
            .collect();
        let pos = Field::api("pos");

        let annotations = annotate(&client, &variants, std::slice::from_ref(&pos))
            .await
            .unwrap();

        let positions: Vec<_> = annotations
            .iter()
            .map(|a| a.f64(&pos).unwrap() as u64)
            .collect();
        assert_eq!(positions, (1..=2500).collect::<Vec<_>>());
        server.verify().await;
    }

    #[tokio::test]
    async fn annotate_variants_passes_on_api_warnings() {
        let mut record = record();
        record["warning"] = "Reference allele does not match".into();
        let server = api(record, 1).await;
        let client = client(&server, ClientConfig::default());

        let annotations = annotate(&client, &[snv()], &[Field::api("acmg_score")])
            .await
            .unwrap();

        assert_eq!(
            annotations[0].warnings,
            [Warning::Api("Reference allele does not match".into())]
        );
    }

    #[tokio::test]
    async fn annotate_variants_uses_cached_records() {
        let server = api(record(), 0).await;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cache.duckdb");
        let cached = record().as_object().unwrap().clone();
        let key = snv().cache_key(Genome::Hg38, AnnotateOptions::default());
        Cache::open(&path)
            .unwrap()
            .store_batch(&[(&key, &cached)])
            .unwrap();
        let client = client(&server, ClientConfig::default().with_cache(path));

        let annotations = annotate(&client, &[snv()], &[Field::api("acmg_score")])
            .await
            .unwrap();

        assert_eq!(annotations[0].f64(&Field::api("acmg_score")), Some(3.0));
        server.verify().await;
    }

    #[tokio::test]
    async fn annotate_variants_uses_cached_records_for_repeated_variants() {
        let server = api(record(), 0).await;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cache.duckdb");
        let key = snv().cache_key(Genome::Hg38, AnnotateOptions::default());
        let cached = record().as_object().unwrap().clone();
        Cache::open(&path)
            .unwrap()
            .store_batch(&[(&key, &cached)])
            .unwrap();
        let client = client(&server, ClientConfig::default().with_cache(path));

        let annotations = annotate(&client, &[snv(), snv()], &[Field::api("acmg_score")])
            .await
            .unwrap();

        assert_eq!(annotations.len(), 2);
        server.verify().await;
    }

    #[tokio::test]
    async fn annotate_variants_asks_api_for_fields_missing_in_cached_records() {
        let server = api(record(), 1).await;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cache.duckdb");
        let key = snv().cache_key(Genome::Hg38, AnnotateOptions::default());
        let mut cached = record().as_object().unwrap().clone();
        cached.remove("acmg_score");
        Cache::open(&path)
            .unwrap()
            .store_batch(&[(&key, &cached)])
            .unwrap();
        let client = client(&server, ClientConfig::default().with_cache(path));
        let acmg = Field::api("acmg_score");

        let annotations = annotate(&client, &[snv()], std::slice::from_ref(&acmg))
            .await
            .unwrap();

        assert_eq!(annotations[0].f64(&acmg), Some(3.0));
        server.verify().await;
    }

    #[tokio::test]
    async fn annotate_variants_reads_copies_independently_of_hub_columns() {
        let server = api(record(), 0).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install(
            &store,
            "@genebe/spliceai",
            "GRCh38",
            &["max", "gene"],
            "SELECT * FROM (VALUES ('22', 99, 1, 'T', 0.5, 'A'), ('22', 99, 1, 'T', 0.25, 'B'))
                AS t(_seq, _pos, _del, _ins, max, gene)",
        );
        let client = client(&server, ClientConfig::default().with_store(store));
        let spliceai = Field::api("spliceai_max_score");
        let gene = Field::hub("@genebe/spliceai", "gene");

        let alone = annotate(&client, &[snv()], std::slice::from_ref(&spliceai))
            .await
            .unwrap();
        let together = annotate(&client, &[snv()], &[gene, spliceai.clone()])
            .await
            .unwrap();

        assert_eq!(alone[0].f64(&spliceai), Some(0.5));
        assert_eq!(together[0].f64(&spliceai), Some(0.5));
        server.verify().await;
    }

    #[tokio::test]
    async fn annotate_variants_caches_records_per_options() {
        let server = api(record(), 1).await;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cache.duckdb");
        let key = snv().cache_key(Genome::Hg38, AnnotateOptions::default());
        let cached = record().as_object().unwrap().clone();
        Cache::open(&path)
            .unwrap()
            .store_batch(&[(&key, &cached)])
            .unwrap();
        let client = client(&server, ClientConfig::default().with_cache(path));
        let opts = AnnotateOptions {
            omit_acmg: true,
            ..Default::default()
        };

        client
            .annotate_variants(&[snv()], Genome::Hg38, &[Field::api("acmg_score")], opts)
            .await
            .unwrap();

        server.verify().await;
    }

    #[tokio::test]
    async fn annotate_variants_reads_api_fields_from_store() {
        let server = api(record(), 0).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);
        let client = client(&server, ClientConfig::default().with_store(store));
        let revel = Field::api("revel_score");

        let annotations = annotate(&client, &[snv()], std::slice::from_ref(&revel))
            .await
            .unwrap();

        assert_eq!(annotations[0].f64(&revel), Some(0.034));
        server.verify().await;
    }

    #[tokio::test]
    async fn annotate_variants_asks_api_for_unaligned_indels() {
        let server = api(record(), 1).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);
        let client = client(&server, ClientConfig::default().with_store(store));
        let revel = Field::api("revel_score");
        // The second variant is not left-aligned.
        let variants = [snv(), Variant::new("22", 100, "AA", "A")];

        let annotations = annotate(&client, &variants, std::slice::from_ref(&revel))
            .await
            .unwrap();

        assert_eq!(annotations[0].f64(&revel), Some(0.034));
        assert_eq!(annotations[1].f64(&revel), Some(0.75));
        let requests = server.received_requests().await.unwrap();
        let asked: Vec<Value> = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(asked.len(), 1);
        server.verify().await;
    }

    #[tokio::test]
    async fn annotate_variants_asks_api_for_hg19() {
        let server = api(record(), 1).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);
        let client = client(&server, ClientConfig::default().with_store(store));
        let revel = Field::api("revel_score");

        let annotations = client
            .annotate_variants(
                &[snv()],
                Genome::Hg19,
                std::slice::from_ref(&revel),
                AnnotateOptions::default(),
            )
            .await
            .unwrap();

        assert_eq!(annotations[0].f64(&revel), Some(0.75));
        server.verify().await;
    }

    #[tokio::test]
    async fn annotate_variants_reads_hub_columns() {
        let server = api(record(), 0).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install(
            &store,
            "@genebe/cadd_hg38:0.0.2",
            "GRCh38",
            &["phred"],
            "SELECT '22' AS _seq, 99 AS _pos, 1 AS _del, 'T' AS _ins, 23.5::FLOAT AS phred",
        );
        let client = client(&server, ClientConfig::default().with_store(store));
        let cadd = Field::hub("@genebe/cadd_hg38", "phred");
        // The second variant is not left-aligned.
        let variants = [snv(), Variant::new("22", 100, "AA", "A")];

        let annotations = annotate(&client, &variants, std::slice::from_ref(&cadd))
            .await
            .unwrap();

        assert_eq!(annotations[0].f64(&cadd), Some(23.5));
        assert!(annotations[0].warnings.is_empty());
        assert_eq!(annotations[1].f64(&cadd), None);
        assert_eq!(annotations[1].warnings, [Warning::NotLookedUp]);
        server.verify().await;
    }

    #[tokio::test]
    async fn annotate_variants_needs_installed_hub_databases() {
        let server = api(record(), 0).await;
        let dir = TempDir::new().unwrap();
        let cadd = Field::hub("@genebe/cadd_hg38", "phred");
        let configs = [
            ClientConfig::default(),
            ClientConfig::default().with_store(Store::new(dir.path())),
        ];

        for config in configs {
            let result = annotate(
                &client(&server, config),
                &[snv()],
                std::slice::from_ref(&cadd),
            )
            .await;
            assert!(matches!(result, Err(GeneBearError::NotInstalled { .. })));
        }
        server.verify().await;
    }

    #[tokio::test]
    async fn annotate_api_returns_complete_records() {
        let server = api(record(), 1).await;
        let client = client(&server, ClientConfig::default());

        let records = client
            .annotate_api(&[snv()], Genome::Hg38, AnnotateOptions::default())
            .await
            .unwrap();

        assert_eq!(records[0].gene_symbol.as_deref(), Some("BRCA1"));
        assert_eq!(records[0].pos, Some(100));
        server.verify().await;
    }

    #[tokio::test]
    async fn missing_annotations_are_an_error() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "variants": [{}] })))
            .mount(&server)
            .await;
        let client = client(&server, ClientConfig::default());
        let variants = [snv(), Variant::new("22", 200, "C", "G")];

        let result = annotate(&client, &variants, &[Field::api("acmg_score")]).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_client() {
        let client = GeneBears::new(ClientConfig::default()).unwrap();
        let variants = vec![
            Variant::new("22", 28_695_868, "AG", "A"),
            Variant::new("6", 160_585_140, "T", "G"),
        ];
        let acmg = Field::api("acmg_score");

        let annotations = annotate(&client, &variants, std::slice::from_ref(&acmg))
            .await
            .unwrap();

        assert_eq!(annotations.len(), 2);
        assert!(annotations.iter().all(|a| a.f64(&acmg).is_some()));
    }

    #[tokio::test]
    async fn reference_mismatch_is_reported_as_warning() {
        let client = GeneBears::new(ClientConfig::default()).unwrap();
        let variant = Variant::new("7", 140_753_336, "G", "T");

        let annotations = annotate(&client, &[variant], &[Field::api("acmg_score")])
            .await
            .unwrap();

        assert!(matches!(annotations[0].warnings[..], [Warning::Api(_)]));
    }
}
