//! Standalone HTTP API for the Sonos plugin.
//!
//! Mirrors the node-sonos-http-api URL scheme:
//!   GET /zones
//!   GET /favorites
//!   GET /playlists
//!   GET /pauseall
//!   GET /:room/state
//!   GET /:room/{play,pause,playpause,stop,next,previous}
//!   GET /:room/volume/:level           (level = 0-100, +N, -N)
//!   GET /:room/{mute,unmute,togglemute}
//!   GET /:room/bass/:level
//!   GET /:room/treble/:level
//!   GET /:room/loudness/:state         (on/off/toggle)
//!   GET /:room/shuffle/:state          (on/off/toggle)
//!   GET /:room/repeat/:state           (on/off/one/toggle)
//!   GET /:room/crossfade/:state        (on/off/toggle)
//!   GET /:room/seek/:seconds
//!   GET /:room/seekby/:seconds          (relative, ±N)
//!   GET /:room/trackseek/:index
//!   GET /:room/join/:target
//!   GET /:room/leave
//!   GET /:room/queue
//!   GET /:room/clearqueue
//!   GET /:room/queue/remove/:index
//!   GET /:room/queue/adduri/:uri
//!   GET /:room/queue/addnexturi/:uri
//!   GET /:room/favorites
//!   GET /:room/playlists
//!   GET /:room/favorite/:name
//!   GET /:room/playlist/:name
//!   GET /:room/playuri/:uri

pub mod content;
pub mod handlers;

use axum::{
    routing::{any, get},
    Router,
};
use tokio::sync::mpsc;
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;

use crate::events::NotifyEvent;
use crate::shared_state::AppState;

/// Build the Axum router.
pub fn router(state: AppState, event_tx: mpsc::Sender<(String, NotifyEvent)>) -> Router {
    use handlers::*;

    Router::new()
        .route("/", get(landing))
        // ── GENA NOTIFY callback (used by Sonos to push state changes) ───────
        .route("/sonos/callback/:uuid/:service", any(sonos_notify))
        // ── System ──────────────────────────────────────────────────────────
        .route("/zones", get(zones))
        .route("/favorites", get(all_favorites))
        .route("/playlists", get(all_playlists))
        .route("/pauseall", get(pause_all))
        // ── Per-room: state ─────────────────────────────────────────────────
        .route("/:room/state", get(room_state))
        .route("/:room/favorites", get(room_favorites))
        .route("/:room/playlists", get(room_playlists))
        .route("/:room/queue", get(queue))
        // ── Per-room: transport ─────────────────────────────────────────────
        .route("/:room/play", get(play))
        .route("/:room/pause", get(pause))
        .route("/:room/playpause", get(playpause))
        .route("/:room/stop", get(stop))
        .route("/:room/next", get(next))
        .route("/:room/previous", get(previous))
        // ── Per-room: volume ────────────────────────────────────────────────
        .route("/:room/volume/:level", get(volume))
        .route("/:room/mute", get(mute))
        .route("/:room/unmute", get(unmute))
        .route("/:room/togglemute", get(togglemute))
        // ── Per-room: EQ ────────────────────────────────────────────────────
        .route("/:room/bass/:level", get(bass))
        .route("/:room/treble/:level", get(treble))
        .route("/:room/loudness/:state", get(loudness))
        // ── Per-room: play modes ─────────────────────────────────────────────
        .route("/:room/shuffle/:state", get(shuffle))
        .route("/:room/repeat/:state", get(repeat))
        .route("/:room/crossfade/:state", get(crossfade))
        // ── Per-room: seek ───────────────────────────────────────────────────
        .route("/:room/seek/:seconds", get(seek))
        .route("/:room/seekby/:seconds", get(seekby))
        .route("/:room/trackseek/:index", get(trackseek))
        // ── Per-room: grouping ───────────────────────────────────────────────
        .route("/:room/join/:target", get(join))
        .route("/:room/leave", get(leave))
        // ── Per-room: queue mgmt ─────────────────────────────────────────────
        .route("/:room/clearqueue", get(clearqueue))
        .route("/:room/queue/remove/:index", get(queue_remove))
        .route("/:room/queue/adduri/:uri", get(queue_add))
        .route("/:room/queue/addnexturi/:uri", get(queue_add_next))
        // ── Per-room: content ────────────────────────────────────────────────
        .route("/:room/favorite/:name", get(play_favorite))
        .route("/:room/playlist/:name", get(play_playlist))
        .route("/:room/playuri/:uri", get(play_uri))
        .layer(
            ServiceBuilder::new()
                .layer(axum::Extension(event_tx))
                .layer(CorsLayer::permissive()),
        )
        .with_state(state)
}

/// Start the HTTP server.  Returns an error only if the bind address is invalid.
pub async fn serve(
    host: &str,
    port: u16,
    state: AppState,
    event_tx: mpsc::Sender<(String, NotifyEvent)>,
) -> anyhow::Result<()> {
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("API server bind {addr}: {e}"))?;

    tracing::info!(addr, "Sonos HTTP API listening");
    axum::serve(listener, router(state, event_tx))
        .await
        .map_err(|e| anyhow::anyhow!("API server error: {e}"))
}
