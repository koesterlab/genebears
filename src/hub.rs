//! Downloading databases from the [GeneBe Hub](https://genebe.net/hub), a repository of
//! variant annotation databases in parquet format.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};
use xxhash_rust::xxh32::Xxh32;

use crate::client::{check, BASE_URL};
use crate::error::GeneBearError;
use crate::store::{Database, Store};

const ATTEMPTS: usize = 3;

/// Identifier of a Hub database, written as `owner/name` or `owner/name:version`, e.g.
/// `@genebe/revel:0.0.1`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DatabaseId {
    pub owner: String,
    pub name: String,
    /// The newest active version is used if not given.
    pub version: Option<String>,
}

impl DatabaseId {
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> Self {
        DatabaseId {
            owner: owner.into(),
            name: name.into(),
            version: None,
        }
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// The parts end up in URLs and paths of the local store, so only a few characters are
    /// allowed.
    pub(crate) fn validate(&self) -> Result<(), GeneBearError> {
        let valid = |part: &str| {
            !part.is_empty()
                && !part.starts_with('.')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "@._+-".contains(c))
        };
        if valid(&self.owner) && valid(&self.name) && self.version.as_deref().is_none_or(valid) {
            Ok(())
        } else {
            Err(invalid_id(self))
        }
    }
}

fn invalid_id(id: impl fmt::Display) -> GeneBearError {
    GeneBearError::InvalidDatabaseId(id.to_string())
}

impl FromStr for DatabaseId {
    type Err = GeneBearError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (path, version) = match text.split_once(':') {
            Some((path, version)) => (path, Some(version)),
            None => (text, None),
        };
        let (owner, name) = path.split_once('/').unwrap_or_default();
        let id = DatabaseId {
            owner: owner.to_string(),
            name: name.to_string(),
            version: version.map(str::to_string),
        };
        id.validate().map_err(|_| invalid_id(text))?;
        Ok(id)
    }
}

impl fmt::Display for DatabaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)?;
        if let Some(version) = &self.version {
            write!(f, ":{version}")?;
        }
        Ok(())
    }
}

/// Description of a Hub database version. Stored as `description.toml` next to the data of
/// an installed database.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Description {
    pub owner: String,
    pub name: String,
    #[serde(deserialize_with = "null_as_default")]
    pub version: String,
    /// `ACTIVE` for databases that can be downloaded. Not part of `description.toml`.
    #[serde(skip_serializing)]
    pub status: Option<String>,
    pub title: Option<String>,
    /// Genome build, e.g. `GRCh38`.
    pub genome: Option<String>,
    pub species: Option<String>,
    /// `VARIANT`, `POSITION`, `REGION`, ...
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub license_type: Option<String>,
    pub license_accept_required: bool,
    #[serde(deserialize_with = "null_as_default")]
    pub columns: Vec<Column>,
    #[serde(deserialize_with = "null_as_default")]
    pub files: Vec<DatabaseFile>,
}

impl Description {
    pub fn id(&self) -> DatabaseId {
        DatabaseId::new(&self.owner, &self.name).with_version(&self.version)
    }
}

/// The Hub sends `null` for some fields that are empty.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DatabaseFile {
    /// Path relative to the database directory, e.g. `parquet/_seq=1/part-0.parquet`.
    pub file_name: String,
    pub size: Option<u64>,
    /// xxHash32 of the file content as hex number.
    pub xxh32sum: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Download {
    description: Description,
    /// Download link of each file.
    #[serde(default)]
    files: HashMap<String, String>,
    description_text: Option<String>,
    license_text: Option<String>,
}

/// Client for the GeneBe Hub.
#[derive(Clone)]
pub struct Hub {
    http: Client,
    base_url: String,
    email: String,
    api_key: String,
}

impl Hub {
    /// Hub client with the credentials of a GeneBe account, which downloads require.
    pub fn new(
        email: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self, GeneBearError> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            // A stalled download would otherwise never end.
            .read_timeout(Duration::from_secs(60))
            .build()?;
        Ok(Hub {
            http,
            base_url: BASE_URL.to_string(),
            email: email.into(),
            api_key: api_key.into(),
        })
    }

    /// Fetch the description of a database version, the newest active one if `id` has
    /// none.
    pub async fn describe(&self, id: &DatabaseId) -> Result<Description, GeneBearError> {
        id.validate()?;
        let mut url = format!("{}/hub/owner/{}/name/{}", self.base_url, id.owner, id.name);
        if let Some(version) = &id.version {
            url = format!("{url}/version/{version}");
        }
        let request = self
            .http
            .get(&url)
            .basic_auth(&self.email, Some(&self.api_key));
        Ok(check(request.send().await?).await?.json().await?)
    }

