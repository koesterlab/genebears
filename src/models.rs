use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Reference genome assembly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Genome {
    #[default]
    Hg38,
    Hg19,
    T2t,
}

impl Genome {
    /// String form accepted by the GeneBe API (`genome` query parameter).
    pub fn as_str(self) -> &'static str {
        match self {
            Genome::Hg38 => "hg38",
            Genome::Hg19 => "hg19",
            Genome::T2t => "t2t",
        }
    }
}

/// A single variant to be annotated (VCF-style coordinates).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Variant {
    /// Chromosome (e.g. `"6"`, `"X"`, `"M"`).
    #[serde(rename = "chr")]
    pub chr: String,
    /// 1-based genomic position (VCF convention).
    #[serde(rename = "pos")]
    pub pos: u64,
    /// Reference allele.
    #[serde(rename = "ref")]
    pub ref_allele: String,
    /// Alternate allele.
    #[serde(rename = "alt")]
    pub alt_allele: String,
}

impl Variant {
    pub fn new(
        chr: impl Into<String>,
        pos: u64,
        ref_allele: impl Into<String>,
        alt_allele: impl Into<String>,
    ) -> Self {
        Variant {
            chr: chr.into(),
            pos,
            ref_allele: ref_allele.into(),
            alt_allele: alt_allele.into(),
        }
    }

    /// Key of the API record of this variant in the cache. Options are part of the key since
    /// they change the record.
    pub(crate) fn cache_key(&self, genome: Genome, opts: AnnotateOptions) -> String {
        let mut key = format!(
            "{}:{}:{}:{}:{}",
            self.chr,
            self.pos,
            self.ref_allele,
            self.alt_allele,
            genome.as_str()
        );
        for (name, _) in opts.params() {
            key.push(':');
            key.push_str(name);
        }
        key
    }
}

/// Controls which sections of the annotation the API should compute.
///
/// All fields default to `false` / `None`, which means the API returns
/// everything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnnotateOptions {
    /// Use only RefSeq transcripts.
    pub use_refseq: Option<bool>,
    /// Use only Ensembl transcripts.
    pub use_ensembl: Option<bool>,
    /// Skip ACMG scoring (faster, smaller response).
    pub omit_acmg: bool,
    /// Skip per-transcript consequence annotation.
    pub omit_csq: bool,
    /// Skip basic annotations (GnomAD frequencies etc.).
    pub omit_basic: bool,
    /// Skip advanced annotations (ClinVar etc.).
    pub omit_advanced: bool,
    /// Annotate for *all* genes in the region, not just the primary one.
    pub all_genes: bool,
}

impl AnnotateOptions {
    /// Query parameters of the options that are set.
    pub(crate) fn params(&self) -> Vec<(&'static str, &'static str)> {
        [
            ("useRefseq", self.use_refseq == Some(true)),
            ("useEnsembl", self.use_ensembl == Some(true)),
            ("omitAcmg", self.omit_acmg),
            ("omitCsq", self.omit_csq),
            ("omitBasic", self.omit_basic),
            ("omitAdvanced", self.omit_advanced),
            ("allGenes", self.all_genes),
        ]
        .into_iter()
        .filter(|(_, set)| *set)
        .map(|(name, _)| (name, "true"))
        .collect()
    }
}

/// Annotation of a variant as returned by the GeneBe API, with every API field as key.
pub(crate) type Record = serde_json::Map<String, Value>;

/// Top-level response envelope returned by the GeneBe API.
#[derive(Debug, Deserialize)]
pub(crate) struct ApiResponse {
    pub variants: Vec<Record>,
}

