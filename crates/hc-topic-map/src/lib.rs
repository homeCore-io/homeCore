//! `hc-topic-map` — config-driven MQTT topic translation and payload transforms.
//!
//! All device type schemas and ecosystem profiles are loaded from files at
//! runtime — nothing is hardcoded in Rust. New ecosystems are added by dropping
//! a `.toml` profile into `config/profiles/`.
//!
//! # Main types
//!
//! - [`EcosystemRouter`] — matches MQTT topics against loaded profiles and
//!   translates them to/from the HomeCore canonical schema.
//! - [`DeviceTypeRegistry`] — resolves device type names to JSON Schema objects.
//!
//! # Quick start
//!
//! ```no_run
//! use hc_topic_map::{EcosystemRouter, DeviceTypeRegistry};
//! use hc_topic_map::loader::load_profiles_from_dir;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let profiles = load_profiles_from_dir("config/profiles")?;
//! let router   = EcosystemRouter::new(profiles, None)?;
//!
//! let type_registry = DeviceTypeRegistry::from_file("config/profiles/device-types.toml")?;
//! # Ok(())
//! # }
//! ```

pub mod coerce;
pub mod device_types;
pub mod loader;
pub mod pattern;
pub mod profile;
pub mod router;

pub use device_types::DeviceTypeRegistry;
pub use router::{EcosystemRouter, InboundResult, OutboundResult};
