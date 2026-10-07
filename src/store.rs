//! Local store of GeneBe Hub databases, laid out like the store of the GeneBe client
//! (`{owner}/{name}/{version}`) so that both can use the same downloads.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use duckdb::{params, Connection};
use tracing::{debug, warn};

use crate::error::GeneBearError;
use crate::hub::Database;
use crate::models::{AnnotatedVariant, Genome, Spdi, Variant};

/// A field of [`AnnotatedVariant`] that is read from a column of a Hub database.
struct Field {
    database: &'static str,
    column: &'static str,
    field: fn(&mut AnnotatedVariant) -> &mut Option<f64>,
}

/// Fields filled from hg38 Hub databases. Their values agree with the GeneBe API, which takes
/// them from the same databases. There are none for hg19, since the API lifts hg19 variants
/// over to hg38 and thus differs from e.g. `@genebe/revel_hg19`.
const FIELDS: [Field; 5] = [
    Field {
        database: "revel",
        column: "score",
        field: |a| &mut a.revel_score,
    },
    Field {
        database: "alpha_missense",
        column: "am_pathogenicity",
        field: |a| &mut a.alphamissense_score,
    },
    Field {
        database: "spliceai",
        column: "max",
        field: |a| &mut a.spliceai_max_score,
    },
    Field {
        database: "gnomad_exomes4",
        column: "AF",
        field: |a| &mut a.gnomad_exomes_af,
    },
    Field {
        database: "gnomad_genomes4",
        column: "AF",
        field: |a| &mut a.gnomad_genomes_af,
    },
];

const OWNER: &str = "@genebe";

/// A database installed in a [`Store`].
#[derive(Debug, Clone)]
pub struct Installed {
    pub path: PathBuf,
    pub database: Database,
}

