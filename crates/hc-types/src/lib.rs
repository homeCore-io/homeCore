//! `hc-types` — shared domain types for the HomeCore platform.
//!
//! This crate is the single source of truth for all types that cross crate
//! boundaries: device state, events, automation rules, and MQTT messages.
//! Every other crate in the workspace depends on this one; it intentionally
//! has no internal (HomeCore) dependencies.

pub mod device;
pub mod event;
pub mod mqtt;
pub mod rule;
