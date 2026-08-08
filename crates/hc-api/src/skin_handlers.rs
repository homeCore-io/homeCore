//! `/skins` — user-defined skins, stored as seeds.
//!
//! Step 3 of `clients/hc-web/theme-editor-plan.md`. See `hc_types::skin` for
//! why core stores seeds rather than resolved tokens, and why it validates
//! structure but never judges whether a skin is legible.
//!
//! Its own module rather than more of `handlers.rs`, which is past 9,000 lines.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use hc_types::skin::Skin;
use serde_json::json;

use crate::auth_middleware::{SkinsRead, SkinsWrite};
use crate::AppState;

fn unavailable() -> axum::response::Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "error": "skins unavailable" })),
    )
        .into_response()
}

/// Every skin the house has defined.
///
/// Not the built-in four: those are compiled into the client and are the floor
/// a data skin layers on top of. A house that has never defined one gets an
/// empty list, which is the correct answer rather than an error.
pub async fn list_skins(State(s): State<AppState>, _user: SkinsRead) -> impl IntoResponse {
    let Some(handle) = &s.skins else {
        return unavailable();
    };
    let data = handle.read().await;
    let mut skins = data.skins.clone();
    skins.sort_by(|a, b| a.name.cmp(&b.name));
    Json(skins).into_response()
}

pub async fn get_skin(
    State(s): State<AppState>,
    _user: SkinsRead,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(handle) = &s.skins else {
        return unavailable();
    };
    let data = handle.read().await;
    match data.skins.iter().find(|skin| skin.id == id) {
        Some(skin) => Json(skin.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no skin '{id}'") })),
        )
            .into_response(),
    }
}

pub async fn create_skin(
    State(s): State<AppState>,
    _user: SkinsWrite,
    Json(skin): Json<Skin>,
) -> impl IntoResponse {
    let (Some(handle), Some(store)) = (&s.skins, &s.skin_store) else {
        return unavailable();
    };
    if let Err(e) = skin.validate() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }

    let mut data = handle.write().await;
    if data.skins.iter().any(|existing| existing.id == skin.id) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("skin '{}' already exists", skin.id) })),
        )
            .into_response();
    }
    data.skins.push(skin.clone());
    if let Err(e) = store.save(&data) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("could not save skins: {e}") })),
        )
            .into_response();
    }
    (StatusCode::CREATED, Json(skin)).into_response()
}

pub async fn update_skin(
    State(s): State<AppState>,
    _user: SkinsWrite,
    Path(id): Path<String>,
    Json(mut skin): Json<Skin>,
) -> impl IntoResponse {
    let (Some(handle), Some(store)) = (&s.skins, &s.skin_store) else {
        return unavailable();
    };
    // The path wins. A body naming a different id is a client bug, and honouring
    // it would let a PUT to one skin silently rewrite another.
    skin.id = id.clone();
    if let Err(e) = skin.validate() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }

    let mut data = handle.write().await;
    let Some(slot) = data.skins.iter_mut().find(|existing| existing.id == id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no skin '{id}'") })),
        )
            .into_response();
    };
    *slot = skin.clone();
    if let Err(e) = store.save(&data) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("could not save skins: {e}") })),
        )
            .into_response();
    }
    Json(skin).into_response()
}

pub async fn delete_skin(
    State(s): State<AppState>,
    _user: SkinsWrite,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let (Some(handle), Some(store)) = (&s.skins, &s.skin_store) else {
        return unavailable();
    };
    let mut data = handle.write().await;
    let before = data.skins.len();
    data.skins.retain(|skin| skin.id != id);
    if data.skins.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no skin '{id}'") })),
        )
            .into_response();
    }
    if let Err(e) = store.save(&data) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("could not save skins: {e}") })),
        )
            .into_response();
    }
    // Deleting the skin a wall panel is showing is allowed: the client falls
    // back to the built-in it was based on. Refusing here would mean a skin
    // could only be removed by first walking round the house.
    StatusCode::NO_CONTENT.into_response()
}
