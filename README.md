# genebears

> **genebe** + **rs** (Rust) = *genebears*

A lightweight, async Rust client for the [GeneBe](https://genebe.net/) genetic
variant annotation API, with a **DuckDB-backed cache** and a **token-bucket
rate limiter** built in. It can also download and read databases of the
[GeneBe Hub](https://genebe.net/hub).

## Installation

```toml
[dependencies]
genebears = "*"
tokio     = { version = "1", features = ["full"] }
```

## Quick start

```rust
use genebears::{AnnotateOptions, AnnotatedVariant, ClientConfig, GeneBears, Genome, Variant};

#[tokio::main]
async fn main() -> Result<(), genebears::GeneBearError> {
    // Unauthenticated — fine for low-volume usage.
    let client = GeneBears::new(ClientConfig::default())?;

    let variants = vec![
        Variant::new("22", 28_695_868, "AG", "A"),
        Variant::new("6",  160_585_140, "T",  "G"),
    ];

    let results: Vec<AnnotatedVariant> = client
        .annotate(&variants, Genome::Hg38, AnnotateOptions::default())
        .await?;

    for v in &results {
        println!(
            "gene={:?}  revel={:?}  alphamissense={:?}  acmg={:?}",
            v.gene_symbol,
            v.revel_score,
            v.alphamissense_score,
            v.acmg_classification,
        );
    }
    Ok(())
}
```

`AnnotatedVariant` holds every field of the API. If you only need a few, any
type that serde can deserialize from the API's JSON works as well:

```rust
#[derive(serde::Deserialize)]
struct Scores {
    revel_score: Option<f64>,
    spliceai_max_score: Option<f64>,
}

let scores: Vec<Scores> = client.annotate(&variants, Genome::Hg38, AnnotateOptions::default()).await?;
```

Variants are sent in batches of 1 000, so lists of any length can be passed.

## Credential and cache usage

```rust
use genebears::{GeneBears, ClientConfig};

let config = ClientConfig::with_credentials("you@example.com", "YOUR_API_KEY")
    .with_cache("variants.duckdb");

let client = GeneBears::new(config)?;
```

On every subsequent run the cache is consulted first; only variants that have
never been seen before reach the network.

## Annotation options

```rust
use genebears::AnnotateOptions;

let opts = AnnotateOptions {
    use_refseq:    Some(true), // RefSeq transcripts only
    omit_advanced: true,       // skip ClinVar etc. for speed
    ..Default::default()
};
```

## GeneBe Hub

The [GeneBe Hub](https://genebe.net/hub) publishes annotation databases, e.g.
CADD, REVEL, SpliceAI or gnomAD, as parquet files. Downloads need a GeneBe
account and API key and are verified against their checksums. Databases are
read locally afterwards, without the API:

```rust
use genebears::{DatabaseId, Genome, Hub, Store};

let hub = Hub::new("you@example.com", "YOUR_API_KEY")?;
let store = Store::new(Store::default_root().unwrap());
let cadd: DatabaseId = "@genebe/cadd_hg38".parse()?;
hub.pull(&cadd, &store).await?;

#[derive(serde::Deserialize)]
struct Cadd {
    phred: f64,
}

// Rows per variant: usually none or one, some databases have one per gene.
let rows: Vec<Vec<Cadd>> = store.get(&cadd)?.lookup(&variants, Genome::Hg38, &["phred"])?;
```

The store has the same layout as the one of the
[GeneBe client](https://github.com/pstawinski/genebe-cli), so both can use the
same downloads. Without a version in the id, `pull` downloads the newest version
of the Hub and `get` uses the newest installed one. Pulling a new version keeps
the old ones, so keep an eye on disk space (`@genebe/cadd_hg38` alone has 53 GB).

Lookups read whole chromosomes and block, so look up many variants at once, in
async code e.g. with `tokio::task::spawn_blocking`. Split
multiallelic records and left-align indels (e.g. with `bcftools norm`) first,
since other indels are not found. Reference alleles are not checked against the
genome.

## Authors
- Felix Wiegand
