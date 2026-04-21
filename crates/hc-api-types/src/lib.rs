//! Shared request/response types for the homeCore REST API.
//!
//! This crate lives between `hc-api` (server) and API clients
//! (`hc-cli`, `hc-mcp`, web UIs). The types here are the wire format —
//! what a caller sends and what the server returns.
//!
//! Scope today: auth + users + api-keys + health. Other endpoints (devices,
//! rules, etc.) still use ad-hoc JSON on both sides and will migrate lazily.

pub mod api_keys;
pub mod auth;
pub mod health;

// Re-export the pieces of hc-auth that clients need to consume responses
// without depending on hc-auth directly.
pub use hc_auth::{Role, UserInfo};