    /// Download a database version into the store, the newest active one if `id` has none,
    /// unless it is installed already.
    ///
    /// Files are verified against their checksums and downloaded into a hidden directory,
    /// which is renamed once complete, so unfinished databases never show up in the store.
    /// An interrupted pull continues with the files it did not finish. Pulls of the same
    /// database in parallel wait for each other.
    pub async fn pull(&self, id: &DatabaseId, store: &Store) -> Result<Database, GeneBearError> {
        let description = self.describe(id).await?;
        let id = description.id();
        id.validate()?;
        let version = &description.version;
        let path = store.path(&id.owner, &id.name, version);
        let hidden = |suffix: &str| path.with_file_name(format!(".{version}{suffix}"));
        fs::create_dir_all(path.parent().unwrap())?;
        let _lock = lock(&hidden(".lock")).await?;
        if let Ok(database) = store.get(&id) {
            info!("{id} is already installed");
            return Ok(database);
        }
        if description.status.as_deref() != Some("ACTIVE") {
            return Err(GeneBearError::Other(format!(
                "{id} cannot be downloaded, its status is {}",
                description.status.as_deref().unwrap_or("unknown")
            )));
        }
        if description.license_accept_required {
            return Err(GeneBearError::Other(format!(
                "{id} requires accepting its license, which is not supported"
            )));
        }

        let mut download = self.request_download(&id).await?;
        let staging = hidden("");
        let files = &download.description.files;
        let size: u64 = files.iter().filter_map(|f| f.size).sum();
        info!(
            "Downloading {id} ({} files, {:.1} GB)",
            files.len(),
            size as f64 / 1e9
        );
        for file in files {
            let target = staging.join(relative_path(&file.file_name)?);
            if file
                .size
                .is_some_and(|size| fs::metadata(&target).is_ok_and(|m| m.len() == size))
            {
                continue;
            }
            self.download_file(&id, file, &target, &mut download.files)
                .await?;
        }

        fs::create_dir_all(&staging)?;
        fs::write(
            staging.join("README.md"),
            download.description_text.unwrap_or_default(),
        )?;
        fs::write(
            staging.join("LICENSE.txt"),
            download.license_text.unwrap_or_default(),
        )?;
        let toml = toml::to_string(&download.description)
            .map_err(|e| GeneBearError::Other(format!("Cannot write description of {id}: {e}")))?;
        fs::write(staging.join("description.toml"), toml)?;
        fs::rename(&staging, &path)?;
        info!("Installed {id} at {}", path.display());
        Ok(Database {
            path,
            description: download.description,
        })
    }

    async fn request_download(&self, id: &DatabaseId) -> Result<Download, GeneBearError> {
        let body = serde_json::json!({
            "owner": id.owner,
            "name": id.name,
            "version": id.version,
            "format": "parquet",
        });
        let request = self
            .http
            .post(format!("{}/hub/download", self.base_url))
            .basic_auth(&self.email, Some(&self.api_key));
        Ok(check(request.json(&body).send().await?)
            .await?
            .json()
            .await?)
    }