/// Full annotation for one variant, as returned by GeneBe.
///
/// Every field is `Option<T>` — the API may omit fields depending on variant
/// type, `omit_*` flags, or absence of data.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AnnotatedVariant {
    pub chr: Option<String>,
    pub pos: Option<u64>,
    #[serde(rename = "ref")]
    pub ref_allele: Option<String>,
    pub alt: Option<String>,
    pub warning: Option<String>,
    pub effect: Option<String>,
    pub transcript: Option<String>,
    pub gene_symbol: Option<String>,
    pub gene_hgnc_id: Option<u32>,
    pub dbsnp: Option<String>,
    pub frequency_reference_population: Option<f64>,
    pub hom_count_reference_population: Option<u64>,
    pub allele_count_reference_population: Option<u64>,
    pub gnomad_exomes_af: Option<f64>,
    pub gnomad_genomes_af: Option<f64>,
    pub gnomad_exomes_ac: Option<u64>,
    pub gnomad_genomes_ac: Option<u64>,
    pub gnomad_exomes_homalt: Option<u64>,
    pub gnomad_genomes_homalt: Option<u64>,
    pub gnomad_mito_homoplasmic: Option<u64>,
    pub gnomad_mito_heteroplasmic: Option<u64>,
    pub computational_score_selected: Option<f64>,
    pub computational_prediction_selected: Option<String>,
    pub computational_source_selected: Option<String>,
    pub revel_score: Option<f64>,
    pub revel_prediction: Option<String>,
    pub alphamissense_score: Option<f64>,
    pub alphamissense_prediction: Option<String>,
    pub bayesdelnoaf_score: Option<f64>,
    pub bayesdelnoaf_prediction: Option<String>,
    pub phylop100way_score: Option<f64>,
    pub phylop100way_prediction: Option<String>,
    pub splice_score_selected: Option<f64>,
    pub splice_prediction_selected: Option<String>,
    pub splice_source_selected: Option<String>,
    pub spliceai_max_score: Option<f64>,
    pub spliceai_max_prediction: Option<String>,
    pub dbscsnv_ada_score: Option<f64>,
    pub dbscsnv_ada_prediction: Option<String>,
    pub apogee2_score: Option<f64>,
    pub apogee2_prediction: Option<String>,
    pub mitotip_score: Option<f64>,
    pub mitotip_prediction: Option<String>,
    pub acmg_score: Option<f64>,
    pub acmg_classification: Option<String>,
    pub acmg_criteria: Option<String>,
    pub acmg_by_gene: Option<Vec<AcmgByGene>>,
    pub clinvar_disease: Option<String>,
    pub clinvar_classification: Option<String>,
    pub clinvar_review_status: Option<String>,
    pub clinvar_submissions_summary: Option<serde_json::Value>,
    pub phenotype_combined: Option<String>,
    pub pathogenicity_classification_combined: Option<String>,
    pub consequences: Option<Vec<Consequence>>,
}

/// Per-gene ACMG evidence breakdown.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AcmgByGene {
    pub score: Option<f64>,
    pub benign_score: Option<f64>,
    pub pathogenic_score: Option<f64>,
    pub criteria: Option<Vec<String>>,
    pub verdict: Option<String>,
    pub transcript: Option<String>,
    pub gene_symbol: Option<String>,
    pub hgnc_id: Option<u32>,
    pub effects: Option<Vec<String>>,
    pub inheritance_mode: Option<String>,
    pub hgvs_c: Option<String>,
    pub hgvs_p: Option<String>,
}

