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


## Authors
- Felix Wiegand
