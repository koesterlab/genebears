//! # genebears
//!
//! A lightweight Rust client for the [GeneBe](https://genebe.net/) genetic
//! variant annotation API, and for annotating variants locally with databases
//! from the [GeneBe Hub](https://genebe.net/hub) (see [`Hub`] and [`Store`]).
//!
//! ## Quick start
//!
//! ```rust, no_run
//! use genebears::{GeneBears, ClientConfig, Variant, Genome, AnnotateOptions};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), genebears::GeneBearError> {
//!     let client = GeneBears::new(ClientConfig::default())?;
//!
//!     let variants = vec![
//!         Variant::new("22", 28_695_868, "AG", "A"),
//!         Variant::new("6",  160_585_140, "T",  "G"),
//!     ];
//!
//!     let results = client
//!         .annotate_variants(&variants, Genome::Hg38, AnnotateOptions::default())
//!         .await?;
//!
//!     for v in &results {
//!         println!(
//!             "gene={:?}  revel={:?}  alphamissense={:?}  acmg={:?}",
//!             v.gene_symbol, v.revel_score, v.alphamissense_score, v.acmg_classification,
//!         );
//!     }
//!     Ok(())
//! }
//! ```

pub mod cache;
pub mod client;
pub mod error;
pub mod hub;
pub mod models;
pub mod rate_limiter;
pub mod store;

pub use client::{ClientConfig, GeneBears};
pub use error::GeneBearError;
pub use hub::{Database, DatabaseId, Hub};
pub use models::{AnnotateOptions, AnnotatedVariant, Genome, Variant};
pub use store::{Installed, Store};
