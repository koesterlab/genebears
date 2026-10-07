# genebears

> **genebe** + **rs** (Rust) = *genebears*

A lightweight, async Rust client for the [GeneBe](https://genebe.net/) genetic
variant annotation API, with a **DuckDB-backed cache** and a **token-bucket
rate limiter** built in.

## Installation

```toml
[dependencies]
genebears = "*"
tokio     = { version = "1", features = ["full"] }
```

## Quick start

```rust
use genebears::{GeneBears, ClientConfig, Variant, Genome, AnnotateOptions};

#[tokio::main]
async fn main() -> Result<(), genebears::GeneBearError> {
    // Unauthenticated — fine for low-volume usage.
    let client = GeneBears::new(ClientConfig::default())?;

    let variants = vec![
        Variant::new("22", 28_695_868, "AG", "A"),
        Variant::new("6",  160_585_140, "T",  "G"),
    ];

    let results = client
        .annotate_variants(&variants, Genome::Hg38, AnnotateOptions::default())
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

## Large variant lists

`annotate_variants_chunked` splits automatically at 1 000 and respects the
rate limiter between chunks:

```rust
let results = client
    .annotate_variants_chunked(&my_big_vec, Genome::Hg38, AnnotateOptions::default())
    .await?;
```

## Local annotation with the GeneBe Hub

The [GeneBe Hub](https://genebe.net/hub) publishes the databases behind many
GeneBe annotations as parquet files. After downloading them, you can annotate
variants locally, without sending any request to GeneBe. Databases are stored
in the same directory and layout as the
[GeneBe client](https://github.com/pstawinski/genebe-cli) uses, so both can use
the same downloads.

```rust
use genebears::{ClientConfig, DatabaseId, GeneBears, Genome, Store, Variant};

let client = GeneBears::new(ClientConfig::with_credentials("you@example.com", "YOUR_API_KEY"))?;
let hub = client.hub();
let store = Store::new(Store::default_root().unwrap());
for id in ["@genebe/revel", "@genebe/alpha_missense", "@genebe/spliceai"] {
    // Downloads the newest version unless it is installed already.
    hub.pull(&id.parse::<DatabaseId>()?, &store).await?;
}

let variants = vec![Variant::new("6", 160_585_140, "T", "G")];
let annotations = tokio::task::spawn_blocking(move || {
    store.annotate_variants(&variants, Genome::Hg38)
})
.await??;
```

For hg38, `Store::annotate_variants` fills these fields from the installed
databases, with the same values as the API:

| Field                 | Database                   |
|-----------------------|----------------------------|
| `revel_score`         | `@genebe/revel`            |
| `alphamissense_score` | `@genebe/alpha_missense`   |
| `spliceai_max_score`  | `@genebe/spliceai`         |
| `gnomad_exomes_af`    | `@genebe/gnomad_exomes4`   |
| `gnomad_genomes_af`   | `@genebe/gnomad_genomes4`  |

Other fields (e.g. ACMG classifications) and other genomes still need the API.
For hg19, the API lifts variants over to hg38, which the hg19 databases of the
Hub do not reproduce.

Split multiallelic records and left-align indels (e.g. with `bcftools norm`)
before annotating. Indels that are not left-aligned get a `warning` instead of
annotations, and reference alleles are not checked against the genome.

The newest installed version of a database is used. Pulling a new version keeps
the old ones, so keep an eye on disk space (`@genebe/gnomad_genomes4` alone has
34 GB). Lookups read whole chromosomes of a database, so annotate many variants
at once. Since `annotate_variants` blocks, the example runs it with
`spawn_blocking`. Downloads need a GeneBe account and API key and are verified
against their checksums. Do not pull a database while annotating with it.


## Authors
- Felix Wiegand