/// Directory with GeneBe Hub databases.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// A store at the given directory, which does not need to exist yet.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Store { root: root.into() }
    }

    /// The directory the GeneBe client uses:
    /// `~/Library/Application Support/GeneBe/data/annotations` on macOS,
    /// `%LOCALAPPDATA%\GeneBe\data\annotations` on Windows and
    /// `$XDG_DATA_HOME/genebe/annotations` (by default `~/.local/share/genebe/annotations`)
    /// elsewhere.
    pub fn default_root() -> Option<PathBuf> {
        let home = std::env::home_dir()?;
        let data = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA")
                .map_or(home, PathBuf::from)
                .join("GeneBe")
                .join("data")
        } else if cfg!(target_os = "macos") {
            home.join("Library")
                .join("Application Support")
                .join("GeneBe")
                .join("data")
        } else {
            std::env::var_os("XDG_DATA_HOME")
                .filter(|dir| !dir.is_empty())
                .map_or_else(|| home.join(".local").join("share"), PathBuf::from)
                .join("genebe")
        };
        Some(data.join("annotations"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn path(&self, owner: &str, name: &str, version: &str) -> PathBuf {
        self.root.join(owner).join(name).join(version)
    }

    /// The installed version of a database.
    pub(crate) fn get(
        &self,
        owner: &str,
        name: &str,
        version: &str,
    ) -> Result<Option<Installed>, GeneBearError> {
        let path = self.path(owner, name, version);
        let description = path.join("description.toml");
        if !description.exists() {
            return Ok(None);
        }
        let database = toml::from_str(&fs::read_to_string(&description)?).map_err(|e| {
            GeneBearError::Other(format!("Cannot read {}: {e}", description.display()))
        })?;
        Ok(Some(Installed { path, database }))
    }

    /// The newest installed version of a database.
    pub(crate) fn newest(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<Option<Installed>, GeneBearError> {
        let versions = visible_directories(&self.root.join(owner).join(name))?;
        // Versions that are not semantic versions count as the oldest.
        let newest = versions
            .iter()
            .max_by_key(|version| (semver::Version::parse(version).ok(), *version));
        match newest {
            Some(version) => match self.get(owner, name, version)? {
                Some(installed) => Ok(Some(installed)),
                None => Err(GeneBearError::Other(format!(
                    "{} has no description.toml",
                    self.path(owner, name, version).display()
                ))),
            },
            None => Ok(None),
        }
    }

    /// All installed database versions. Directories without a readable `description.toml`
    /// are skipped.
    pub fn installed(&self) -> Result<Vec<Installed>, GeneBearError> {
        let mut installed = Vec::new();
        for owner in visible_directories(&self.root)? {
            for name in visible_directories(&self.root.join(&owner))? {
                for version in visible_directories(&self.root.join(&owner).join(&name))? {
                    match self.get(&owner, &name, &version) {
                        Ok(Some(database)) => installed.push(database),
                        Ok(None) => {}
                        Err(e) => warn!("Skipping {owner}/{name}:{version}: {e}"),
                    }
                }
            }
        }
        Ok(installed)
    }

    /// Annotate hg38 variants from the installed databases, without contacting GeneBe.
    /// Results are returned in the same order as `variants`.
    ///
    /// Fills `revel_score`, `alphamissense_score`, `spliceai_max_score`, `gnomad_exomes_af`
    /// and `gnomad_genomes_af` from `@genebe/revel`, `@genebe/alpha_missense`,
    /// `@genebe/spliceai`, `@genebe/gnomad_exomes4` and `@genebe/gnomad_genomes4`. Fields of
    /// databases that are not installed stay empty. Reference alleles are not checked
    /// against the genome. Variants that cannot be looked up, e.g. indels that are not
    /// left-aligned, get a `warning`.
    ///
    /// Hub databases are read in blocks of whole chromosomes, so annotate variants in large
    /// batches rather than one by one. This blocks, use `tokio::task::spawn_blocking` in
    /// async code.
    pub fn annotate_variants(
        &self,
        variants: &[Variant],
        genome: Genome,
    ) -> Result<Vec<AnnotatedVariant>, GeneBearError> {
        if genome != Genome::Hg38 {
            return Err(GeneBearError::Other(format!(
                "Local annotation is only available for hg38, not {}",
                genome.as_str()
            )));
        }
        let keys: Vec<Option<Spdi>> = variants.iter().map(Variant::to_spdi).collect();
        let mut annotations: Vec<AnnotatedVariant> = variants
            .iter()
            .zip(&keys)
            .map(|(variant, key)| AnnotatedVariant {
                chr: Some(variant.chr.clone()),
                pos: Some(variant.pos),
                ref_allele: Some(variant.ref_allele.clone()),
                alt: Some(variant.alt_allele.clone()),
                warning: key.is_none().then(|| {
                    "Not looked up in GeneBe Hub databases: unsupported chromosome or alleles, \
                     or indel not left-aligned"
                        .into()
                }),
                ..Default::default()
            })
            .collect();

        let conn = Connection::open_in_memory()?;
        for field in &FIELDS {
            let installed = match self.newest(OWNER, field.database) {
                Ok(Some(installed)) => installed,
                Ok(None) => {
                    debug!("{OWNER}/{} is not installed", field.database);
                    continue;
                }
                Err(e) => {
                    warn!("Skipping {OWNER}/{}: {e}", field.database);
                    continue;
                }
            };
            if !installed
                .database
                .columns
                .iter()
                .any(|c| c.name == field.column)
            {
                warn!(
                    "Skipping {}: no column {}",
                    installed.database.id(),
                    field.column
                );
                continue;
            }
            debug!(
                "Annotating {} from {}",
                field.column,
                installed.database.id()
            );
            let parquet = installed.path.join("parquet");
            for (index, value) in lookup(&conn, &parquet, field.column, &keys)? {
                *(field.field)(&mut annotations[index]) = value;
            }
        }
        Ok(annotations)
    }
}

/// Look up a column for the given variants in a hive-partitioned parquet database and
/// return the values by index into `keys`.
fn lookup(
    conn: &Connection,
    parquet: &Path,
    column: &str,
    keys: &[Option<Spdi>],
) -> Result<Vec<(usize, Option<f64>)>, GeneBearError> {
    conn.execute_batch(
        "CREATE OR REPLACE TEMP TABLE query \
         (idx UINTEGER, seq VARCHAR, pos BIGINT, del BIGINT, ins VARCHAR)",
    )?;
    let mut appender = conn.appender("query")?;
    for (index, key) in keys.iter().enumerate() {
        if let Some(key) = key {
            appender.append_row(params![
                index as u32,
                key.seq,
                key.pos as i64,
                key.del as i64,
                key.ins
            ])?;
        }
    }
    appender.flush()?;
    drop(appender);

    // Querying one chromosome at a time with the range of positions lets DuckDB skip the
    // parts of the files that cannot match.
    let mut ranges: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
    for key in keys.iter().flatten() {
        let range = ranges.entry(key.seq.as_str()).or_insert((key.pos, key.pos));
        *range = (range.0.min(key.pos), range.1.max(key.pos));
    }
    let mut values = Vec::new();
    for (seq, (start, end)) in ranges {
        let partition = parquet.join(format!("_seq={seq}"));
        // Partitions without files would make DuckDB fail.
        if !has_parquet_files(&partition) {
            continue;
        }
        // Values are read through their shortest decimal representation, so float32
        // columns give the same numbers as the API, e.g. 0.034 instead of
        // 0.03400000184774399. Keys are unique in Hub databases, max() is only a safeguard.
        let sql = format!(
            "SELECT query.idx, max(TRY_CAST(CAST(t.\"{column}\" AS VARCHAR) AS DOUBLE)) \
             FROM read_parquet({}) t \
             JOIN query ON query.seq = ? AND t._pos = query.pos AND t._del = query.del \
             AND coalesce(t._ins, '') = query.ins \
             WHERE t._pos BETWEEN ? AND ? \
             GROUP BY query.idx",
            sql_string(&partition.join("*.parquet")),
        );
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map(params![seq, start, end], |row| {
            Ok((row.get::<_, u32>(0)? as usize, row.get(1)?))
        })?;
        for row in rows {
            values.push(row?);
        }
    }
    Ok(values)
}

fn sql_string(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

fn has_parquet_files(directory: &Path) -> bool {
    fs::read_dir(directory).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "parquet"))
    })
}

