//! HTTP API route handlers.
//!
//! URL scheme mirrors node-sonos-http-api:
//!   GET /{room}/{action}
//!   GET /{room}/{action}/{param}
//!   GET /zones
//!   GET /favorites
//!   GET /playlists
//!   GET /pauseall

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use sonor::{RepeatMode, Speaker};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::content;
use crate::events::{self, NotifyEvent};
use crate::shared_state::AppState;
use crate::speaker;

// ---------------------------------------------------------------------------
// Helper types / macros
// ---------------------------------------------------------------------------

fn ok() -> Response {
    Json(json!({"status": "success"})).into_response()
}

fn err_resp(code: StatusCode, msg: impl std::fmt::Display) -> Response {
    (code, Json(json!({"error": msg.to_string()}))).into_response()
}

fn not_found(what: &str) -> Response {
    err_resp(StatusCode::NOT_FOUND, format!("{what} not found"))
}

fn offline() -> Response {
    err_resp(StatusCode::SERVICE_UNAVAILABLE, "speaker is offline")
}

fn bad_req(msg: &str) -> Response {
    err_resp(StatusCode::BAD_REQUEST, msg)
}

/// Clone a speaker handle by room name, enforcing availability.
async fn get_speaker(state: &AppState, room: &str) -> Result<Speaker, Response> {
    let st = state.read().await;
    match st.find_by_room(room) {
        None => Err(not_found("room")),
        Some(e) if !e.available => Err(offline()),
        Some(e) => Ok(e.speaker.clone()),
    }
}

/// Project raw content items (favorites/playlists) into a clean, stable list for
/// HTTP responses: a 0-based `index` (the value `/:room/favorite/:index` expects),
/// the Sonos object `id` for reference, plus title/uri/art. The bulky DIDL-Lite
/// `metadata` blob used internally for playback is intentionally omitted.
fn content_view(items: &[Value]) -> Value {
    let list: Vec<Value> = items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let mut obj = json!({
                "index": i,
                "id":    it.get("id").cloned().unwrap_or(Value::Null),
                "title": it.get("title").cloned().unwrap_or(Value::Null),
                "uri":   it.get("uri").cloned().unwrap_or(Value::Null),
            });
            if let Some(a) = it.get("albumArtUri") {
                obj["albumArtUri"] = a.clone();
            }
            obj
        })
        .collect();
    json!({ "count": list.len(), "items": list })
}

fn repeat_to_str(r: &RepeatMode) -> &'static str {
    match r {
        RepeatMode::None => "none",
        RepeatMode::One => "one",
        RepeatMode::All => "all",
    }
}

