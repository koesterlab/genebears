//! # genebears
//!
//! A lightweight Rust client for the [GeneBe](https://genebe.net/) genetic
//! variant annotation API and the databases of the [GeneBe Hub](https://genebe.net/hub).
//!
//! ## Quick start
//!
//! ```rust, no_run
//! use genebears::{AnnotateOptions, AnnotatedVariant, ClientConfig, GeneBears, Genome, Variant};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), genebears::GeneBearError> {
//!     let client = GeneBears::new(ClientConfig::default())?;
//!
//!     let variants = vec![
//!         Variant::new("22", 28_695_868, "AG", "A"),
//!         Variant::new("6",  160_585_140, "T",  "G"),
//!     ];
//!     let annotations: Vec<AnnotatedVariant> = client
//!         .annotate(&variants, Genome::Hg38, AnnotateOptions::default())
//!         .await?;
//!
//!     for annotation in &annotations {
//!         println!("{:?}  acmg={:?}", annotation.gene_symbol, annotation.acmg_classification);
//!     }
//!     Ok(())
//! }
//! ```

mod cache;
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
