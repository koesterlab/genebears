//! Local store of GeneBe Hub databases, laid out like the store of the GeneBe client
//! (`{owner}/{name}/{version}`) so that both can use the same downloads.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use duckdb::{params, Connection};
use serde::de::DeserializeOwned;
use serde_json::{Map, Number, Value};

use crate::error::GeneBearError;
use crate::hub::{Database, DatabaseId};
use crate::models::{Genome, Variant};

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

    /// The installed database in the version of `id`, or the newest installed version if
    /// `id` has none. Versions that are not semantic versions count as the oldest.
    pub fn get(&self, id: &DatabaseId) -> Result<Installed, GeneBearError> {
        let not_installed = || GeneBearError::NotInstalled {
            database: id.clone(),
        };
        let version = match &id.version {
            Some(version) => version.clone(),
            None => directories(&self.root.join(&id.owner).join(&id.name))?
                .into_iter()
                .max_by_key(|version| (semver::Version::parse(version).ok(), version.clone()))
                .ok_or_else(not_installed)?,
        };
        let path = self.path(&id.owner, &id.name, &version);
        let description = path.join("description.toml");
        if !description.exists() {
            return Err(not_installed());
        }
        let database = toml::from_str(&fs::read_to_string(&description)?).map_err(|e| {
            GeneBearError::Other(format!("Cannot read {}: {e}", description.display()))
        })?;
        Ok(Installed { path, database })
    }
}

/// A database installed in a [`Store`].
#[derive(Debug, Clone)]
pub struct Installed {
    pub path: PathBuf,
    pub database: Database,
}

impl Installed {
    /// Look up the variants, giving the rows of each with the given columns. Usually a variant
    /// has at most one row, but some databases have more, e.g. one per gene. Variants that
    /// cannot be written like the keys of the Hub, e.g. indels that are not left-aligned, have
    /// none.
    ///
    /// Each row is deserialized from a JSON object with the columns as keys. Reading the
    /// parquet files blocks, so use e.g. `tokio::task::spawn_blocking` in async code.
    pub fn lookup<T: DeserializeOwned>(
        &self,
        variants: &[Variant],
        genome: Genome,
        columns: &[&str],
    ) -> Result<Vec<Vec<T>>, GeneBearError> {
        self.check(genome, columns)?;
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TEMP TABLE query (idx UINTEGER, seq VARCHAR, pos BIGINT, del BIGINT, ins VARCHAR)",
        )?;
        // Querying one chromosome at a time with the range of positions lets DuckDB skip the
        // parts of the files that cannot match.
        let mut ranges: BTreeMap<String, (u32, u32)> = BTreeMap::new();
        let mut appender = conn.appender("query")?;
        for (index, key) in variants.iter().map(Variant::to_spdi).enumerate() {
            if let Some(key) = key {
                appender.append_row(params![
                    index as u32,
                    key.seq,
                    key.pos as i64,
                    key.del as i64,
                    key.ins
                ])?;
                let range = ranges.entry(key.seq).or_insert((key.pos, key.pos));
                *range = (range.0.min(key.pos), range.1.max(key.pos));
            }
        }
        appender.flush()?;
        drop(appender);

