//! Built-in admin UI for HomeCore.
//!
//! Serves the pre-built Leptos/WASM application as static files when enabled.
//! The dist directory is produced by `trunk build --release` in hc-web-leptos.
//!
//! SPA fallback: any request that doesn't match a static file returns
//! `index.html` so that client-side routing works correctly.

use axum::Router;
use std::path::PathBuf;
use tower_http::services::{ServeDir, ServeFile};
use tracing::info;

/// Build the admin UI router serving static files from `dist_path`.
///
/// All requests are served from the directory.  Requests that don't match
/// a file fall back to `index.html` (SPA routing).
///
/// The router is merged at the root level — static files are served from `/`,
/// but API routes at `/api/v1` take priority because they're added first.
pub fn router(dist_path: PathBuf) -> Router {
    let index = dist_path.join("index.html");
    info!(path = %dist_path.display(), "Serving admin UI from static files");
    Router::new().fallback_service(ServeDir::new(&dist_path).fallback(ServeFile::new(index)))
}