    /// Download a file to `path`, renewing the download links once if the storage rejects
    /// them since they expire.
    async fn download_file(
        &self,
        id: &DatabaseId,
        file: &DatabaseFile,
        path: &Path,
        links: &mut HashMap<String, String>,
    ) -> Result<(), GeneBearError> {
        let checksum = u32::from_str_radix(&file.xxh32sum, 16).map_err(|_| {
            GeneBearError::Other(format!(
                "GeneBe Hub sent an invalid checksum for {}",
                file.file_name
            ))
        })?;
        let mut renewed = false;
        let mut attempt = 0;
        loop {
            let url = links.get(&file.file_name).ok_or_else(|| {
                GeneBearError::Other(format!("GeneBe Hub sent no link for {}", file.file_name))
            })?;
            match download_once(&self.http, url, file, checksum, path).await {
                Ok(()) => return Ok(()),
                Err(GeneBearError::Http(e)) if !renewed && is_client_error(&e) => {
                    warn!(
                        "Download link of {} was rejected, renewing: {e}",
                        file.file_name
                    );
                    *links = self.request_download(id).await?.files;
                    renewed = true;
                }
                Err(e) if attempt + 1 < ATTEMPTS && retryable(&e) => {
                    attempt += 1;
                    warn!("Download of {} failed, retrying: {e}", file.file_name);
                    tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Download a file next to `path` and move it there once size and checksum match.
async fn download_once(
    http: &Client,
    url: &str,
    file: &DatabaseFile,
    checksum: u32,
    path: &Path,
) -> Result<(), GeneBearError> {
    // Download links are credentials, so they must not end up in errors and logs. Neither
    // do the error messages of the storage, which contain the signature of the link.
    let mut response = http
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(reqwest::Error::without_url)?;
    fs::create_dir_all(path.parent().unwrap())?;
    let mut partial = path.as_os_str().to_owned();
    partial.push(".part");
    let mut out = tokio::fs::File::create(&partial).await?;
    let mut hasher = Xxh32::new(0);
    let mut size = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        hasher.update(&chunk);
        size += chunk.len() as u64;
        out.write_all(&chunk).await?;
    }
    out.sync_all().await?;
    if file.size.is_some_and(|expected| expected != size) || hasher.digest() != checksum {
        fs::remove_file(&partial)?;
        return Err(GeneBearError::Checksum {
            file: file.file_name.clone(),
        });
    }
    fs::rename(&partial, path)?;
    Ok(())
}

/// Lock the file, waiting while another process holds the lock.
async fn lock(path: &Path) -> Result<fs::File, GeneBearError> {
    let file = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => {
            info!("Waiting for another pull to release {}", path.display());
            let file = tokio::task::spawn_blocking(move || file.lock().map(|()| file))
                .await
                .map_err(io::Error::other)??;
            Ok(file)
        }
        Err(fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

fn is_client_error(error: &reqwest::Error) -> bool {
    error
        .status()
        .is_some_and(|status| status.is_client_error())
}

fn retryable(error: &GeneBearError) -> bool {
    match error {
        GeneBearError::Http(e) => !is_client_error(e),
        GeneBearError::Checksum { .. } => true,
        _ => false,
    }
}

/// Reject file names from the Hub that point outside the database directory.
fn relative_path(file_name: &str) -> Result<&Path, GeneBearError> {
    let path = Path::new(file_name);
    if !file_name.is_empty()
        && path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        Ok(path)
    } else {
        Err(GeneBearError::Other(format!(
            "GeneBe Hub sent an invalid file name {file_name}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use xxhash_rust::xxh32::xxh32;

    const FILES: [(&str, &[u8]); 2] = [
        ("parquet/_seq=1/part-0.parquet", b"first"),
        ("parquet/_seq=X/part-0.parquet", b"second"),
    ];

    fn hub(server: &MockServer) -> Hub {
        Hub {
            base_url: server.uri(),
            ..Hub::new("me@example.com", "key").unwrap()
        }
    }

    fn revel() -> DatabaseId {
        DatabaseId::new("@genebe", "revel")
    }

    /// Description of `@genebe/revel` as the Hub sends it, including nulls.
    fn description(version: &str) -> serde_json::Value {
        json!({
            "owner": "@genebe",
            "name": "revel",
            "version": version,
            "title": "REVEL",
            "genome": "GRCh38",
            "species": "homo_sapiens",
            "type": "VARIANT",
            "licenseType": "OTHER",
            "licenseAcceptRequired": false,
            "labels": null,
            "columns": [{"name": "score", "type": "float32", "description": "REVEL score", "children": null}],
            "files": FILES.iter().map(|(name, content)| json!({
                "fileName": name,
                "size": content.len(),
                "xxh32sum": format!("{:08x}", xxh32(content, 0)),
                "md5sum": null,
            })).collect::<Vec<_>>(),
        })
    }

    /// Database entity as the Hub sends it.
    fn entity(version: &str) -> serde_json::Value {
        let mut entity = description(version);
        entity["id"] = format!("@genebe/revel:{version}").into();
        entity["status"] = "ACTIVE".into();
        entity
    }

    async fn serve_entity(server: &MockServer, entity: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/hub/owner/@genebe/name/revel"))
            .respond_with(ResponseTemplate::new(200).set_body_json(entity))
            .mount(server)
            .await;
    }

    async fn serve_description(server: &MockServer, version: &str) {
        serve_entity(server, entity(version)).await;
    }

    async fn serve_links(server: &MockServer, version: &str, requests: u64) {
        let files: serde_json::Map<String, serde_json::Value> = FILES
            .iter()
            .map(|(name, _)| {
                let url = format!("{}/files/{version}/{name}", server.uri());
                (name.to_string(), url.into())
            })
            .collect();
        Mock::given(method("POST"))
            .and(path("/hub/download"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "description": description(version),
                "files": files,
                "format": "parquet",
                "descriptionText": "## REVEL",
                "licenseText": "Free for non-commercial use",
            })))
            .expect(requests)
            .mount(server)
            .await;
    }

    async fn serve_file(
        server: &MockServer,
        version: &str,
        index: usize,
        content: &[u8],
        requests: u64,
    ) {
        Mock::given(method("GET"))
            .and(path(format!("/files/{version}/{}", FILES[index].0)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content))
            .expect(requests)
            .mount(server)
            .await;
    }

    /// Serves `@genebe/revel` in the given version, expecting each file to be downloaded
    /// `requests` times.
    async fn serve(server: &MockServer, version: &str, requests: u64) {
        serve_description(server, version).await;
        serve_links(server, version, requests).await;
        for (index, (_, content)) in FILES.iter().enumerate() {
            serve_file(server, version, index, content, requests).await;
        }
    }

    #[test]
    fn database_id_parses_and_displays() {
        let id: DatabaseId = "@genebe/revel:0.0.1".parse().unwrap();
        assert_eq!(id, revel().with_version("0.0.1"));
        assert_eq!(id.to_string(), "@genebe/revel:0.0.1");

        let id: DatabaseId = "@genebe/clinvar".parse().unwrap();
        assert_eq!(id, DatabaseId::new("@genebe", "clinvar"));
        assert_eq!(id.to_string(), "@genebe/clinvar");

        let id: DatabaseId = "@genebe/alfa:0.0.3-20260205170148".parse().unwrap();
        assert_eq!(id.version.as_deref(), Some("0.0.3-20260205170148"));
    }

    #[test]
    fn database_id_rejects_invalid_ids() {
        for id in [
            "revel",
            "@genebe/",
            "/revel",
            "@genebe/revel:",
            "../x/revel",
            "@genebe/re/vel",
            "@genebe/.revel",
            "@genebe/revel:../1",
        ] {
            assert!(id.parse::<DatabaseId>().is_err(), "{id}");
        }
    }

    #[test]
    fn file_names_stay_inside_the_database() {
        assert!(relative_path("parquet/_seq=1/part-0.parquet").is_ok());
        for name in [
            "",
            "/etc/passwd",
            "../revel/x.parquet",
            "parquet/../../x",
            "./x",
        ] {
            assert!(relative_path(name).is_err(), "{name}");
        }
    }

    #[tokio::test]
    async fn describe_fetches_description() {
        let server = MockServer::start().await;
        serve_description(&server, "0.0.2").await;
        Mock::given(method("GET"))
            .and(path("/hub/owner/@genebe/name/revel/version/0.0.1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(entity("0.0.1")))
            .mount(&server)
            .await;
        let hub = hub(&server);

        let newest = hub.describe(&revel()).await.unwrap();
        let older = hub.describe(&revel().with_version("0.0.1")).await.unwrap();

        assert_eq!(newest.id(), revel().with_version("0.0.2"));
        assert_eq!(older.id(), revel().with_version("0.0.1"));
        assert_eq!(newest.status.as_deref(), Some("ACTIVE"));
        assert_eq!(newest.genome.as_deref(), Some("GRCh38"));
        assert_eq!(newest.columns[0].name, "score");
        assert_eq!(newest.files.len(), 2);
        assert_eq!(newest.files[0].size, Some(5));
    }

    #[tokio::test]
    async fn pull_installs_database() {
        let server = MockServer::start().await;
        serve(&server, "0.0.1", 1).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());

        let database = hub(&server).pull(&revel(), &store).await.unwrap();

        let path = dir.path().join("@genebe/revel/0.0.1");
        assert_eq!(database.path, path);
        assert_eq!(database.description.version, "0.0.1");
        for (name, content) in FILES {
            assert_eq!(fs::read(path.join(name)).unwrap(), content);
        }
        let read = |name| fs::read_to_string(path.join(name)).unwrap();
        assert_eq!(read("README.md"), "## REVEL");
        assert_eq!(read("LICENSE.txt"), "Free for non-commercial use");
        let description: Description = toml::from_str(&read("description.toml")).unwrap();
        assert_eq!(description, database.description);
        assert_eq!(store.get(&revel()).unwrap().description, description);
        server.verify().await;
    }

    #[tokio::test]
    async fn pull_skips_installed_versions() {
        let server = MockServer::start().await;
        serve(&server, "0.0.1", 1).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());

        hub(&server).pull(&revel(), &store).await.unwrap();
        let database = hub(&server).pull(&revel(), &store).await.unwrap();

        assert_eq!(database.description.version, "0.0.1");
        server.verify().await;
    }

    #[tokio::test]
    async fn parallel_pulls_download_once() {
        let server = MockServer::start().await;
        serve(&server, "0.0.1", 1).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let (hub, id) = (hub(&server), revel());

        let (first, second) = tokio::join!(hub.pull(&id, &store), hub.pull(&id, &store));

        assert_eq!(first.unwrap().path, second.unwrap().path);
        server.verify().await;
    }

    #[tokio::test]
    async fn pull_resumes_unfinished_downloads() {
        let server = MockServer::start().await;
        serve_description(&server, "0.0.1").await;
        serve_links(&server, "0.0.1", 1).await;
        serve_file(&server, "0.0.1", 0, FILES[0].1, 0).await;
        serve_file(&server, "0.0.1", 1, FILES[1].1, 1).await;
        let dir = TempDir::new().unwrap();
        let finished = dir.path().join("@genebe/revel/.0.0.1").join(FILES[0].0);
        fs::create_dir_all(finished.parent().unwrap()).unwrap();
        fs::write(&finished, FILES[0].1).unwrap();
        let store = Store::new(dir.path());
        assert!(store.get(&revel()).is_err());

        hub(&server).pull(&revel(), &store).await.unwrap();

        assert!(store.get(&revel()).is_ok());
        server.verify().await;
    }

    #[tokio::test]
    async fn pull_rejects_corrupt_downloads() {
        let server = MockServer::start().await;
        serve_description(&server, "0.0.1").await;
        serve_links(&server, "0.0.1", 1).await;
        serve_file(&server, "0.0.1", 0, b"corrupt", ATTEMPTS as u64).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());

        let result = hub(&server).pull(&revel(), &store).await;

        assert!(matches!(result, Err(GeneBearError::Checksum { .. })));
        assert!(store.get(&revel()).is_err());
        server.verify().await;
    }

    #[tokio::test]
    async fn pull_renews_rejected_links() {
        let server = MockServer::start().await;
        serve_description(&server, "0.0.1").await;
        serve_links(&server, "0.0.1", 2).await;
        Mock::given(method("GET"))
            .and(path(format!("/files/0.0.1/{}", FILES[0].0)))
            .respond_with(ResponseTemplate::new(403))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        serve_file(&server, "0.0.1", 0, FILES[0].1, 1).await;
        serve_file(&server, "0.0.1", 1, FILES[1].1, 1).await;
        let dir = TempDir::new().unwrap();

        hub(&server)
            .pull(&revel(), &Store::new(dir.path()))
            .await
            .unwrap();

        server.verify().await;
    }

    #[tokio::test]
    async fn pull_retries_failed_downloads() {
        let server = MockServer::start().await;
        serve_description(&server, "0.0.1").await;
        serve_links(&server, "0.0.1", 1).await;
        Mock::given(method("GET"))
            .and(path(format!("/files/0.0.1/{}", FILES[0].0)))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        serve_file(&server, "0.0.1", 0, FILES[0].1, 1).await;
        serve_file(&server, "0.0.1", 1, FILES[1].1, 1).await;
        let dir = TempDir::new().unwrap();

        hub(&server)
            .pull(&revel(), &Store::new(dir.path()))
            .await
            .unwrap();

        server.verify().await;
    }

    #[tokio::test]
    async fn pull_refuses_inactive_versions() {
        let server = MockServer::start().await;
        let mut entity = entity("0.0.1");
        entity["status"] = "DURING_UPLOAD".into();
        serve_entity(&server, entity).await;
        serve_links(&server, "0.0.1", 0).await;
        let dir = TempDir::new().unwrap();

        let result = hub(&server).pull(&revel(), &Store::new(dir.path())).await;

        assert!(result.unwrap_err().to_string().contains("DURING_UPLOAD"));
        server.verify().await;
    }

    #[tokio::test]
    async fn pull_refuses_license_acceptance() {
        let server = MockServer::start().await;
        let mut entity = entity("0.0.1");
        entity["licenseAcceptRequired"] = true.into();
        serve_entity(&server, entity).await;
        serve_links(&server, "0.0.1", 0).await;
        let dir = TempDir::new().unwrap();

        let result = hub(&server).pull(&revel(), &Store::new(dir.path())).await;

        assert!(result.unwrap_err().to_string().contains("license"));
        server.verify().await;
    }
}