const LANDING_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width,initial-scale=1" />
    <title>HomeCore Sonos API</title>
    <style>
        :root {
            --bg: #0f1419;
            --panel: #17212b;
            --text: #e6edf3;
            --muted: #9fb0c0;
            --accent: #4ec9b0;
            --warn: #f4c430;
            --ok: #6ee7a8;
            --code: #0b1117;
            --border: #2a3a4a;
        }
        * { box-sizing: border-box; }
        body {
            margin: 0;
            font-family: "Segoe UI", sans-serif;
            background: radial-gradient(1200px 800px at 85% -10%, #1f2f3f 0%, var(--bg) 55%);
            color: var(--text);
            line-height: 1.45;
        }
        .wrap { max-width: 1000px; margin: 0 auto; padding: 24px; }
        .hero {
            background: linear-gradient(135deg, rgba(78,201,176,0.14), rgba(110,231,168,0.06));
            border: 1px solid var(--border);
            border-radius: 14px;
            padding: 20px;
            margin-bottom: 18px;
        }
        h1 { margin: 0 0 8px; font-size: 1.9rem; }
        h2 { margin: 18px 0 8px; font-size: 1.15rem; }
        p { margin: 6px 0; color: var(--muted); }
        .chip {
            display: inline-block;
            padding: 4px 10px;
            border-radius: 999px;
            border: 1px solid var(--border);
            margin-right: 8px;
            margin-top: 8px;
            color: var(--text);
            background: rgba(255,255,255,0.03);
            font-size: 0.9rem;
        }
        .grid {
            display: grid;
            grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
            gap: 12px;
            margin-top: 10px;
        }
        .card {
            background: var(--panel);
            border: 1px solid var(--border);
            border-radius: 12px;
            padding: 14px;
        }
        .ep {
            display: block;
            margin: 6px 0;
            padding: 8px 10px;
            border-radius: 8px;
            background: var(--code);
            border: 1px solid #1f2a35;
            font-family: "Consolas", "DejaVu Sans Mono", monospace;
            font-size: 0.89rem;
            color: #d5e6f7;
            text-decoration: none;
            overflow-wrap: anywhere;
        }
        .method {
            display: inline-block;
            width: 40px;
            font-weight: 700;
            color: var(--ok);
        }
        .example {
            margin-top: 8px;
            background: var(--code);
            border: 1px solid #1f2a35;
            border-radius: 8px;
            padding: 10px;
            font-family: "Consolas", "DejaVu Sans Mono", monospace;
            font-size: 0.85rem;
            color: #cfe2f5;
            white-space: pre-wrap;
            overflow-wrap: anywhere;
        }
        .note {
            border-left: 3px solid var(--warn);
            padding: 8px 10px;
            background: rgba(244,196,48,0.08);
            border-radius: 6px;
            color: #f6dd92;
            margin-top: 8px;
        }
        .footer {
            margin-top: 16px;
            color: var(--muted);
            font-size: 0.9rem;
        }
    </style>
</head>
<body>
    <div class="wrap">
        <section class="hero">
            <h1>HomeCore Sonos Control API</h1>
            <p>Direct HTTP controls for Sonos speakers discovered by this plugin.</p>
            <span class="chip">Route style: node-sonos-http-api compatible</span>
            <span class="chip">JSON responses</span>
            <span class="chip">All control endpoints use GET</span>
        </section>

        <section class="grid">
            <article class="card">
                <h2>System Endpoints</h2>
                <a class="ep" href="/zones"><span class="method">GET</span>/zones</a>
                <a class="ep" href="/favorites"><span class="method">GET</span>/favorites</a>
                <a class="ep" href="/playlists"><span class="method">GET</span>/playlists</a>
                <a class="ep" href="/pauseall"><span class="method">GET</span>/pauseall</a>
            </article>

            <article class="card">
                <h2>Room State and Queue</h2>
                <div class="ep"><span class="method">GET</span>/:room/state</div>
                <div class="ep"><span class="method">GET</span>/:room/queue</div>
                <div class="ep"><span class="method">GET</span>/:room/favorites</div>
                <div class="ep"><span class="method">GET</span>/:room/playlists</div>
                <div class="example">Examples:
GET /Living%20Room/state
GET /Kitchen/queue</div>
            </article>

            <article class="card">
                <h2>Transport</h2>
                <div class="ep"><span class="method">GET</span>/:room/play</div>
                <div class="ep"><span class="method">GET</span>/:room/pause</div>
                <div class="ep"><span class="method">GET</span>/:room/playpause</div>
                <div class="ep"><span class="method">GET</span>/:room/stop</div>
                <div class="ep"><span class="method">GET</span>/:room/next</div>
                <div class="ep"><span class="method">GET</span>/:room/previous</div>
            </article>

            <article class="card">
                <h2>Volume and EQ</h2>
                <div class="ep"><span class="method">GET</span>/:room/volume/:level</div>
                <div class="ep"><span class="method">GET</span>/:room/mute</div>
                <div class="ep"><span class="method">GET</span>/:room/unmute</div>
                <div class="ep"><span class="method">GET</span>/:room/togglemute</div>
                <div class="ep"><span class="method">GET</span>/:room/bass/:level</div>
                <div class="ep"><span class="method">GET</span>/:room/treble/:level</div>
                <div class="ep"><span class="method">GET</span>/:room/loudness/:state</div>
                <div class="example">Valid values:
volume: 0..100, +N, -N
bass/treble: -10..10
state: on | off | toggle</div>
            </article>

            <article class="card">
                <h2>Modes, Seek, Grouping</h2>
                <div class="ep"><span class="method">GET</span>/:room/shuffle/:state</div>
                <div class="ep"><span class="method">GET</span>/:room/repeat/:state</div>
                <div class="ep"><span class="method">GET</span>/:room/crossfade/:state</div>
                <div class="ep"><span class="method">GET</span>/:room/seek/:seconds</div>
                <div class="ep"><span class="method">GET</span>/:room/seekby/:seconds</div>
                <div class="ep"><span class="method">GET</span>/:room/trackseek/:index</div>
                <div class="ep"><span class="method">GET</span>/:room/join/:target</div>
                <div class="ep"><span class="method">GET</span>/:room/leave</div>
            </article>

            <article class="card">
                <h2>Queue and Content Playback</h2>
                <div class="ep"><span class="method">GET</span>/:room/clearqueue</div>
                <div class="ep"><span class="method">GET</span>/:room/queue/remove/:index</div>
                <div class="ep"><span class="method">GET</span>/:room/queue/adduri/:uri</div>
                <div class="ep"><span class="method">GET</span>/:room/queue/addnexturi/:uri</div>
                <div class="ep"><span class="method">GET</span>/:room/favorite/:index</div>
                <div class="ep"><span class="method">GET</span>/:room/playlist/:index</div>
                <div class="ep"><span class="method">GET</span>/:room/playuri/:uri</div>
                <div class="note">URI path segments must be URL-encoded before calling.</div>
            </article>
        </section>

        <section class="card">
            <h2>Quick Start</h2>
            <div class="example">1) List available zones:
GET /zones

2) Start playback in Living Room:
GET /Living%20Room/play

3) Set volume to 35:
GET /Living%20Room/volume/35

4) Play favorite index 0:
GET /Living%20Room/favorite/0</div>
        </section>

        <div class="footer">
            Sonos callback endpoint used internally: ANY /sonos/callback/:uuid/:service
        </div>
    </div>
</body>
</html>
"#;

// ---------------------------------------------------------------------------
// System endpoints
// ---------------------------------------------------------------------------

/// GET / - landing page with API endpoint docs and examples.
pub async fn landing() -> Html<&'static str> {
    Html(LANDING_PAGE)
}

/// GET /zones — all zone groups with state.
pub async fn zones(State(state): State<AppState>) -> Response {
    // Get zone group state from the first available speaker
    let speaker = {
        let st = state.read().await;
        st.speakers
            .values()
            .find(|e| e.available)
            .map(|e| e.speaker.clone())
    };
    let speaker = match speaker {
        Some(s) => s,
        None => return err_resp(StatusCode::SERVICE_UNAVAILABLE, "no speakers available"),
    };

    let zone_map = match speaker.zone_group_state().await {
        Ok(m) => m,
        Err(e) => return err_resp(StatusCode::BAD_GATEWAY, e),
    };

    let st = state.read().await;
    let zones: Vec<Value> = zone_map
        .iter()
        .map(|(coord_uuid, members)| {
            let coord_entry = st.speakers.get(coord_uuid);
            let coord_room = coord_entry
                .map(|e| e.room_name.as_str())
                .unwrap_or(coord_uuid);
            let coord_state = coord_entry
                .and_then(|e| e.last_state.as_ref())
                .map(speaker::to_json);

            let member_list: Vec<Value> = members
                .iter()
                .map(|m| {
                    let m_uuid = m.uuid();
                    let m_entry = st.speakers.get(m_uuid);
                    let m_room = m_entry.map(|e| e.room_name.as_str()).unwrap_or(m_uuid);
                    let m_state = m_entry
                        .and_then(|e| e.last_state.as_ref())
                        .map(speaker::to_json);
                    json!({
                        "uuid":     m_uuid,
                        "roomName": m_room,
                        "state":    m_state,
                    })
                })
                .collect();

            json!({
                "coordinator": {
                    "uuid":     coord_uuid,
                    "roomName": coord_room,
                    "state":    coord_state,
                },
                "members": member_list,
            })
        })
        .collect();

    Json(json!(zones)).into_response()
}

/// GET /favorites — system-wide favorites list (from first available speaker).
pub async fn all_favorites(State(state): State<AppState>) -> Response {
    let speaker = {
        let st = state.read().await;
        st.speakers
            .values()
            .find(|e| e.available)
            .map(|e| e.speaker.clone())
    };
    match speaker {
        None => err_resp(StatusCode::SERVICE_UNAVAILABLE, "no speakers available"),
        Some(sp) => match content::list_favorites(&sp).await {
            Ok(items) => Json(content_view(&items)).into_response(),
            Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        },
    }
}

/// GET /playlists — all Sonos playlists (from first available speaker).
pub async fn all_playlists(State(state): State<AppState>) -> Response {
    let speaker = {
        let st = state.read().await;
        st.speakers
            .values()
            .find(|e| e.available)
            .map(|e| e.speaker.clone())
    };
    match speaker {
        None => err_resp(StatusCode::SERVICE_UNAVAILABLE, "no speakers available"),
        Some(sp) => match content::list_playlists(&sp).await {
            Ok(items) => Json(content_view(&items)).into_response(),
            Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        },
    }
}