/// Names of the subdirectories, without hidden ones and the temporary directory of the
/// GeneBe client, which holds unfinished downloads.
fn visible_directories(directory: &Path) -> Result<Vec<String>, GeneBearError> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().is_dir() && !name.starts_with('.') && name != "__temp" {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{Column, DatabaseId};
    use tempfile::TempDir;

    type Row<'a> = (&'a str, i32, i32, Option<&'a str>, f32);

    fn install(store: &Store, name: &str, column: &str, rows: &[Row]) {
        install_version(store, name, "0.0.1", column, rows);
    }

    /// Writes a VARIANT database with one float column into the store, partitioned like Hub
    /// databases.
    fn install_version(store: &Store, name: &str, version: &str, column: &str, rows: &[Row]) {
        let path = store.path(OWNER, name, version);
        fs::create_dir_all(&path).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE data \
             (_seq VARCHAR, _pos INTEGER, _del INTEGER, _ins VARCHAR, \"{column}\" FLOAT)"
        ))
        .unwrap();
        let mut appender = conn.appender("data").unwrap();
        for row in rows {
            appender
                .append_row(params![row.0, row.1, row.2, row.3, row.4])
                .unwrap();
        }
        appender.flush().unwrap();
        drop(appender);
        conn.execute_batch(&format!(
            "COPY data TO {} (FORMAT PARQUET, PARTITION_BY (_seq))",
            sql_string(&path.join("parquet"))
        ))
        .unwrap();
        let database = Database {
            owner: OWNER.into(),
            name: name.into(),
            version: version.into(),
            kind: Some("VARIANT".into()),
            columns: vec![Column {
                name: column.into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        fs::write(
            path.join("description.toml"),
            toml::to_string(&database).unwrap(),
        )
        .unwrap();
    }

    fn install_revel(store: &Store) {
        install(
            store,
            "revel",
            "score",
            &[
                ("22", 99, 1, Some("T"), 0.034),
                ("22", 99, 1, Some("G"), 0.5),
                ("22", 200, 0, Some("TT"), 0.25),
                ("22", 300, 2, Some(""), 0.75),
                ("22", 400, 1, None, 0.625),
                ("X", 99, 1, Some("T"), 0.9),
            ],
        );
    }

    #[test]
    fn annotate_variants_reads_installed_databases() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);
        let variants = [
            Variant::new("chrX", 100, "A", "T"),
            Variant::new("22", 100, "A", "T"),
            Variant::new("22", 100, "A", "C"),
            Variant::new("22", 200, "C", "CTT"),
            Variant::new("22", 300, "GCA", "G"),
            Variant::new("22", 400, "TA", "T"),
            Variant::new("22", 100, "A", "T"),
            Variant::new("21", 100, "A", "T"),
        ];

        let annotations = store.annotate_variants(&variants, Genome::Hg38).unwrap();

        let scores: Vec<_> = annotations.iter().map(|a| a.revel_score).collect();
        assert_eq!(
            scores,
            [
                Some(0.9),
                Some(0.034),
                None,
                Some(0.25),
                Some(0.75),
                Some(0.625),
                Some(0.034),
                None
            ]
        );
        assert_eq!(annotations[0].chr.as_deref(), Some("chrX"));
        assert_eq!(annotations[0].pos, Some(100));
        assert_eq!(annotations[0].alt.as_deref(), Some("T"));
        assert!(annotations.iter().all(|a| a.warning.is_none()));
        assert!(annotations.iter().all(|a| a.alphamissense_score.is_none()));
    }

    #[test]
    fn annotate_variants_handles_many_variants() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);
        let mut variants = vec![Variant::new("22", 1000, "A", "T"); 2500];
        variants[0] = Variant::new("22", 100, "A", "T");
        variants[1001] = Variant::new("22", 100, "A", "G");
        variants[2499] = Variant::new("X", 100, "A", "T");

        let annotations = store.annotate_variants(&variants, Genome::Hg38).unwrap();

        let hits: Vec<_> = annotations
            .iter()
            .enumerate()
            .filter_map(|(i, a)| a.revel_score.map(|score| (i, score)))
            .collect();
        assert_eq!(hits, [(0, 0.034), (1001, 0.5), (2499, 0.9)]);
    }

    #[test]
    fn annotate_variants_warns_on_unsupported() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);
        let variants = [
            Variant::new("22", 100, "AA", "A"),
            Variant::new("22", 100, "A", "N"),
        ];

        let annotations = store.annotate_variants(&variants, Genome::Hg38).unwrap();

        assert!(annotations.iter().all(|a| a.warning.is_some()));
        assert!(annotations.iter().all(|a| a.revel_score.is_none()));
    }

    #[test]
    fn annotate_variants_without_databases() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path().join("missing"));

        let annotations = store
            .annotate_variants(&[Variant::new("22", 100, "A", "T")], Genome::Hg38)
            .unwrap();

        assert_eq!(annotations.len(), 1);
        assert_eq!(annotations[0].chr.as_deref(), Some("22"));
        assert!(annotations[0].revel_score.is_none());
        assert!(annotations[0].warning.is_none());
    }

    #[test]
    fn annotate_variants_refuses_other_genomes() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);
        let variants = [Variant::new("22", 100, "A", "T")];

        assert!(store.annotate_variants(&variants, Genome::Hg19).is_err());
        assert!(store.annotate_variants(&variants, Genome::T2t).is_err());
    }

    #[test]
    fn annotate_variants_handles_paths_with_special_characters() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path().join("Application Support").join("Felix's"));
        install_revel(&store);

        let annotations = store
            .annotate_variants(&[Variant::new("22", 100, "A", "T")], Genome::Hg38)
            .unwrap();

        assert_eq!(annotations[0].revel_score, Some(0.034));
    }

    #[test]
    fn annotate_variants_skips_databases_without_column() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install(&store, "revel", "other", &[("22", 99, 1, Some("T"), 0.5)]);

        let annotations = store
            .annotate_variants(&[Variant::new("22", 100, "A", "T")], Genome::Hg38)
            .unwrap();

        assert_eq!(annotations[0].revel_score, None);
    }

    #[test]
    fn annotate_variants_skips_unreadable_databases() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);
        install(&store, "spliceai", "max", &[("22", 99, 1, Some("T"), 0.5)]);
        let description = store
            .path(OWNER, "spliceai", "0.0.1")
            .join("description.toml");
        fs::write(description, "columns = 1").unwrap();

        let annotations = store
            .annotate_variants(&[Variant::new("22", 100, "A", "T")], Genome::Hg38)
            .unwrap();

        assert_eq!(annotations[0].revel_score, Some(0.034));
        assert_eq!(annotations[0].spliceai_max_score, None);
    }

    #[test]
    fn lookup_takes_max_of_duplicates() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install(
            &store,
            "spliceai",
            "max",
            &[
                ("22", 99, 1, Some("T"), 0.25),
                ("22", 99, 1, Some("T"), 0.5),
            ],
        );
        let keys = [Variant::new("22", 100, "A", "T").to_spdi()];
        let parquet = store.path(OWNER, "spliceai", "0.0.1").join("parquet");

        let conn = Connection::open_in_memory().unwrap();
        let values = lookup(&conn, &parquet, "max", &keys).unwrap();

        assert_eq!(values, [(0, Some(0.5))]);
    }

    #[test]
    fn annotate_variants_skips_chromosomes_without_data() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install(&store, "revel", "score", &[("1", 99, 1, Some("T"), 0.5)]);
        let variants = [
            Variant::new("X", 100, "A", "T"),
            Variant::new("1", 100, "A", "T"),
        ];

        let annotations = store.annotate_variants(&variants, Genome::Hg38).unwrap();

        assert_eq!(annotations[0].revel_score, None);
        assert_eq!(annotations[1].revel_score, Some(0.5));
    }

    #[test]
    fn annotate_variants_uses_newest_version() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        for (version, score) in [("0.0.2", 0.25), ("0.0.10", 0.5), ("0.0.10-1", 0.125)] {
            install_version(
                &store,
                "revel",
                version,
                "score",
                &[("22", 99, 1, Some("T"), score)],
            );
        }

        let annotations = store
            .annotate_variants(&[Variant::new("22", 100, "A", "T")], Genome::Hg38)
            .unwrap();

        assert_eq!(annotations[0].revel_score, Some(0.5));
    }

    #[test]
    fn newest_reports_broken_versions() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);
        fs::create_dir_all(store.path(OWNER, "revel", "0.0.2")).unwrap();

        assert!(store.newest(OWNER, "revel").is_err());
    }

    #[test]
    fn installed_ignores_unfinished_downloads() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);
        let temp = dir.path().join("__temp/genebears").join(OWNER);
        fs::create_dir_all(temp.join("spliceai/0.0.1/parquet")).unwrap();
        fs::write(temp.join("spliceai.lock"), "").unwrap();
        fs::create_dir_all(dir.path().join(OWNER).join(".hidden/0.0.1")).unwrap();
        fs::create_dir_all(dir.path().join(OWNER).join("empty/0.0.1")).unwrap();

        let installed = store.installed().unwrap();

        assert_eq!(installed.len(), 1);
        assert_eq!(
            installed[0].database.id(),
            "@genebe/revel:0.0.1".parse::<DatabaseId>().unwrap()
        );
        assert_eq!(installed[0].path, dir.path().join("@genebe/revel/0.0.1"));
    }

    /// Checks that the databases and columns used for annotation still exist on the Hub.
    #[tokio::test]
    #[ignore = "needs the GeneBe Hub"]
    async fn fields_exist_on_hub() {
        let hub = crate::GeneBears::new(crate::ClientConfig::default())
            .unwrap()
            .hub();
        for field in &FIELDS {
            let id = DatabaseId::new(OWNER, field.database);
            let database = hub.database(&id).await.unwrap();
            assert!(
                database.columns.iter().any(|c| c.name == field.column),
                "{id} has no column {}",
                field.column
            );
        }
    }
}
