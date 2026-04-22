//! Library half of `hc-cli`. Exposes the HTTP client + command primitives
//! so they can be driven from tests (notably the Phase A E2E integration
//! test) without shelling out to the binary.

pub mod client;
pub mod config;
pub mod output;

pub use client::{Client, Transport};
pub use config::Config;