/// GET /pauseall — pause every available speaker.
pub async fn pause_all(State(state): State<AppState>) -> Response {
    let speakers: Vec<(String, Speaker)> = {
        let st = state.read().await;
        st.speakers
            .values()
            .filter(|e| e.available)
            .map(|e| (e.room_name.clone(), e.speaker.clone()))
            .collect()
    };
    let mut errors: Vec<String> = Vec::new();
    for (room, sp) in speakers {
        if let Err(e) = sp.pause().await {
            warn!(room, error = %e, "pauseall: pause failed");
            errors.push(format!("{room}: {e}"));
        }
    }
    if errors.is_empty() {
        ok()
    } else {
        Json(json!({"status": "partial", "errors": errors})).into_response()
    }
}

// ---------------------------------------------------------------------------
// Room state
// ---------------------------------------------------------------------------

/// GET /:room/state — live state of a single speaker.
pub async fn room_state(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let (speaker, uuid, room_name) = {
        let st = state.read().await;
        match st.find_by_room(&room) {
            None => return not_found("room"),
            Some(e) if !e.available => return offline(),
            Some(e) => (e.speaker.clone(), e.uuid.clone(), e.room_name.clone()),
        }
    };

    match speaker::poll(&speaker).await {
        Ok(s) => {
            let mut obj = speaker::to_json(&s);
            obj["uuid"] = json!(uuid);
            obj["roomName"] = json!(room_name);
            Json(obj).into_response()
        }
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

/// GET /:room/favorites — favorites list (room-scoped, same system-wide data).
pub async fn room_favorites(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match content::list_favorites(&sp).await {
        Ok(items) => Json(content_view(&items)).into_response(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

/// GET /:room/playlists — Sonos playlists (room-scoped).
pub async fn room_playlists(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match content::list_playlists(&sp).await {
        Ok(items) => Json(content_view(&items)).into_response(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

pub async fn play(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match sp.play().await {
        Ok(()) => ok(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

pub async fn pause(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match sp.pause().await {
        Ok(()) => ok(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

pub async fn playpause(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match sp.is_playing().await {
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        Ok(playing) => {
            let res = if playing {
                sp.pause().await
            } else {
                sp.play().await
            };
            match res {
                Ok(()) => Json(json!({"status": "success", "playing": !playing})).into_response(),
                Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
            }
        }
    }
}

pub async fn stop(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match sp.stop().await {
        Ok(()) => ok(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

pub async fn next(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match sp.next().await {
        Ok(()) => ok(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

pub async fn previous(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match sp.previous().await {
        Ok(()) => ok(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

// ---------------------------------------------------------------------------
// Volume
// ---------------------------------------------------------------------------

pub async fn volume(
    Path((room, level)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let result = if let Some(n) = level.strip_prefix('+') {
        match n.parse::<i16>() {
            Err(_) => return bad_req("volume adjustment must be an integer"),
            Ok(adj) => sp.set_volume_relative(adj).await.map(|_| ()),
        }
    } else if let Some(n) = level.strip_prefix('-') {
        match n.parse::<i16>() {
            Err(_) => return bad_req("volume adjustment must be an integer"),
            Ok(adj) => sp.set_volume_relative(-adj).await.map(|_| ()),
        }
    } else {
        match level.parse::<u16>() {
            Err(_) => return bad_req("volume must be 0-100 or ±n"),
            Ok(vol) => sp.set_volume(vol.min(100)).await,
        }
    };
    match result {
        Ok(()) => ok(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

pub async fn mute(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match sp.set_mute(true).await {
        Ok(()) => ok(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

pub async fn unmute(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match sp.set_mute(false).await {
        Ok(()) => ok(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

pub async fn togglemute(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match sp.mute().await {
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        Ok(muted) => match sp.set_mute(!muted).await {
            Ok(()) => Json(json!({"status": "success", "muted": !muted})).into_response(),
            Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        },
    }
}

// ---------------------------------------------------------------------------
// EQ
// ---------------------------------------------------------------------------

pub async fn loudness(
    Path((room, mode)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let value = match mode.as_str() {
        "on" => true,
        "off" => false,
        "toggle" => match sp.loudness().await {
            Ok(v) => !v,
            Err(e) => return err_resp(StatusCode::BAD_GATEWAY, e),
        },
        _ => return bad_req("loudness state must be on/off/toggle"),
    };
    match sp.set_loudness(value).await {
        Ok(()) => Json(json!({"status": "success", "loudness": value})).into_response(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

pub async fn bass(
    Path((room, level)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match level.parse::<i8>() {
        Err(_) => bad_req("bass must be -10..10"),
        Ok(v) => match sp.set_bass(v.clamp(-10, 10)).await {
            Ok(()) => ok(),
            Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        },
    }
}

pub async fn treble(
    Path((room, level)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match level.parse::<i8>() {
        Err(_) => bad_req("treble must be -10..10"),
        Ok(v) => match sp.set_treble(v.clamp(-10, 10)).await {
            Ok(()) => ok(),
            Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        },
    }
}

// ---------------------------------------------------------------------------
// Play modes
// ---------------------------------------------------------------------------

pub async fn shuffle(
    Path((room, mode)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let value = match mode.as_str() {
        "on" => true,
        "off" => false,
        "toggle" => match sp.shuffle().await {
            Ok(v) => !v,
            Err(e) => return err_resp(StatusCode::BAD_GATEWAY, e),
        },
        _ => return bad_req("shuffle state must be on/off/toggle"),
    };
    match sp.set_shuffle(value).await {
        Ok(()) => Json(json!({"status": "success", "shuffle": value})).into_response(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

pub async fn repeat(
    Path((room, mode)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let value = match mode.as_str() {
        "on" | "all" => RepeatMode::All,
        "one" => RepeatMode::One,
        "off" | "none" => RepeatMode::None,
        "toggle" => match sp.repeat_mode().await {
            Ok(RepeatMode::None) => RepeatMode::All,
            Ok(RepeatMode::All) => RepeatMode::One,
            Ok(RepeatMode::One) => RepeatMode::None,
            Err(e) => return err_resp(StatusCode::BAD_GATEWAY, e),
        },
        _ => return bad_req("repeat state must be on/off/one/toggle"),
    };
    let label = repeat_to_str(&value).to_string();
    match sp.set_repeat_mode(value).await {
        Ok(()) => Json(json!({"status": "success", "repeat": label})).into_response(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

pub async fn crossfade(
    Path((room, mode)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let value = match mode.as_str() {
        "on" => true,
        "off" => false,
        "toggle" => match sp.crossfade().await {
            Ok(v) => !v,
            Err(e) => return err_resp(StatusCode::BAD_GATEWAY, e),
        },
        _ => return bad_req("crossfade state must be on/off/toggle"),
    };
    match sp.set_crossfade(value).await {
        Ok(()) => Json(json!({"status": "success", "crossfade": value})).into_response(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

// ---------------------------------------------------------------------------
// Seek
// ---------------------------------------------------------------------------

pub async fn seek(
    Path((room, secs)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match secs.parse::<u32>() {
        Err(_) => bad_req("position must be seconds (integer)"),
        Ok(s) => match sp.skip_to(s).await {
            Ok(()) => ok(),
            Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        },
    }
}

pub async fn seekby(
    Path((room, secs)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match secs.parse::<i32>() {
        Err(_) => bad_req("offset must be an integer (positive or negative seconds)"),
        Ok(s) => match sp.skip_by(s).await {
            Ok(()) => ok(),
            Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        },
    }
}

pub async fn trackseek(
    Path((room, index)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match index.parse::<u32>() {
        Err(_) => bad_req("track index must be an integer"),
        Ok(i) => match sp.seek_track(i).await {
            Ok(()) => ok(),
            Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        },
    }
}

// ---------------------------------------------------------------------------
// Grouping
// ---------------------------------------------------------------------------

/// GET /:room/join/:target — move :room into :target's group.
pub async fn join(
    Path((room, target)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let (speaker, target_room) = {
        let st = state.read().await;
        let sp = match st.find_by_room(&room) {
            None => return not_found("room"),
            Some(e) if !e.available => return offline(),
            Some(e) => e.speaker.clone(),
        };
        let target_room = match st.find_by_room(&target) {
            None => return not_found("target room"),
            Some(e) => e.room_name.clone(),
        };
        (sp, target_room)
    };
    match speaker.join(&target_room).await {
        Ok(_) => ok(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

/// GET /:room/leave — remove :room from its current group.
pub async fn leave(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match sp.leave().await {
        Ok(()) => ok(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

// ---------------------------------------------------------------------------
// Queue
// ---------------------------------------------------------------------------

/// GET /:room/queue — current queue contents.
pub async fn queue(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match sp.queue().await {
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        Ok(tracks) => {
            let items: Vec<Value> = tracks
                .iter()
                .map(|t| {
                    json!({
                        "title":   t.title(),
                        "artist":  t.creator(),
                        "album":   t.album(),
                        "duration": t.duration(),
                        "uri":     t.uri(),
                    })
                })
                .collect();
            Json(json!(items)).into_response()
        }
    }
}

/// GET /:room/clearqueue — clear the queue.
pub async fn clearqueue(Path(room): Path<String>, State(state): State<AppState>) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match sp.clear_queue().await {
        Ok(()) => ok(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

/// GET /:room/queue/remove/:index — remove a track from the queue by 1-based track number.
pub async fn queue_remove(
    Path((room, index)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match index.parse::<u32>() {
        Err(_) => bad_req("track index must be an integer"),
        Ok(i) => match sp.remove_track(i).await {
            Ok(()) => ok(),
            Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        },
    }
}

/// GET /:room/queue/adduri/:uri — add a URI to the end of the queue.
pub async fn queue_add(
    Path((room, uri)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let uri = uri.replace('&', "&amp;");
    match sp.queue_end(&uri, "").await {
        Ok(()) => ok(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

/// GET /:room/queue/addnexturi/:uri — add a URI to play next in the queue.
pub async fn queue_add_next(
    Path((room, uri)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let uri = uri.replace('&', "&amp;");
    match sp.queue_next(&uri, "").await {
        Ok(()) => ok(),
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
    }
}

// ---------------------------------------------------------------------------
// Favorites & playlists playback
// ---------------------------------------------------------------------------

/// GET /:room/favorite/:index — play a Sonos favorite by 0-based index from /favorites.
pub async fn play_favorite(
    Path((room, index)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let idx: usize = match index.parse() {
        Ok(n) => n,
        Err(_) => return bad_req("favorite index must be an integer (see /favorites for list)"),
    };
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match content::get_favorite_by_index(&sp, idx).await {
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        Ok(None) => not_found("favorite at that index"),
        Ok(Some((uri, metadata))) => match sp.set_transport_uri(&uri, &metadata).await {
            Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
            Ok(()) => match sp.play().await {
                Ok(()) => ok(),
                Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
            },
        },
    }
}

/// GET /:room/playlist/:index — play a Sonos playlist by 0-based index from /playlists.
pub async fn play_playlist(
    Path((room, index)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let idx: usize = match index.parse() {
        Ok(n) => n,
        Err(_) => return bad_req("playlist index must be an integer (see /playlists for list)"),
    };
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match content::get_playlist_by_index(&sp, idx).await {
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        Ok(None) => not_found("playlist at that index"),
        Ok(Some((uri, metadata))) => match sp.set_transport_uri(&uri, &metadata).await {
            Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
            Ok(()) => match sp.play().await {
                Ok(()) => ok(),
                Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
            },
        },
    }
}

/// GET /:room/playuri/:uri — play a raw URI (URI must be URL-encoded by caller).
pub async fn play_uri(
    Path((room, uri)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    let sp = match get_speaker(&state, &room).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let uri = uri.replace('&', "&amp;");
    match sp.set_transport_uri(&uri, "").await {
        Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        Ok(()) => match sp.play().await {
            Ok(()) => ok(),
            Err(e) => err_resp(StatusCode::BAD_GATEWAY, e),
        },
    }
}

// ---------------------------------------------------------------------------
// GENA NOTIFY callback
// ---------------------------------------------------------------------------

/// ANY /sonos/callback/:uuid/:service — receive UPnP GENA NOTIFY from Sonos.
///
/// Sonos sends `HTTP NOTIFY` (a non-standard HTTP method) to this endpoint.
/// We parse the body, build a `NotifyEvent`, and forward it to the bridge via
/// the mpsc channel.  We always return 200 OK so Sonos doesn't retry.
pub async fn sonos_notify(
    Path((uuid, service)): Path<(String, String)>,
    Extension(event_tx): Extension<mpsc::Sender<(String, NotifyEvent)>>,
    body: String,
) -> StatusCode {
    let event = match service.as_str() {
        "avt" => events::parse_avt(&body).map(NotifyEvent::Avt),
        "rc" => events::parse_rc(&body).map(NotifyEvent::Rc),
        other => {
            warn!(uuid, service = other, "Unknown NOTIFY service");
            return StatusCode::OK;
        }
    };

    match event {
        Some(ev) => {
            debug!(uuid, service, "GENA NOTIFY received");
            let _ = event_tx.try_send((uuid, ev)); // drop if bridge is busy
        }
        None => {
            // Could be a subscription-confirmed NOTIFY with no LastChange — ignore.
            debug!(uuid, service, "NOTIFY body had no parseable LastChange");
        }
    }

    StatusCode::OK
}
