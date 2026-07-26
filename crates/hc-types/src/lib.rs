//! `hc-types` — shared domain types for the HomeCore platform.
//!
//! This crate is the single source of truth for all types that cross crate
//! boundaries: device state, events, automation rules, and MQTT messages.
//! Every other crate in the workspace depends on this one; it intentionally
//! has no internal (HomeCore) dependencies.

/// Wire-protocol version this build emits and consumes — the SemVer of
/// `hc-types` itself. Used by core's startup log and `state_bridge`'s
/// SDK-compat check (component versioning plan, Phase B). Plugins emit
/// their `plugin-sdk-rs` version in heartbeats; core compares against
/// this constant. Treats MINOR as breaking for 0.x and MAJOR as breaking
/// for 1.0+, matching `state_bridge::sdk_versions_compatible`.
pub const PROTOCOL_VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod dashboard;
pub mod device;
pub mod event;
pub mod log_line;
pub mod mqtt;
pub mod plugin_capabilities;
pub mod rule;
pub mod schema;

/// The rule vocabulary, derived from the rule types rather than written down.
/// See [`vocabulary`] for why that distinction is the whole point.
#[cfg(feature = "schema")]
pub mod vocabulary;

pub use log_line::LogLine;
pub use plugin_capabilities::{Action, Capabilities, Concurrency, ItemOp, RequiresRole};
pub use schema::{AttributeKind, AttributeSchema, DeviceSchema};
