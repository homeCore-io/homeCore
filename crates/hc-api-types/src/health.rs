//! Health-endpoint response.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String, // "ok"
    pub version: String,
}
