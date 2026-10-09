//! # genebears
//!
//! A lightweight Rust client for the [GeneBe](https://genebe.net/) genetic
//! variant annotation API. Databases from the [GeneBe Hub](https://genebe.net/hub)
//! can be downloaded into a local [`Store`] and are then used instead of the API.
//!
//! ## Quick start
//!
//! ```rust, no_run
//! use genebears::{AnnotateOptions, ClientConfig, Field, GeneBears, Genome, Variant};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), genebears::GeneBearError> {
//!     let client = GeneBears::new(ClientConfig::default())?;
//!
//!     let variants = vec![
//!         Variant::new("22", 28_695_868, "AG", "A"),
//!         Variant::new("6",  160_585_140, "T",  "G"),
//!     ];
//!     let revel = Field::api("revel_score");
//!     let acmg = Field::api("acmg_classification");
//!     let fields = [revel.clone(), acmg.clone()];
//!
//!     let annotations = client
//!         .annotate_variants(&variants, Genome::Hg38, &fields, AnnotateOptions::default())
//!         .await?;
//!
//!     for annotation in &annotations {
//!         println!("revel={:?}  acmg={:?}", annotation.f64(&revel), annotation.str(&acmg));
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
pub use models::{AnnotateOptions, AnnotatedVariant, Annotation, Field, Genome, Variant, Warning};
pub use store::{Installed, Store};
