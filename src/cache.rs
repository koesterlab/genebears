//! DuckDB based persistent cache for variant annotations.
//!
//! Each variant is keyed by `chr:pos:ref:alt:genome` and the options that were set. The
//! [`Record`] returned by the API is stored as a JSON string.

use std::collections::HashMap;
use std::path::Path;

use duckdb::{params, Connection};

use crate::error::GeneBearError;
use crate::models::Record;

/// DuckDB cache.
pub struct Cache {
    conn: Connection,
}

impl Cache {
    /// Open (or create) a DuckDB database at `path` and ensure the cache
    /// table exists.
    pub fn open(path: &Path) -> Result<Self, GeneBearError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS variant_cache (
                cache_key   VARCHAR PRIMARY KEY,
                payload     VARCHAR  NOT NULL,
                inserted_at TIMESTAMP DEFAULT current_timestamp
            );",
        )?;
        Ok(Cache { conn })
    }

    /// Look up several keys. Returns the records of the keys that were found.
    pub fn get_batch(&self, keys: &[&str]) -> Result<HashMap<String, Record>, GeneBearError> {
        if keys.is_empty() {
            return Ok(HashMap::new());
        }

        let placeholders = keys.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT cache_key, payload \
             FROM variant_cache \
             WHERE cache_key IN ({placeholders})"
        );

        let mut stmt = self.conn.prepare(&sql)?;

        let params_vec: Vec<&dyn duckdb::ToSql> =
            keys.iter().map(|k| k as &dyn duckdb::ToSql).collect();

        let mut rows = stmt.query(params_vec.as_slice())?;
        let mut map = HashMap::new();

        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let json: String = row.get(1)?;
            if let Ok(v) = serde_json::from_str::<Record>(&json) {
                map.insert(key, v);
            }
        }
        Ok(map)
    }

    /// Atomically store multiple annotations in a single transaction.
    pub fn store_batch(&self, entries: &[(&str, &Record)]) -> Result<(), GeneBearError> {
        if entries.is_empty() {
            return Ok(());
        }

        let tx = self.conn.unchecked_transaction()?;

        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO variant_cache (cache_key, payload) VALUES (?, ?)",
            )?;

            for (key, record) in entries {
                let json = serde_json::to_string(record)?;
                stmt.execute(params![key, json])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Remove all cached entries.
    pub fn clear(&self) -> Result<(), GeneBearError> {
        self.conn.execute_batch("DELETE FROM variant_cache;")?;
        Ok(())
    }

    /// Return the number of variants currently in the cache.
    pub fn count(&self) -> Result<u64, GeneBearError> {
        let n: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM variant_cache", [], |row| row.get(0))?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn temp_cache() -> (Cache, tempfile::TempDir) {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("cache.duckdb");
        let c = Cache::open(&db_path).expect("cache open");
        (c, dir)
    }

    fn get(cache: &Cache, key: &str) -> Option<Record> {
        cache.get_batch(&[key]).unwrap().remove(key)
    }

    fn store(cache: &Cache, key: &str, record: &Record) {
        cache.store_batch(&[(key, record)]).unwrap();
    }

    fn make_record(gene: &str, score: f64) -> Record {
        let json = serde_json::json!({
            "chr": "22",
            "pos": 1_000,
            "ref": "A",
            "alt": "T",
            "gene_symbol": gene,
            "revel_score": score,
            "acmg_score": null,
        });
        json.as_object().unwrap().clone()
    }

    #[test]
    fn empty_cache_count_is_zero() {
        let (cache, _f) = temp_cache();
        assert_eq!(cache.count().unwrap(), 0);
    }

    #[test]
    fn clear_empties_cache() {
        let (cache, _f) = temp_cache();
        store(&cache, "key1", &make_record("BRCA1", 0.9));
        assert_eq!(cache.count().unwrap(), 1);
        cache.clear().unwrap();
        assert_eq!(cache.count().unwrap(), 0);
    }

    #[test]
    fn store_and_get_round_trip() {
        let (cache, _f) = temp_cache();
        let record = make_record("BRCA2", 0.75);
        store(&cache, "k1", &record);

        let hit = get(&cache, "k1").expect("expected cache hit");
        assert_eq!(hit, record);
    }

    #[test]
    fn get_miss_returns_none() {
        let (cache, _f) = temp_cache();
        assert!(get(&cache, "nonexistent_key").is_none());
    }

    #[test]
    fn store_overwrites_existing_key() {
        let (cache, _f) = temp_cache();
        store(&cache, "key", &make_record("GENE_A", 0.1));
        store(&cache, "key", &make_record("GENE_B", 0.9));

        let hit = get(&cache, "key").unwrap();
        assert_eq!(hit["gene_symbol"], "GENE_B");
        assert_eq!(cache.count().unwrap(), 1);
    }

    #[test]
    fn store_batch_and_get_batch() {
        let (cache, _f) = temp_cache();
        let (r1, r2, r3) = (
            make_record("GENE1", 0.1),
            make_record("GENE2", 0.2),
            make_record("GENE3", 0.3),
        );

        cache
            .store_batch(&[("k1", &r1), ("k2", &r2), ("k3", &r3)])
            .unwrap();

        assert_eq!(cache.count().unwrap(), 3);
        let map = cache.get_batch(&["k1", "k2", "k3"]).unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(map["k1"], r1);
        assert_eq!(map["k3"], r3);
    }

    #[test]
    fn get_batch_partial_hit() {
        let (cache, _f) = temp_cache();
        store(&cache, "present_key", &make_record("PRESENT", 0.5));

        let map = cache.get_batch(&["present_key", "missing_key"]).unwrap();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("present_key"));
    }

    #[test]
    fn get_batch_empty_input_returns_empty_map() {
        let (cache, _f) = temp_cache();
        assert!(cache.get_batch(&[]).unwrap().is_empty());
    }

    #[test]
    fn store_batch_empty_input_is_noop() {
        let (cache, _f) = temp_cache();
        cache.store_batch(&[]).unwrap();
        assert_eq!(cache.count().unwrap(), 0);
    }

    #[test]
    fn store_batch_overwrites_existing_keys() {
        let (cache, _f) = temp_cache();
        store(&cache, "k", &make_record("OLD", 0.1));
        cache
            .store_batch(&[("k", &make_record("NEW", 0.9))])
            .unwrap();

        let hit = get(&cache, "k").unwrap();
        assert_eq!(hit["gene_symbol"], "NEW");
        assert_eq!(cache.count().unwrap(), 1);
    }

    #[test]
    fn reads_records_cached_by_earlier_versions() {
        let (cache, _f) = temp_cache();
        let old = crate::models::AnnotatedVariant {
            revel_score: Some(0.5),
            ..Default::default()
        };
        cache
            .conn
            .execute(
                "INSERT INTO variant_cache (cache_key, payload) VALUES (?, ?)",
                params!["k", serde_json::to_string(&old).unwrap()],
            )
            .unwrap();

        let hit = get(&cache, "k").unwrap();
        assert_eq!(hit["revel_score"], 0.5);
    }
}
