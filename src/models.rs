use clap::ValueEnum;
use serde::{Deserialize, Serialize};

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

    pub fn cache_key(&self, genome: Genome) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.chr,
            self.pos,
            self.ref_allele,
            self.alt_allele,
            genome.as_str()
        )
    }

    /// Convert into the key of GeneBe Hub databases: chromosome without `chr`, 0-based
    /// position, number of deleted bases and inserted bases. Shared bases are trimmed from
    /// the end (keeping one base per allele), the start and the end again, which gives the
    /// keys GeneBe builds from normalized VCF records. Returns `None` for indels that are not
    /// provably left-aligned, non-ACGT alleles and chromosomes other than 1-22, X, Y and M.
    pub(crate) fn to_spdi(&self) -> Option<Spdi> {
        let seq = ["chr", "CHR", "Chr"]
            .iter()
            .find_map(|prefix| self.chr.strip_prefix(prefix))
            .unwrap_or(&self.chr);
        let seq = if seq == "MT" { "M" } else { seq };
        if !CHROMOSOMES.contains(&seq) {
            return None;
        }

        let reference = self.ref_allele.to_ascii_uppercase();
        let alternative = self.alt_allele.to_ascii_uppercase();
        if !is_dna(&reference) || !is_dna(&alternative) {
            return None;
        }
        let (mut deleted, mut inserted) = (reference.as_str(), alternative.as_str());
        while deleted.len() > 1
            && inserted.len() > 1
            && deleted.as_bytes().last() == inserted.as_bytes().last()
        {
            deleted = &deleted[..deleted.len() - 1];
            inserted = &inserted[..inserted.len() - 1];
        }
        let leading = deleted
            .bytes()
            .zip(inserted.bytes())
            .take_while(|(r, a)| r == a)
            .count();
        let (deleted, inserted) = (&deleted[leading..], &inserted[leading..]);
        let trailing = deleted
            .bytes()
            .rev()
            .zip(inserted.bytes().rev())
            .take_while(|(r, a)| r == a)
            .count();
        let deleted = &deleted[..deleted.len() - trailing];
        let inserted = &inserted[..inserted.len() - trailing];
        if deleted.is_empty() && inserted.is_empty() {
            return None;
        }

        if deleted.is_empty() || inserted.is_empty() {
            // A pure indel is left-aligned if the base before it differs from its last base.
            let indel = if deleted.is_empty() {
                inserted
            } else {
                deleted
            };
            let before = reference[..leading].bytes().last()?;
            if indel.bytes().last() == Some(before) {
                return None;
            }
        }

        Some(Spdi {
            seq: seq.to_string(),
            pos: u32::try_from(self.pos.checked_sub(1)? + leading as u64).ok()?,
            del: deleted.len() as u32,
            ins: inserted.to_string(),
        })
    }
}

const CHROMOSOMES: [&str; 25] = [
    "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16", "17",
    "18", "19", "20", "21", "22", "X", "Y", "M",
];

fn is_dna(allele: &str) -> bool {
    !allele.is_empty()
        && allele
            .bytes()
            .all(|base| matches!(base, b'A' | b'C' | b'G' | b'T'))
}

/// A variant in the notation of GeneBe Hub databases.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Spdi {
    pub seq: String,
    pub pos: u32,
    pub del: u32,
    pub ins: String,
}

/// Controls which sections of the annotation the API should compute.
///
/// All fields default to `false` / `None`, which means the API returns
/// everything.
#[derive(Debug, Clone, Default)]
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

