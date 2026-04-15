//! DuckDB based persistent cache for variant annotations.
//!
//! Each variant is keyed by `chr:pos:ref:alt:genome`.  The full
//! [`AnnotatedVariant`] is stored as a JSON string.

use std::collections::HashMap;
use std::path::Path;

use duckdb::{params, Connection};

use crate::error::GeneBearError;
use crate::models::AnnotatedVariant;

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

    /// Look up a single key.  Returns `None` on a cache miss.
    pub fn get(&self, key: &str) -> Result<Option<AnnotatedVariant>, GeneBearError> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM variant_cache WHERE cache_key = ?")?;

        let mut rows = stmt.query(params![key])?;
        if let Some(row) = rows.next()? {
            let json: String = row.get(0)?;
            let v: AnnotatedVariant = serde_json::from_str(&json)?;
            return Ok(Some(v));
        }
        Ok(None)
    }

    /// Bulk cache lookup.  Returns a map of `cache_key → AnnotatedVariant`
    /// for every key that was found.
    pub fn get_batch(
        &self,
        keys: &[&str],
    ) -> Result<HashMap<String, AnnotatedVariant>, GeneBearError> {
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
            if let Ok(v) = serde_json::from_str::<AnnotatedVariant>(&json) {
                map.insert(key, v);
            }
        }
        Ok(map)
    }

    /// Store a single annotation.  Overwrites any existing entry for the same key.
    pub fn store(&self, key: &str, variant: &AnnotatedVariant) -> Result<(), GeneBearError> {
        let json = serde_json::to_string(variant)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO variant_cache (cache_key, payload) VALUES (?, ?)",
            params![key, json],
        )?;
        Ok(())
    }

    /// Atomically store multiple annotations in a single transaction.
    pub fn store_batch(&self, entries: &[(&str, &AnnotatedVariant)]) -> Result<(), GeneBearError> {
        if entries.is_empty() {
            return Ok(());
        }

        let tx = self.conn.unchecked_transaction()?;

        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO variant_cache (cache_key, payload) VALUES (?, ?)",
            )?;

            for (key, variant) in entries {
                let json = serde_json::to_string(variant)?;
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
    use crate::models::AnnotatedVariant;
    use tempfile::tempdir;

    fn temp_cache() -> (Cache, tempfile::TempDir) {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("cache.duckdb");
        let c = Cache::open(&db_path).expect("cache open");
        (c, dir)
    }

    fn make_variant(gene: &str, score: f64) -> AnnotatedVariant {
        AnnotatedVariant {
            chr: Some("22".into()),
            pos: Some(1_000),
            ref_allele: Some("A".into()),
            alt: Some("T".into()),
            gene_symbol: Some(gene.into()),
            revel_score: Some(score),
            effect: None,
            transcript: None,
            gene_hgnc_id: None,
            dbsnp: None,
            frequency_reference_population: None,
            hom_count_reference_population: None,
            allele_count_reference_population: None,
            gnomad_exomes_af: None,
            gnomad_genomes_af: None,
            gnomad_exomes_ac: None,
            gnomad_genomes_ac: None,
            gnomad_exomes_homalt: None,
            gnomad_genomes_homalt: None,
            gnomad_mito_homoplasmic: None,
            gnomad_mito_heteroplasmic: None,
            computational_score_selected: None,
            computational_prediction_selected: None,
            computational_source_selected: None,
            revel_prediction: None,
            alphamissense_score: None,
            alphamissense_prediction: None,
            bayesdelnoaf_score: None,
            bayesdelnoaf_prediction: None,
            phylop100way_score: None,
            phylop100way_prediction: None,
            splice_score_selected: None,
            splice_prediction_selected: None,
            splice_source_selected: None,
            spliceai_max_score: None,
            spliceai_max_prediction: None,
            dbscsnv_ada_score: None,
            dbscsnv_ada_prediction: None,
            apogee2_score: None,
            apogee2_prediction: None,
            mitotip_score: None,
            mitotip_prediction: None,
            acmg_score: None,
            acmg_classification: None,
            acmg_criteria: None,
            acmg_by_gene: None,
            clinvar_disease: None,
            clinvar_classification: None,
            clinvar_review_status: None,
            clinvar_submissions_summary: None,
            phenotype_combined: None,
            pathogenicity_classification_combined: None,
            consequences: None,
        }
    }

    #[test]
    fn empty_cache_count_is_zero() {
        let (cache, _f) = temp_cache();
        assert_eq!(cache.count().unwrap(), 0);
    }

    #[test]
    fn clear_empties_cache() {
        let (cache, _f) = temp_cache();
        let v = make_variant("BRCA1", 0.9);
        cache.store("key1", &v).unwrap();
        assert_eq!(cache.count().unwrap(), 1);
        cache.clear().unwrap();
        assert_eq!(cache.count().unwrap(), 0);
    }

    #[test]
    fn store_and_get_round_trip() {
        let (cache, _f) = temp_cache();
        let v = make_variant("BRCA2", 0.75);
        cache.store("k1", &v).unwrap();

        let hit = cache.get("k1").unwrap().expect("expected cache hit");
        assert_eq!(hit.gene_symbol.as_deref(), Some("BRCA2"));
        assert!((hit.revel_score.unwrap() - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn get_miss_returns_none() {
        let (cache, _f) = temp_cache();
        assert!(cache.get("nonexistent_key").unwrap().is_none());
    }

    #[test]
    fn store_overwrites_existing_key() {
        let (cache, _f) = temp_cache();
        let v1 = make_variant("GENE_A", 0.1);
        let v2 = make_variant("GENE_B", 0.9);

        cache.store("key", &v1).unwrap();
        cache.store("key", &v2).unwrap(); // same key → overwrite

        let hit = cache.get("key").unwrap().unwrap();
        assert_eq!(hit.gene_symbol.as_deref(), Some("GENE_B"));
        assert_eq!(cache.count().unwrap(), 1); // still only one row
    }

    #[test]
    fn store_batch_and_get_batch() {
        let (cache, _f) = temp_cache();

        let v1 = make_variant("GENE1", 0.1);
        let v2 = make_variant("GENE2", 0.2);
        let v3 = make_variant("GENE3", 0.3);

        cache
            .store_batch(&[("k1", &v1), ("k2", &v2), ("k3", &v3)])
            .unwrap();

        assert_eq!(cache.count().unwrap(), 3);

        let map = cache.get_batch(&["k1", "k2", "k3"]).unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(map["k1"].gene_symbol.as_deref(), Some("GENE1"));
        assert_eq!(map["k2"].gene_symbol.as_deref(), Some("GENE2"));
        assert_eq!(map["k3"].gene_symbol.as_deref(), Some("GENE3"));
    }

    #[test]
    fn get_batch_partial_hit() {
        let (cache, _f) = temp_cache();
        let v = make_variant("PRESENT", 0.5);
        cache.store("present_key", &v).unwrap();

        let map = cache.get_batch(&["present_key", "missing_key"]).unwrap();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("present_key"));
        assert!(!map.contains_key("missing_key"));
    }

    #[test]
    fn get_batch_empty_input_returns_empty_map() {
        let (cache, _f) = temp_cache();
        let map = cache.get_batch(&[]).unwrap();
        assert!(map.is_empty());
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
        let old = make_variant("OLD", 0.1);
        let new = make_variant("NEW", 0.9);

        cache.store("k", &old).unwrap();
        cache.store_batch(&[("k", &new)]).unwrap();

        let hit = cache.get("k").unwrap().unwrap();
        assert_eq!(hit.gene_symbol.as_deref(), Some("NEW"));
        assert_eq!(cache.count().unwrap(), 1);
    }
}