        let selected = columns
            .iter()
            .map(|column| format!("t.\"{}\"", column.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(", ");
        let mut rows: Vec<Vec<T>> = variants.iter().map(|_| Vec::new()).collect();
        for (seq, (start, end)) in ranges {
            let partition = self.path.join("parquet").join(format!("_seq={seq}"));
            // Partitions without files would make DuckDB fail.
            if !has_parquet_files(&partition) {
                continue;
            }
            let sql = format!(
                "SELECT query.idx, {selected} \
                 FROM read_parquet({}) t \
                 JOIN query ON query.seq = ? AND t._pos = query.pos AND t._del = query.del \
                 AND coalesce(t._ins, '') = query.ins \
                 WHERE t._pos BETWEEN ? AND ?",
                sql_string(&partition.join("*.parquet")),
            );
            let mut statement = conn.prepare(&sql)?;
            let mut result = statement.query(params![seq, start, end])?;
            while let Some(row) = result.next()? {
                let mut values = Map::new();
                for (i, column) in columns.iter().enumerate() {
                    values.insert(column.to_string(), json(row.get(i + 1)?));
                }
                rows[row.get::<_, u32>(0)? as usize]
                    .push(serde_json::from_value(Value::Object(values))?);
            }
        }
        Ok(rows)
    }

    /// Check that the database has variant keys, the columns and a matching genome.
    fn check(&self, genome: Genome, columns: &[&str]) -> Result<(), GeneBearError> {
        let database = &self.database;
        if let Some(kind) = database.kind.as_deref().filter(|kind| *kind != "VARIANT") {
            return Err(GeneBearError::Other(format!(
                "{} is a {kind} database, only VARIANT databases are supported",
                database.id()
            )));
        }
        if let Some(column) = columns
            .iter()
            .find(|column| !database.columns.iter().any(|c| c.name == **column))
        {
            return Err(GeneBearError::UnknownColumn {
                database: database.id(),
                column: column.to_string(),
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
/// GeneBe client.
fn directories(directory: &Path) -> Result<Vec<String>, GeneBearError> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().is_dir() && !name.starts_with('.') && name != "__temp" {
            names.push(name);
        }
    }
    Ok(names)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::hub::Column;
    use serde::Deserialize;
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

    fn revel(store: &Store) -> Installed {
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
        store.get(&"@genebe/revel".parse().unwrap()).unwrap()
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Score {
        score: f64,
    }

    #[test]
    fn lookup_finds_variants_by_their_normalized_keys() {
        let dir = TempDir::new().unwrap();
        let variants = [
            Variant::new("chrX", 100, "A", "T"),
            Variant::new("22", 100, "A", "C"),
            Variant::new("22", 100, "A", "T"),
            Variant::new("22", 200, "C", "CTT"),
            Variant::new("22", 400, "TA", "T"),
            Variant::new("21", 100, "A", "T"),
            // Not left-aligned.
            Variant::new("22", 100, "AA", "A"),
        ];

        let rows: Vec<Vec<Score>> = revel(&Store::new(dir.path()))
            .lookup(&variants, Genome::Hg38, &["score"])
            .unwrap();

        let scores: Vec<Option<f64>> = rows.iter().map(|r| r.first().map(|s| s.score)).collect();
        assert_eq!(
            scores,
            [
                Some(0.9),
                None,
                Some(0.034),
                Some(0.25),
                Some(0.625),
                None,
                None
            ]
        );
    }

    #[test]
    fn lookup_keeps_column_types_and_every_row() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path().join("Application Support").join("Felix's"));
        install(
            &store,
            "@genebe/spliceai",
            "GRCh38",
            &["max", "gene", "positions"],
            "SELECT * FROM (VALUES ('22', 99, 1, 'T', 0.25::FLOAT, 'A', [1, 2]),
                ('22', 99, 1, 'T', 0.5::FLOAT, 'B', [3]))
                AS t(_seq, _pos, _del, _ins, max, gene, positions)",
        );

        let rows: Vec<Vec<Value>> = store
            .get(&"@genebe/spliceai".parse().unwrap())
            .unwrap()
            .lookup(
                &[Variant::new("22", 100, "A", "T")],
                Genome::Hg38,
                &["max", "gene", "positions"],
            )
            .unwrap();

        let mut genes = rows[0].clone();
        genes.sort_by_key(|row| row["gene"].to_string());
        assert_eq!(
            genes,
            [
                serde_json::json!({"max": 0.25, "gene": "A", "positions": [1, 2]}),
                serde_json::json!({"max": 0.5, "gene": "B", "positions": [3]}),
            ]
        );
    }

    #[test]
    fn lookup_checks_columns_and_genome() {
        let dir = TempDir::new().unwrap();
        let revel = revel(&Store::new(dir.path()));
        let variants = [Variant::new("22", 100, "A", "T")];

        assert!(matches!(
            revel.lookup::<Value>(&variants, Genome::Hg38, &["max"]),
            Err(GeneBearError::UnknownColumn { .. })
        ));
        assert!(matches!(
            revel.lookup::<Value>(&variants, Genome::Hg19, &["score"]),
            Err(GeneBearError::GenomeMismatch { .. })
        ));
    }

    #[test]
    fn get_uses_newest_or_given_version() {
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
        let version = |id: &str| store.get(&id.parse().unwrap()).map(|i| i.database.version);

        assert_eq!(version("@genebe/revel").unwrap(), "0.0.10");
        assert_eq!(version("@genebe/revel:0.0.2").unwrap(), "0.0.2");
        for missing in ["@genebe/revel:0.0.3", "@genebe/spliceai"] {
            assert!(matches!(
                version(missing),
                Err(GeneBearError::NotInstalled { .. })
            ));
        }
    }
}