/// Top-level response envelope returned by the GeneBe API.
#[derive(Debug, Deserialize)]
pub(crate) struct ApiResponse {
    pub variants: Vec<AnnotatedVariant>,
    pub _message: Option<String>,
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
        let key = v.cache_key(Genome::Hg38);
        assert_eq!(key, "22:28695868:AG:A:hg38");
    }

    #[test]
    fn variant_cache_key_differs_by_genome() {
        let v = Variant::new("1", 100, "C", "T");
        let k38 = v.cache_key(Genome::Hg38);
        let k19 = v.cache_key(Genome::Hg19);
        let kt2t = v.cache_key(Genome::T2t);
        assert_ne!(k38, k19);
        assert_ne!(k38, kt2t);
        assert_ne!(k19, kt2t);
    }

    #[test]
    fn variant_cache_key_differs_by_position() {
        let a = Variant::new("1", 100, "C", "T");
        let b = Variant::new("1", 101, "C", "T");
        assert_ne!(a.cache_key(Genome::Hg38), b.cache_key(Genome::Hg38));
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

    fn spdi(seq: &str, pos: u32, del: u32, ins: &str) -> Option<Spdi> {
        Some(Spdi {
            seq: seq.to_string(),
            pos,
            del,
            ins: ins.to_string(),
        })
    }

    #[test]
    fn to_spdi_matches_genebe_examples() {
        // Examples from the GeneBe Hub format description.
        assert_eq!(
            Variant::new("chr1", 1000, "AG", "TT").to_spdi(),
            spdi("1", 999, 2, "TT")
        );
        assert_eq!(
            Variant::new("1", 12, "A", "C").to_spdi(),
            spdi("1", 11, 1, "C")
        );
    }

    #[test]
    fn to_spdi_removes_shared_bases_of_indels() {
        assert_eq!(
            Variant::new("2", 200, "ATG", "A").to_spdi(),
            spdi("2", 200, 2, "")
        );
        assert_eq!(
            Variant::new("3", 300, "C", "CTAG").to_spdi(),
            spdi("3", 300, 0, "TAG")
        );
    }

    #[test]
    fn to_spdi_removes_shared_bases_at_the_end() {
        assert_eq!(
            Variant::new("1", 100, "ACG", "ATG").to_spdi(),
            spdi("1", 100, 1, "T")
        );
        assert_eq!(
            Variant::new("1", 100, "CAT", "CT").to_spdi(),
            spdi("1", 100, 1, "")
        );
    }

    #[test]
    fn to_spdi_keeps_complex_changes() {
        assert_eq!(
            Variant::new("X", 400, "GCA", "TTAG").to_spdi(),
            spdi("X", 399, 3, "TTAG")
        );
    }

    #[test]
    fn to_spdi_accepts_left_aligned_repeats() {
        assert_eq!(
            Variant::new("1", 100, "ATT", "AT").to_spdi(),
            spdi("1", 100, 1, "")
        );
        assert_eq!(
            Variant::new("1", 100, "CAGAG", "CAG").to_spdi(),
            spdi("1", 100, 2, "")
        );
        assert_eq!(
            Variant::new("1", 100, "CAT", "CAAT").to_spdi(),
            spdi("1", 100, 0, "A")
        );
    }

    #[test]
    fn to_spdi_rejects_shiftable_indels() {
        // Deleting or inserting an A next to another A can be shifted to the left.
        assert_eq!(Variant::new("1", 100, "AA", "A").to_spdi(), None);
        assert_eq!(Variant::new("1", 100, "A", "AA").to_spdi(), None);
        assert_eq!(Variant::new("1", 100, "TAT", "TATAT").to_spdi(), None);
        // Without a shared first base the alignment is unknown.
        assert_eq!(Variant::new("1", 100, "AC", "C").to_spdi(), None);
        // Left-aligned versions of the above.
        assert_eq!(
            Variant::new("1", 100, "CA", "C").to_spdi(),
            spdi("1", 100, 1, "")
        );
        assert_eq!(
            Variant::new("1", 100, "C", "CAT").to_spdi(),
            spdi("1", 100, 0, "AT")
        );
    }

    #[test]
    fn to_spdi_normalizes_chromosome_and_case() {
        assert_eq!(
            Variant::new("chrMT", 10, "a", "g").to_spdi(),
            spdi("M", 9, 1, "G")
        );
        assert_eq!(
            Variant::new("MT", 10, "A", "G").to_spdi(),
            spdi("M", 9, 1, "G")
        );
        assert_eq!(
            Variant::new("chrM", 10, "A", "G").to_spdi(),
            spdi("M", 9, 1, "G")
        );
    }

    #[test]
    fn to_spdi_rejects_unsupported_variants() {
        assert_eq!(Variant::new("1", 100, "A", "N").to_spdi(), None);
        assert_eq!(Variant::new("1", 100, "A", "*").to_spdi(), None);
        assert_eq!(Variant::new("1", 100, "A", "<DEL>").to_spdi(), None);
        assert_eq!(Variant::new("1", 100, "A", "A").to_spdi(), None);
        assert_eq!(Variant::new("1", 0, "A", "G").to_spdi(), None);
        assert_eq!(
            Variant::new("chr1_KI270706v1_random", 100, "A", "G").to_spdi(),
            None
        );
        assert_eq!(Variant::new("GL000192.1", 100, "A", "G").to_spdi(), None);
        assert_eq!(Variant::new("23", 100, "A", "G").to_spdi(), None);
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
