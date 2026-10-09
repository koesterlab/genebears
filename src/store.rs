//! Local store of GeneBe Hub databases, laid out like the store of the GeneBe client
//! (`{owner}/{name}/{version}`) so that both can use the same downloads.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use duckdb::{params, Connection};
use serde_json::{Number, Value};
use tracing::warn;

use crate::error::GeneBearError;
use crate::hub::{Database, DatabaseId};
use crate::models::{hub_source, Field, Genome, Spdi};

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

    /// The installed database with the version of `id`, or the newest one if it has none.
    pub(crate) fn find(&self, id: &DatabaseId) -> Result<Option<Installed>, GeneBearError> {
        if let Some(version) = &id.version {
            return self.get(&id.owner, &id.name, version);
        }
        let versions = visible_directories(&self.root.join(&id.owner).join(&id.name))?;
        // Versions that are not semantic versions count as the oldest.
        let newest = versions
            .iter()
            .max_by_key(|version| (semver::Version::parse(version).ok(), *version));
        match newest {
            Some(version) => match self.get(&id.owner, &id.name, version)? {
                Some(installed) => Ok(Some(installed)),
                None => Err(GeneBearError::Other(format!(
                    "{} has no description.toml",
                    self.path(&id.owner, &id.name, version).display()
                ))),
            },
            None => Ok(None),
        }
    }

    /// The installed database and column the API takes a field from.
    pub(crate) fn api_field(
        &self,
        field: &str,
        genome: Genome,
    ) -> Result<Option<(Installed, &'static str)>, GeneBearError> {
        let Some((database, column)) = hub_source(field, genome) else {
            return Ok(None);
        };
        let Some(installed) = self.find(&database.parse()?)? else {
            return Ok(None);
        };
        check_column(&installed, column, genome)?;
        Ok(Some((installed, column)))
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
}

/// Check that a database has the column and matches the genome.
pub(crate) fn check_column(
    installed: &Installed,
    column: &str,
    genome: Genome,
) -> Result<(), GeneBearError> {
    let database = &installed.database;
    if let Some(kind) = database.kind.as_deref().filter(|kind| *kind != "VARIANT") {
        return Err(GeneBearError::Other(format!(
            "{} is a {kind} database, only VARIANT databases are supported",
            database.id()
        )));
    }
    if !database.columns.iter().any(|c| c.name == column) {
        return Err(GeneBearError::UnknownField {
            field: Field::hub(database.id().to_string(), column),
        });
    }
    let expected = match genome {
        Genome::Hg38 => "GRCh38",
        Genome::Hg19 => "GRCh37",
        Genome::T2t => "T2T",
    };
    // Some databases have other values than genome builds here.
    if let Some(build @ ("GRCh37" | "GRCh38")) = database.genome.as_deref() {
        if build != expected {
            return Err(GeneBearError::GenomeMismatch {
                database: database.id(),
                genome,
            });
        }
    }
    Ok(())
}

/// Look up columns of installed databases. Returns `(variant, field, value)`, with
/// `variant` an index into `keys` and `field` the index given with the column.
pub(crate) fn lookup(
    local: &[(Installed, Vec<(usize, String)>)],
    keys: &[Option<Spdi>],
) -> Result<Vec<(usize, usize, Value)>, GeneBearError> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(
        "CREATE TEMP TABLE query (idx UINTEGER, seq VARCHAR, pos BIGINT, del BIGINT, ins VARCHAR)",
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

    let mut found = Vec::new();
    for (installed, columns) in local {
        let names: Vec<&str> = columns.iter().map(|(_, column)| column.as_str()).collect();
        for (variant, values) in
            lookup_columns(&conn, &installed.path.join("parquet"), &names, keys)?
        {
            for ((field, _), value) in columns.iter().zip(values) {
                found.push((variant, *field, value));
            }
        }
    }
    Ok(found)
}

