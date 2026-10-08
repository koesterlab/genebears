# genebears

> **genebe** + **rs** (Rust) = *genebears*

A lightweight, async Rust client for the [GeneBe](https://genebe.net/) genetic
variant annotation API, with a **DuckDB-backed cache** and a **token-bucket
rate limiter** built in. Databases from the [GeneBe Hub](https://genebe.net/hub)
can be downloaded and are then used instead of the API.

## Installation

```toml
[dependencies]
genebears = "*"
tokio     = { version = "1", features = ["full"] }
```

## Quick start

```rust
use genebears::{AnnotateOptions, ClientConfig, Field, GeneBears, Genome, Variant};

#[tokio::main]
async fn main() -> Result<(), genebears::GeneBearError> {
    // Unauthenticated, fine for low-volume usage.
    let client = GeneBears::new(ClientConfig::default())?;

    let variants = vec![
        Variant::new("22", 28_695_868, "AG", "A"),
        Variant::new("6",  160_585_140, "T",  "G"),
    ];
    let revel = Field::api("revel_score");
    let acmg = Field::api("acmg_classification");
    let fields = [revel.clone(), acmg.clone()];

    let annotations = client
        .annotate_variants(&variants, Genome::Hg38, &fields, AnnotateOptions::default())
        .await?;

    for annotation in &annotations {
        println!("revel={:?}  acmg={:?}", annotation.f64(&revel), annotation.str(&acmg));
    }
    Ok(())
}
```

## Fields

`annotate_variants` returns one `Annotation` per variant, in the order of the
variants, with a value for each requested field:

* `Field::api(name)` is a field of the API response, named as in GeneBe's JSON,
  e.g. `acmg_score`, `gnomad_exomes_af` or `consequences`.
* `Field::hub(database, column)` is a column of a GeneBe Hub database, e.g.
  `Field::hub("@genebe/cadd_hg38", "phred")`. The database has to be installed
  (see below).

Fields can also be parsed from strings like `acmg_score` or
`@genebe/cadd_hg38/phred`. Values are `serde_json::Value`s; `Annotation::f64`
and `Annotation::str` read numbers and strings. Variants that could not be
annotated completely, e.g. because the reference allele does not match the
genome, have `warnings`.

To get everything the API knows about a variant instead, `annotate_api` returns
its complete record.

## Credentials, cache and Hub databases

```rust
use genebears::{ClientConfig, GeneBears, Store};

let config = ClientConfig::with_credentials("you@example.com", "YOUR_API_KEY")
    .with_cache("variants.duckdb")
    .with_store(Store::new(Store::default_root().unwrap()));

let client = GeneBears::new(config)?;
```

Each field is read from one source:

1. Hub databases in the store. Besides their own columns, these provide the
   hg38 API fields GeneBe takes from them, with the same values (see below).
   Options like `omit_basic` do not apply to them.
2. Otherwise the cache, which holds the API records of variants seen before.
3. And for variants not in the cache, the API, asked once for all of them.

| Database                  | API fields                                                          |
|---------------------------|---------------------------------------------------------------------|
| `@genebe/revel`           | `revel_score`                                                       |
| `@genebe/alpha_missense`  | `alphamissense_score`                                               |
| `@genebe/spliceai`        | `spliceai_max_score`                                                |
| `@genebe/gnomad_exomes4`  | `gnomad_exomes_af`, `gnomad_exomes_ac`, `gnomad_exomes_homalt`      |
| `@genebe/gnomad_genomes4` | `gnomad_genomes_af`, `gnomad_genomes_ac`, `gnomad_genomes_homalt`   |

If every requested field comes from installed databases, annotating normalized
variants needs no request to GeneBe. ACMG classifications, consequences and
other genomes always come from the API, since there are no Hub databases for
them. For hg19, the API lifts variants over to hg38, which the hg19 databases of
the Hub do not reproduce.

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

The [GeneBe Hub](https://genebe.net/hub) publishes the databases behind many
GeneBe annotations as parquet files. Downloads need a GeneBe account and API
key and are verified against their checksums:

```rust
use genebears::{DatabaseId, Store};

let hub = client.hub();
let store = Store::new(Store::default_root().unwrap());
for id in ["@genebe/revel", "@genebe/alpha_missense", "@genebe/spliceai"] {
    // Downloads the newest version unless it is installed already.
    hub.pull(&id.parse::<DatabaseId>()?, &store).await?;
}
```

Databases are stored in the same directory and layout as the
[GeneBe client](https://github.com/pstawinski/genebe-cli) uses, so both can use
the same downloads. The newest installed version of a database is used, unless
a field names one, e.g. `Field::hub("@genebe/cadd_hg38:0.0.2-1.7.0", "phred")`.
Pulling a new version keeps the old ones, so keep an eye on disk space
(`@genebe/gnomad_genomes4` alone has 34 GB).

Split multiallelic records and left-align indels (e.g. with `bcftools norm`)
before annotating. Variants that cannot be looked up in Hub databases, e.g.
indels that are not left-aligned, are annotated by the API instead. Hub columns
stay empty for them, with a `warning`. Reference alleles are not checked against
the genome. Lookups read whole chromosomes of a database, so annotate many variants
at once. Do not pull a database while annotating with it.


## Authors
- Felix Wiegand
