//! Downloading databases from the [GeneBe Hub](https://genebe.net/hub), a repository of
//! variant annotation databases in parquet format.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use reqwest::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};
use xxhash_rust::xxh32::Xxh32;

use crate::client::check;
use crate::error::GeneBearError;
use crate::store::{Installed, Store};

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
    fn validate(&self) -> Result<(), GeneBearError> {
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
pub struct Database {
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

impl Database {
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
    description: Database,
    #[serde(default)]
    files: HashMap<String, String>,
    description_text: Option<String>,
    license_text: Option<String>,
}

/// Client for the GeneBe Hub, created with [`crate::GeneBears::hub`].
#[derive(Clone)]
pub struct Hub {
    pub(crate) http: Client,
    pub(crate) base_url: String,
    pub(crate) credentials: Option<(String, String)>,
}

impl Hub {
    /// Fetch the description of a database version, the newest active one if `id` has
    /// none.
    pub async fn database(&self, id: &DatabaseId) -> Result<Database, GeneBearError> {
        id.validate()?;
        let mut url = format!("{}/hub/owner/{}/name/{}", self.base_url, id.owner, id.name);
        if let Some(version) = &id.version {
            url = format!("{url}/version/{version}");
        }
        let response = self.authorized(self.http.get(&url)).send().await?;
        Ok(check(response).await?.json().await?)
    }

    /// Download a database version into the local store, unless it is installed already.
    /// Other versions stay installed. Requires credentials and asks the Hub for the version
    /// even if it is installed.
    ///
    /// Files are verified against their checksums and only become visible in the store once
    /// the whole database is downloaded. An interrupted pull resumes with the files it did
    /// not finish.
    pub async fn pull(&self, id: &DatabaseId, store: &Store) -> Result<Installed, GeneBearError> {
        if self.credentials.is_none() {
            return Err(GeneBearError::Other(
                "Downloading from the GeneBe Hub requires an email and API key".into(),
            ));
        }
        let database = self.database(id).await?;
        let requested = id;
        let id = database.id();
        if id.owner != requested.owner
            || id.name != requested.name
            || requested
                .version
                .as_ref()
                .is_some_and(|v| *v != database.version)
        {
            return Err(GeneBearError::Other(format!(
                "GeneBe Hub sent {id} when asked for {requested}"
            )));
        }
        id.validate()?;
        if database.status.as_deref() != Some("ACTIVE") {
            return Err(GeneBearError::Other(format!(
                "{id} cannot be downloaded, its status is {}",
                database.status.as_deref().unwrap_or("unknown")
            )));
        }
        if database.license_accept_required {
            return Err(GeneBearError::LicenseNotAccepted { id: id.to_string() });
        }

        let target = store.path(&id.owner, &id.name, &database.version);
        // Unfinished downloads are kept in the temporary directory of the GeneBe client,
        // which it does not list as installed databases.
        let work = store
            .root()
            .join("__temp")
            .join("genebears")
            .join(&id.owner);
        let partial = work.join(&id.name);
        let staging = partial.join(&database.version);
        fs::create_dir_all(&work)?;
        let _lock = lock(work.join(format!("{}.lock", id.name))).await?;

        // Unfinished downloads of other versions are dropped, those of this version resumed.
        if let Ok(entries) = fs::read_dir(&partial) {
            for entry in entries.flatten() {
                if entry.path() != staging {
                    remove(&entry.path()).await;
                }
            }
        }
        if let Some(installed) = complete(store, &id)? {
            info!("{id} is already installed");
            remove(&partial).await;
            return Ok(installed);
        }

        let mut download = self.download(&id).await?;
        if download.description.id() != id {
            return Err(GeneBearError::Other(format!(
                "GeneBe Hub sent {} when asked for {id}",
                download.description.id()
            )));
        }
        let size: u64 = download
            .description
            .files
            .iter()
            .filter_map(|f| f.size)
            .sum();
        info!(
            "Downloading {id} ({} files, {:.1} GB)",
            download.description.files.len(),
            size as f64 / 1e9
        );
        for file in &download.description.files {
            let path = staging.join(relative_path(&file.file_name)?);
            if file
                .size
                .is_some_and(|size| fs::metadata(&path).is_ok_and(|m| m.len() == size))
            {
                continue;
            }
            self.download_file(&id, file, &path, &mut download.files)
                .await?;
        }

        fs::create_dir_all(&staging)?;
        remove_undeclared(&staging, &download.description.files)?;
        fs::write(
            staging.join("README.md"),
            download.description_text.unwrap_or_default(),
        )?;
        fs::write(
            staging.join("LICENSE.txt"),
            download.license_text.unwrap_or_default(),
        )?;
        let description = toml::to_string(&download.description)
            .map_err(|e| GeneBearError::Other(format!("Cannot write description of {id}: {e}")))?;
        fs::write(staging.join("description.toml"), description)?;

        // The GeneBe client does not take the lock and may have installed the version in the
        // meantime. Otherwise, an installed copy is incomplete.
        if let Some(installed) = complete(store, &id)? {
            remove(&partial).await;
            return Ok(installed);
        }
        if target.exists() {
            tokio::fs::remove_dir_all(&target).await?;
        }
        fs::create_dir_all(target.parent().unwrap())?;
        tokio::fs::rename(&staging, &target).await?;
        remove(&partial).await;
        info!("Installed {id} at {}", target.display());
        Ok(Installed {
            path: target,
            database: download.description,
        })
    }

    fn authorized(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.credentials {
            Some((email, api_key)) => request.basic_auth(email, Some(api_key)),
            None => request,
        }
    }

    async fn download(&self, id: &DatabaseId) -> Result<Download, GeneBearError> {
        let body = serde_json::json!({
            "owner": id.owner,
            "name": id.name,
            "version": id.version,
            "format": "parquet",
        });
        let request = self.http.post(format!("{}/hub/download", self.base_url));
        let response = self.authorized(request).json(&body).send().await?;
        Ok(check(response).await?.json().await?)
    }

    /// Download a file next to `path` and move it there once size and checksum match.
    /// Download links expire, so they are renewed once if the storage rejects them.
    async fn download_file(
        &self,
        id: &DatabaseId,
        file: &DatabaseFile,
        path: &Path,
        links: &mut HashMap<String, String>,
    ) -> Result<(), GeneBearError> {
        let mut renewed = false;
        let mut attempt = 0;
        loop {
            let url = links.get(&file.file_name).ok_or_else(|| {
                GeneBearError::Other(format!("GeneBe Hub sent no link for {}", file.file_name))
            })?;
            match fetch(&self.http, url, file, path).await {
                Ok(()) => return Ok(()),
                Err(GeneBearError::ApiClientError { status, .. }) if !renewed => {
                    warn!(
                        "Download link of {} was rejected ({status}), renewing",
                        file.file_name
                    );
                    *links = self.download(id).await?.files;
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

async fn fetch(
    http: &Client,
    url: &str,
    file: &DatabaseFile,
    path: &Path,
) -> Result<(), GeneBearError> {
    // Download links are credentials, so they must not end up in errors and logs.
    let response = http
        .get(url)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    let mut response = check(response).await?;
    fs::create_dir_all(path.parent().unwrap())?;
    let mut partial = path.as_os_str().to_owned();
    partial.push(".part");
    let mut out = tokio::fs::File::create(&partial).await?;
    let mut hasher = Xxh32::new(0);
    let mut size = 0;
    // A stalled download would otherwise hold the lock forever.
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(60), response.chunk())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "download stalled"))?
        .map_err(reqwest::Error::without_url)?
    {
        hasher.update(&chunk);
        size += chunk.len() as u64;
        out.write_all(&chunk).await?;
    }
    out.sync_all().await?;
    let checksum = u32::from_str_radix(&file.xxh32sum, 16).ok();
    if file.size.is_some_and(|expected| expected != size) || checksum != Some(hasher.digest()) {
        fs::remove_file(&partial)?;
        return Err(GeneBearError::Checksum {
            file: file.file_name.clone(),
        });
    }
    fs::rename(&partial, path)?;
    Ok(())
}

fn retryable(error: &GeneBearError) -> bool {
    match error {
        GeneBearError::Http(_)
        | GeneBearError::Checksum { .. }
        | GeneBearError::ApiServerError { .. } => true,
        GeneBearError::Io(e) => e.kind() == io::ErrorKind::TimedOut,
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

/// The installed copy of a database version if all its files exist with the expected size.
fn complete(store: &Store, id: &DatabaseId) -> Result<Option<Installed>, GeneBearError> {
    let version = id.version.as_deref().unwrap_or_default();
    let installed = store.get(&id.owner, &id.name, version)?;
    Ok(installed.filter(|installed| {
        installed.database.files.iter().all(|file| {
            relative_path(&file.file_name).is_ok_and(|name| {
                fs::metadata(installed.path.join(name))
                    .is_ok_and(|m| file.size.is_none_or(|size| m.len() == size))
            })
        })
    }))
}

/// Remove files that do not belong to the database, e.g. from an earlier download with
/// other files, since the GeneBe client reports them as errors.
fn remove_undeclared(directory: &Path, files: &[DatabaseFile]) -> Result<(), GeneBearError> {
    let declared: HashSet<PathBuf> = files
        .iter()
        .map(|file| directory.join(&file.file_name))
        .collect();
    let mut directories = vec![directory.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            if path.is_dir() {
                directories.push(path);
            } else if !declared.contains(&path) {
                fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}

/// Lock a file so that only one process downloads a database at a time.
async fn lock(path: PathBuf) -> Result<fs::File, GeneBearError> {
    let file = tokio::task::spawn_blocking(move || {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        match file.lock() {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                warn!("Cannot lock {}, continuing without: {e}", path.display())
            }
            Err(e) => return Err(e),
        }
        Ok::<_, io::Error>(file)
    })
    .await
    .map_err(|e| GeneBearError::Other(e.to_string()))??;
    Ok(file)
}

/// Remove leftovers of a pull. Failing to do so is only logged since the installed
/// database is not affected.
async fn remove(path: &Path) {
    let removed = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.is_dir() => tokio::fs::remove_dir_all(path).await,
        Ok(_) => tokio::fs::remove_file(path).await,
        Err(_) => return,
    };
    if let Err(e) = removed {
        warn!("Failed to remove {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::{any, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use xxhash_rust::xxh32::xxh32;

    const FILES: [(&str, &[u8]); 2] = [
        ("parquet/_seq=1/part-0.parquet", b"first"),
        ("parquet/_seq=X/part-0.parquet", b"second"),
    ];

    fn hub(server: &MockServer) -> Hub {
        Hub {
            http: Client::new(),
            base_url: server.uri(),
            credentials: Some(("me@example.com".into(), "key".into())),
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
    async fn database_fetches_description() {
        let server = MockServer::start().await;
        serve_description(&server, "0.0.2").await;
        Mock::given(method("GET"))
            .and(path("/hub/owner/@genebe/name/revel/version/0.0.1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(entity("0.0.1")))
            .mount(&server)
            .await;
        let hub = hub(&server);

        let newest = hub.database(&revel()).await.unwrap();
        let older = hub.database(&revel().with_version("0.0.1")).await.unwrap();

        assert_eq!(newest.id(), revel().with_version("0.0.2"));
        assert_eq!(older.id(), revel().with_version("0.0.1"));
        assert_eq!(newest.status.as_deref(), Some("ACTIVE"));
        assert_eq!(newest.genome.as_deref(), Some("GRCh38"));
        assert_eq!(newest.columns[0].name, "score");
        assert_eq!(newest.files.len(), 2);
        assert_eq!(newest.files[0].size, Some(5));
    }

    #[test]
    fn hub_uses_client_credentials() {
        let config = crate::ClientConfig::with_credentials("me@example.com", "key");
        let hub = crate::GeneBears::new(config).unwrap().hub();
        assert_eq!(
            hub.credentials,
            Some(("me@example.com".into(), "key".into()))
        );
    }

    #[tokio::test]
    async fn pull_installs_database() {
        let server = MockServer::start().await;
        serve(&server, "0.0.1", 1).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());

        let installed = hub(&server).pull(&revel(), &store).await.unwrap();

        let path = dir.path().join("@genebe/revel/0.0.1");
        assert_eq!(installed.path, path);
        assert_eq!(installed.database.version, "0.0.1");
        for (name, content) in FILES {
            assert_eq!(fs::read(path.join(name)).unwrap(), content);
        }
        let read = |name| fs::read_to_string(path.join(name)).unwrap();
        assert_eq!(read("README.md"), "## REVEL");
        assert_eq!(read("LICENSE.txt"), "Free for non-commercial use");
        let description: Database = toml::from_str(&read("description.toml")).unwrap();
        assert_eq!(description, installed.database);
        assert!(!dir.path().join("__temp/genebears/@genebe/revel").exists());
        assert_eq!(store.installed().unwrap().len(), 1);
        server.verify().await;
    }

    #[tokio::test]
    async fn pull_skips_installed_versions() {
        let server = MockServer::start().await;
        serve(&server, "0.0.1", 1).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());

        hub(&server).pull(&revel(), &store).await.unwrap();
        let installed = hub(&server).pull(&revel(), &store).await.unwrap();

        assert_eq!(installed.database.version, "0.0.1");
        server.verify().await;
    }

    #[tokio::test]
    async fn concurrent_pulls_download_once() {
        let server = MockServer::start().await;
        serve(&server, "0.0.1", 1).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let (hub, id) = (hub(&server), revel());

        let (first, second) = tokio::join!(hub.pull(&id, &store), hub.pull(&id, &store));

        assert_eq!(first.unwrap().database, second.unwrap().database);
        server.verify().await;
    }

    #[tokio::test]
    async fn pull_keeps_other_versions() {
        let server = MockServer::start().await;
        serve(&server, "0.0.1", 1).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        hub(&server).pull(&revel(), &store).await.unwrap();
        server.verify().await;
        server.reset().await;
        serve(&server, "0.0.2", 1).await;

        let installed = hub(&server).pull(&revel(), &store).await.unwrap();

        assert_eq!(installed.path, dir.path().join("@genebe/revel/0.0.2"));
        let versions: Vec<_> = store
            .installed()
            .unwrap()
            .into_iter()
            .map(|installed| installed.database.version)
            .collect();
        assert_eq!(versions, ["0.0.1", "0.0.2"]);
        server.verify().await;
    }

    #[tokio::test]
    async fn failed_pull_keeps_installed_version() {
        let server = MockServer::start().await;
        serve(&server, "0.0.1", 1).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        hub(&server).pull(&revel(), &store).await.unwrap();
        server.reset().await;
        serve_description(&server, "0.0.2").await;
        serve_links(&server, "0.0.2", 1).await;
        serve_file(&server, "0.0.2", 0, b"broken", ATTEMPTS as u64).await;

        let result = hub(&server).pull(&revel(), &store).await;

        assert!(matches!(result, Err(GeneBearError::Checksum { .. })));
        let listed = store.installed().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].database.version, "0.0.1");
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
        let partial = dir.path().join("__temp/genebears/@genebe/revel");
        let staged = partial.join("0.0.1").join(FILES[0].0);
        fs::create_dir_all(staged.parent().unwrap()).unwrap();
        fs::write(&staged, FILES[0].1).unwrap();
        // Unfinished downloads of other versions and stray files are dropped.
        fs::create_dir_all(partial.join("0.0.0")).unwrap();
        fs::write(partial.join(".DS_Store"), "").unwrap();
        fs::write(staged.with_file_name("old.parquet"), "").unwrap();

        let installed = hub(&server)
            .pull(&revel(), &Store::new(dir.path()))
            .await
            .unwrap();

        assert!(!partial.exists());
        assert!(!installed.path.join("parquet/_seq=1/old.parquet").exists());
        server.verify().await;
    }

    #[tokio::test]
    async fn pull_cleans_up_if_installed() {
        let server = MockServer::start().await;
        serve(&server, "0.0.1", 1).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        hub(&server).pull(&revel(), &store).await.unwrap();
        let partial = dir.path().join("__temp/genebears/@genebe/revel");
        fs::create_dir_all(partial.join("0.0.0")).unwrap();
        fs::write(partial.join(".DS_Store"), "").unwrap();

        let installed = hub(&server).pull(&revel(), &store).await.unwrap();

        assert_eq!(installed.database.version, "0.0.1");
        assert!(!partial.exists());
        server.verify().await;
    }

    #[tokio::test]
    async fn pull_replaces_incomplete_versions() {
        let server = MockServer::start().await;
        serve(&server, "0.0.1", 2).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        hub(&server).pull(&revel(), &store).await.unwrap();
        let path = dir.path().join("@genebe/revel/0.0.1");
        fs::write(path.join(FILES[1].0), "truncated").unwrap();

        hub(&server).pull(&revel(), &store).await.unwrap();

        assert_eq!(fs::read(path.join(FILES[1].0)).unwrap(), FILES[1].1);
        server.verify().await;
    }

    #[tokio::test]
    async fn pull_keeps_versions_it_cannot_read() {
        let server = MockServer::start().await;
        serve(&server, "0.0.1", 1).await;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        hub(&server).pull(&revel(), &store).await.unwrap();
        let path = dir.path().join("@genebe/revel/0.0.1");
        fs::write(path.join("description.toml"), "files = 1").unwrap();

        let result = hub(&server).pull(&revel(), &store).await;

        assert!(result.unwrap_err().to_string().contains("description.toml"));
        assert_eq!(fs::read(path.join(FILES[1].0)).unwrap(), FILES[1].1);
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

        assert!(matches!(
            result,
            Err(GeneBearError::LicenseNotAccepted { .. })
        ));
        server.verify().await;
    }

    #[tokio::test]
    async fn pull_refuses_unexpected_answers() {
        // Owner, name and version become paths in the store.
        let answers = [
            ("name", "clinvar", "sent @genebe/clinvar"),
            ("name", "../../revel", "sent @genebe/../../revel"),
            ("version", "../0.0.1", "Invalid GeneBe Hub database id"),
        ];
        for (field, value, error) in answers {
            let server = MockServer::start().await;
            let mut entity = entity("0.0.1");
            entity[field] = value.into();
            serve_entity(&server, entity).await;
            serve_links(&server, "0.0.1", 0).await;
            let dir = TempDir::new().unwrap();

            let result = hub(&server).pull(&revel(), &Store::new(dir.path())).await;

            assert!(result.unwrap_err().to_string().contains(error), "{field}");
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn pull_requires_credentials() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let hub = Hub {
            credentials: None,
            ..hub(&server)
        };
        let dir = TempDir::new().unwrap();

        assert!(hub.pull(&revel(), &Store::new(dir.path())).await.is_err());
        server.verify().await;
    }
}