/// Look up columns of a hive-partitioned parquet database for the variants in the `query`
/// table and return their values by index into `keys`.
fn lookup_columns(
    conn: &Connection,
    parquet: &Path,
    columns: &[&str],
    keys: &[Option<Spdi>],
) -> Result<Vec<(usize, Vec<Value>)>, GeneBearError> {
    // Querying one chromosome at a time with the range of positions lets DuckDB skip the
    // parts of the files that cannot match.
    let mut ranges: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
    for key in keys.iter().flatten() {
        let range = ranges.entry(key.seq.as_str()).or_insert((key.pos, key.pos));
        *range = (range.0.min(key.pos), range.1.max(key.pos));
    }
    let selected = columns
        .iter()
        .map(|column| format!("t.\"{}\"", column.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let mut values: Vec<(usize, Vec<Value>)> = Vec::new();
    for (seq, (start, end)) in ranges {
        let partition = parquet.join(format!("_seq={seq}"));
        // Partitions without files would make DuckDB fail.
        if !has_parquet_files(&partition) {
            continue;
        }
        // Keys are unique in Hub databases. Should one occur twice, ordering by the first
        // column makes the result deterministic.
        let sql = format!(
            "SELECT query.idx, {selected} \
             FROM read_parquet({}) t \
             JOIN query ON query.seq = ? AND t._pos = query.pos AND t._del = query.del \
             AND coalesce(t._ins, '') = query.ins \
             WHERE t._pos BETWEEN ? AND ? \
             ORDER BY query.idx, 2 DESC NULLS LAST",
            sql_string(&partition.join("*.parquet")),
        );
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![seq, start, end])?;
        while let Some(row) = rows.next()? {
            let index = row.get::<_, u32>(0)? as usize;
            if values.last().is_some_and(|(last, _)| *last == index) {
                continue;
            }
            let row: Vec<Value> = (1..=columns.len())
                .map(|i| row.get::<_, duckdb::types::Value>(i).map(json))
                .collect::<Result<_, _>>()?;
            values.push((index, row));
        }
    }
    Ok(values)
}

/// Convert a DuckDB value into JSON. Floats are converted through their shortest decimal
/// representation, so float32 columns give the same numbers as the API, e.g. 0.034 instead
/// of 0.03400000184774399.
fn json(value: duckdb::types::Value) -> Value {
    use duckdb::types::Value as DuckDb;
    match value {
        DuckDb::Null => Value::Null,
        DuckDb::Boolean(value) => value.into(),
        DuckDb::TinyInt(value) => value.into(),
        DuckDb::SmallInt(value) => value.into(),
        DuckDb::Int(value) => value.into(),
        DuckDb::BigInt(value) => value.into(),
        DuckDb::UTinyInt(value) => value.into(),
        DuckDb::USmallInt(value) => value.into(),
        DuckDb::UInt(value) => value.into(),
        DuckDb::UBigInt(value) => value.into(),
        DuckDb::HugeInt(value) => {
            i64::try_from(value).map_or_else(|_| number(&value.to_string()), Value::from)
        }
        DuckDb::UHugeInt(value) => {
            u64::try_from(value).map_or_else(|_| number(&value.to_string()), Value::from)
        }
        DuckDb::Float(value) => number(&value.to_string()),
        DuckDb::Decimal(value) => number(&value.to_string()),
        DuckDb::Double(value) => Number::from_f64(value).map_or(Value::Null, Value::Number),
        DuckDb::Text(value) | DuckDb::Enum(value) => value.into(),
        DuckDb::List(values) | DuckDb::Array(values) => values.into_iter().map(json).collect(),
        DuckDb::Struct(fields) => Value::Object(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), json(value.clone())))
                .collect(),
        ),
        DuckDb::Union(value) => json(*value),
        // Not used by Hub databases.
        other => format!("{other:?}").into(),
    }
}