/// Per-transcript consequence annotation.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Consequence {
    pub aa_ref: Option<String>,
    pub aa_alt: Option<String>,
    pub canonical: Option<bool>,
    pub protein_coding: Option<bool>,
    pub strand: Option<bool>,
    pub consequences: Option<Vec<String>>,
    pub exon_rank: Option<u32>,
    pub exon_count: Option<u32>,
    pub gene_symbol: Option<String>,
    pub gene_hgnc_id: Option<u32>,
    pub hgvs_c: Option<String>,
    pub hgvs_p: Option<String>,
    pub transcript: Option<String>,
    pub protein_id: Option<String>,
    pub transcript_support_level: Option<u32>,
    pub aa_start: Option<u32>,
    pub aa_length: Option<u32>,
    pub cds_start: Option<u32>,
    pub cdna_start: Option<u32>,
    pub mane_select: Option<String>,
    pub mane_plus: Option<String>,
    pub biotype: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genome_as_str() {
        assert_eq!(Genome::Hg38.as_str(), "hg38");
        assert_eq!(Genome::Hg19.as_str(), "hg19");
        assert_eq!(Genome::T2t.as_str(), "t2t");
    }

    #[test]
    fn genome_default_is_hg38() {
        assert_eq!(Genome::default(), Genome::Hg38);
    }

    #[test]
    fn genome_serde_round_trip() {
        for g in [Genome::Hg38, Genome::Hg19, Genome::T2t] {
            let json = serde_json::to_string(&g).unwrap();
            let decoded: Genome = serde_json::from_str(&json).unwrap();
            assert_eq!(g, decoded);
        }
    }

    #[test]
    fn variant_new_stores_fields() {
        let v = Variant::new("22", 28_695_868, "AG", "A");
        assert_eq!(v.chr, "22");
        assert_eq!(v.pos, 28_695_868);
        assert_eq!(v.ref_allele, "AG");
        assert_eq!(v.alt_allele, "A");
    }

    #[test]
    fn variant_cache_key_format() {
        let v = Variant::new("22", 28_695_868, "AG", "A");
        let key = v.cache_key(Genome::Hg38, AnnotateOptions::default());
        assert_eq!(key, "22:28695868:AG:A:hg38");
    }

    #[test]
    fn variant_cache_key_differs_by_genome() {
        let v = Variant::new("1", 100, "C", "T");
        let k38 = v.cache_key(Genome::Hg38, AnnotateOptions::default());
        let k19 = v.cache_key(Genome::Hg19, AnnotateOptions::default());
        let kt2t = v.cache_key(Genome::T2t, AnnotateOptions::default());
        assert_ne!(k38, k19);
        assert_ne!(k38, kt2t);
        assert_ne!(k19, kt2t);
    }

    #[test]
    fn variant_cache_key_contains_options() {
        let v = Variant::new("22", 28_695_868, "AG", "A");
        let opts = AnnotateOptions {
            omit_acmg: true,
            use_refseq: Some(true),
            ..Default::default()
        };
        assert_eq!(
            v.cache_key(Genome::Hg38, opts),
            "22:28695868:AG:A:hg38:useRefseq:omitAcmg"
        );
    }

    #[test]
    fn variant_cache_key_differs_by_position() {
        let a = Variant::new("1", 100, "C", "T");
        let b = Variant::new("1", 101, "C", "T");
        let opts = AnnotateOptions::default();
        assert_ne!(
            a.cache_key(Genome::Hg38, opts),
            b.cache_key(Genome::Hg38, opts)
        );
    }

    #[test]
    fn variant_serde_round_trip() {
        let v = Variant::new("X", 5_000_000, "ACGT", "A");
        let json = serde_json::to_string(&v).unwrap();

        assert!(json.contains(r#""ref""#));
        assert!(json.contains(r#""alt""#));

        let decoded: Variant = serde_json::from_str(&json).unwrap();
        assert_eq!(v, decoded);
    }

    #[test]
    fn annotate_options_default_all_false() {
        let opts = AnnotateOptions::default();
        assert!(opts.use_refseq.is_none());
        assert!(opts.use_ensembl.is_none());
        assert!(!opts.omit_acmg);
        assert!(!opts.omit_csq);
        assert!(!opts.omit_basic);
        assert!(!opts.omit_advanced);
        assert!(!opts.all_genes);
    }

    #[test]
    fn annotated_variant_deserializes_partial_json() {
        let json = r#"{
            "chr": "22",
            "pos": 28695868,
            "ref": "AG",
            "alt": "A",
            "gene_symbol": "EXAMPLE",
            "revel_score": 0.75
        }"#;

        let v: AnnotatedVariant = serde_json::from_str(json).unwrap();
        assert_eq!(v.chr.as_deref(), Some("22"));
        assert_eq!(v.pos, Some(28_695_868));
        assert_eq!(v.ref_allele.as_deref(), Some("AG"));
        assert_eq!(v.alt.as_deref(), Some("A"));
        assert_eq!(v.gene_symbol.as_deref(), Some("EXAMPLE"));
        assert!((v.revel_score.unwrap() - 0.75).abs() < f64::EPSILON);
        assert!(v.alphamissense_score.is_none());
        assert!(v.acmg_classification.is_none());
    }

    #[test]
    fn annotated_variant_serde_round_trip() {
        let json = r#"{
            "chr": "6",
            "pos": 160585140,
            "ref": "T",
            "alt": "G",
            "revel_score": 0.42,
            "alphamissense_score": 0.91,
            "acmg_classification": "Likely pathogenic"
        }"#;

        let v: AnnotatedVariant = serde_json::from_str(json).unwrap();
        let back = serde_json::to_string(&v).unwrap();
        let again: AnnotatedVariant = serde_json::from_str(&back).unwrap();

        assert_eq!(v.chr, again.chr);
        assert_eq!(v.revel_score, again.revel_score);
        assert_eq!(v.acmg_classification, again.acmg_classification);
    }
}