fn number(decimal: &str) -> Value {
    decimal
        .parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .map_or(Value::Null, Value::Number)
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
pub(crate) mod tests {
    use super::*;
    use crate::hub::Column;
    use crate::models::Variant;
    use tempfile::TempDir;

    /// Write a VARIANT database into the store, partitioned like Hub databases. `rows` is a
    /// DuckDB query with the columns `_seq`, `_pos`, `_del`, `_ins` and `columns`.
    pub(crate) fn install(store: &Store, id: &str, genome: &str, columns: &[&str], rows: &str) {
        let id: DatabaseId = id.parse().unwrap();
        let version = id.version.clone().unwrap_or("0.0.1".into());
        let path = store.path(&id.owner, &id.name, &version);
        fs::create_dir_all(&path).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY ({rows}) TO {} (FORMAT PARQUET, PARTITION_BY (_seq))",
            sql_string(&path.join("parquet"))
        ))
        .unwrap();
        let database = Database {
            owner: id.owner.clone(),
            name: id.name.clone(),
            version,
            genome: Some(genome.into()),
            kind: Some("VARIANT".into()),
            columns: columns
                .iter()
                .map(|name| Column {
                    name: name.to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        fs::write(
            path.join("description.toml"),
            toml::to_string(&database).unwrap(),
        )
        .unwrap();
    }

    pub(crate) fn install_revel(store: &Store) {
        install(
            store,
            "@genebe/revel:0.0.1",
            "GRCh38",
            &["score"],
            "SELECT * FROM (VALUES
                ('22', 99, 1, 'T', 0.034::FLOAT),
                ('22', 99, 1, 'G', 0.5::FLOAT),
                ('22', 200, 0, 'TT', 0.25::FLOAT),
                ('22', 400, 1, NULL, 0.625::FLOAT),
                ('X', 99, 1, 'T', 0.9::FLOAT)
            ) AS t(_seq, _pos, _del, _ins, score)",
        );
    }

    fn keys(variants: &[Variant]) -> Vec<Option<Spdi>> {
        variants.iter().map(Variant::to_spdi).collect()
    }

    fn lookup_one(
        store: &Store,
        id: &str,
        columns: &[&str],
        variants: &[Variant],
    ) -> Vec<(usize, usize, Value)> {
        let installed = store.find(&id.parse().unwrap()).unwrap().unwrap();
        let columns = columns
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.to_string()))
            .collect();
        lookup(&[(installed, columns)], &keys(variants)).unwrap()
    }

    #[test]
    fn lookup_reads_columns_of_variants() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);
        let variants = [
            Variant::new("chrX", 100, "A", "T"),
            Variant::new("22", 100, "A", "C"),
            Variant::new("22", 100, "A", "T"),
            Variant::new("22", 200, "C", "CTT"),
            Variant::new("22", 400, "TA", "T"),
            Variant::new("21", 100, "A", "T"),
            Variant::new("22", 100, "AA", "A"),
        ];

        let found = lookup_one(&store, "@genebe/revel", &["score"], &variants);

        let mut scores: Vec<_> = found
            .iter()
            .map(|(v, _, value)| (*v, value.as_f64().unwrap()))
            .collect();
        scores.sort_by_key(|(v, _)| *v);
        assert_eq!(scores, [(0, 0.9), (2, 0.034), (3, 0.25), (4, 0.625)]);
    }

    #[test]
    fn lookup_keeps_column_types() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install(
            &store,
            "@genebe/clinvar",
            "GRCh38",
            &["significance", "stars", "diseases", "frequency"],
            "SELECT '1' AS _seq, 99 AS _pos, 1 AS _del, 'T' AS _ins, 'Pathogenic' AS significance,
                    2::SMALLINT AS stars, ['A', 'B'] AS diseases, 0.1::FLOAT AS frequency",
        );

        let found = lookup_one(
            &store,
            "@genebe/clinvar",
            &["significance", "stars", "diseases", "frequency"],
            &[Variant::new("1", 100, "A", "T")],
        );

        let values: Vec<_> = found.into_iter().map(|(_, _, value)| value).collect();
        assert_eq!(
            values,
            [
                Value::from("Pathogenic"),
                Value::from(2),
                serde_json::json!(["A", "B"]),
                Value::from(0.1)
            ]
        );
    }

    #[test]
    fn lookup_takes_one_row_of_duplicate_keys() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install(
            &store,
            "@genebe/spliceai",
            "GRCh38",
            &["max", "gene"],
            "SELECT * FROM (VALUES ('22', 99, 1, 'T', 0.25, 'A'), ('22', 99, 1, 'T', 0.5, 'B'))
                AS t(_seq, _pos, _del, _ins, max, gene)",
        );

        let found = lookup_one(
            &store,
            "@genebe/spliceai",
            &["max", "gene"],
            &[Variant::new("22", 100, "A", "T")],
        );

        let values: Vec<_> = found.into_iter().map(|(_, _, value)| value).collect();
        assert_eq!(values, [Value::from(0.5), Value::from("B")]);
    }

    #[test]
    fn lookup_handles_paths_with_special_characters() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path().join("Application Support").join("Felix's"));
        install_revel(&store);

        let found = lookup_one(
            &store,
            "@genebe/revel",
            &["score"],
            &[Variant::new("22", 100, "A", "T")],
        );

        assert_eq!(found, [(0, 0, Value::from(0.034))]);
    }

    #[test]
    fn find_uses_newest_or_given_version() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        for version in ["0.0.2", "0.0.10", "0.0.10-1", "unversioned"] {
            install(
                &store,
                &format!("@genebe/revel:{version}"),
                "GRCh38",
                &["score"],
                "SELECT '1' AS _seq, 1 AS _pos, 1 AS _del, 'A' AS _ins, 0.5 AS score",
            );
        }

        let version = |id: &str| {
            store
                .find(&id.parse().unwrap())
                .unwrap()
                .unwrap()
                .database
                .version
        };

        assert_eq!(version("@genebe/revel"), "0.0.10");
        assert_eq!(version("@genebe/revel:0.0.2"), "0.0.2");
        assert!(store
            .find(&"@genebe/revel:0.0.3".parse().unwrap())
            .unwrap()
            .is_none());
        assert!(store
            .find(&"@genebe/spliceai".parse().unwrap())
            .unwrap()
            .is_none());
    }

    #[test]
    fn find_reports_broken_versions() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);
        fs::create_dir_all(store.path("@genebe", "revel", "0.0.2")).unwrap();

        assert!(store.find(&"@genebe/revel".parse().unwrap()).is_err());
    }

    #[test]
    fn api_field_finds_hg38_databases() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);

        let (installed, column) = store
            .api_field("revel_score", Genome::Hg38)
            .unwrap()
            .unwrap();
        assert_eq!(installed.database.name, "revel");
        assert_eq!(column, "score");
        assert!(store
            .api_field("revel_score", Genome::Hg19)
            .unwrap()
            .is_none());
        assert!(store
            .api_field("spliceai_max_score", Genome::Hg38)
            .unwrap()
            .is_none());
        assert!(store
            .api_field("acmg_score", Genome::Hg38)
            .unwrap()
            .is_none());
    }

    #[test]
    fn check_rejects_unknown_columns_and_other_genomes() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install(
            &store,
            "@genebe/revel_hg19",
            "GRCh37",
            &["score"],
            "SELECT '1' AS _seq, 1 AS _pos, 1 AS _del, 'A' AS _ins, 0.5 AS score",
        );
        let installed = store
            .find(&"@genebe/revel_hg19".parse().unwrap())
            .unwrap()
            .unwrap();

        assert!(check_column(&installed, "score", Genome::Hg19).is_ok());
        assert!(matches!(
            check_column(&installed, "phred", Genome::Hg19),
            Err(GeneBearError::UnknownField { .. })
        ));
        assert!(check_column(&installed, "score", Genome::Hg38).is_err());
    }

    #[test]
    fn check_rejects_other_database_types() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);
        let mut installed = store
            .find(&"@genebe/revel".parse().unwrap())
            .unwrap()
            .unwrap();
        installed.database.kind = Some("POSITION".into());

        assert!(check_column(&installed, "score", Genome::Hg38).is_err());
    }

    #[test]
    fn installed_ignores_unfinished_downloads() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        install_revel(&store);
        let temp = dir.path().join("__temp/genebears/@genebe");
        fs::create_dir_all(temp.join("spliceai/0.0.1/parquet")).unwrap();
        fs::write(temp.join("spliceai.lock"), "").unwrap();
        fs::create_dir_all(dir.path().join("@genebe/.hidden/0.0.1")).unwrap();
        fs::create_dir_all(dir.path().join("@genebe/empty/0.0.1")).unwrap();

        let installed = store.installed().unwrap();

        assert_eq!(installed.len(), 1);
        assert_eq!(
            installed[0].database.id().to_string(),
            "@genebe/revel:0.0.1"
        );
        assert_eq!(installed[0].path, dir.path().join("@genebe/revel/0.0.1"));
    }

    /// Checks that the databases and columns the API takes fields from still exist on the
    /// Hub.
    #[tokio::test]
    #[ignore = "needs the GeneBe Hub"]
    async fn api_fields_exist_on_hub() {
        let hub = crate::GeneBears::new(crate::ClientConfig::default())
            .unwrap()
            .hub();
        for (_, database, column) in crate::models::HUB_SOURCES {
            let database = hub.database(&database.parse().unwrap()).await.unwrap();
            assert!(
                database.columns.iter().any(|c| c.name == column),
                "{} has no column {column}",
                database.id()
            );
        }
    }
}
