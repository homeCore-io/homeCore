//! Somewhere to put a picture.
//!
//! Six client features store an *address* and have had nowhere to put the file
//! behind it: a card's picture, a page background, the image widget, a skin's
//! custom font, custom icon sets, and the floor plan's own image modes. All of
//! them keep a string and resolve it in the browser, so an endpoint that hands
//! back `/assets/…` drops into every one of them without changing a stored
//! shape.
//!
//! ## Why the read is public
//!
//! A browser sends no `Authorization` header when it loads an `<img>`, a CSS
//! background or a font. `require_auth` takes a Bearer token or a whitelisted
//! source IP and nothing else, so an authenticated GET here would work on the
//! LAN through port 8080 and return 401 through the front door. That is not
//! hypothetical — it is why album art is currently broken at :3001.
//!
//! So [`get_asset`] lives in the public router, and **the id is what stands in
//! for the token**: a 256-bit content hash the caller cannot choose, guess or
//! enumerate. Writes, the listing and deletion all stay authenticated; the
//! listing especially, because that is the only thing that would turn an
//! unguessable id into a guessable one.
//!
//! The trade is stated rather than hidden: anyone holding the URL can read the
//! bytes. These are wallpapers, fonts and floor plans.

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::auth_middleware::{DashboardsRead, DashboardsWrite};
use crate::AppState;

/// Large enough for a floor plan render or a variable font, small enough that
/// filling the disk takes deliberate effort. The house stops when the disk is
/// full, so "it is my own house" is not a reason to accept an unbounded body.
pub const MAX_ASSET_BYTES: usize = 16 * 1024 * 1024;

/// What may be stored, declared by the uploader and never sniffed.
///
/// SVG is on the list because a floor plan is the reason this exists — see the
/// response headers in [`get_asset`] for what makes that safe.
const ALLOWED: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/avif",
    "image/svg+xml",
    "font/ttf",
    "font/otf",
    "font/woff",
    "font/woff2",
];

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    /// The name it had on the way in, shown by the manager. Descriptive only —
    /// it never becomes part of a path.
    #[serde(default)]
    pub name: Option<String>,
    /// What it arrived with, so a floor plan's textures prune together.
    #[serde(default)]
    pub group: Option<String>,
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

/// `POST /assets` — store bytes, get back an address.
///
/// The body is the file itself rather than a multipart form. Every caller
/// already holds the bytes: the picker has read a `File`, and the `.sh3d`
/// import has just unzipped forty textures in memory. Multipart would make
/// both of them wrap what they already have.
///
/// Idempotent by content, so the import can hand over every texture and let
/// core work out which are new.
pub async fn upload_asset(
    State(s): State<AppState>,
    _: DashboardsWrite,
    Query(q): Query<UploadQuery>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        // `image/png; charset=binary` is still image/png.
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase())
        .unwrap_or_default();

    if !ALLOWED.contains(&content_type.as_str()) {
        return err(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "not a picture or a font this stores",
        );
    }
    if body.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty body");
    }
    if body.len() > MAX_ASSET_BYTES {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "over the size limit");
    }

    let name = q.name.unwrap_or_else(|| "untitled".to_string());
    let group = q.group;
    let store = s.store.assets();

    // Writing bytes is the one blocking thing here.
    let result = tokio::task::spawn_blocking(move || {
        store.put(&body, &content_type, &name, group.as_deref())
    })
    .await;

    match result {
        Ok(Ok(record)) => (StatusCode::CREATED, Json(json!(record))).into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `GET /assets/{id}` — **public**, see the module note.
pub async fn get_asset(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    let store = s.store.assets();
    let meta = match store.get_meta(&id) {
        Ok(Some(m)) => m,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such asset"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let id_for_read = id.clone();
    let store2 = s.store.assets();
    let bytes = match tokio::task::spawn_blocking(move || store2.read(&id_for_read)).await {
        Ok(Ok(Some(b))) => b,
        Ok(Ok(None)) => return err(StatusCode::NOT_FOUND, "no such asset"),
        Ok(Err(e)) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, meta.content_type.clone()),
            // An SVG in an <img> cannot run script, but navigating straight to
            // one can. This is what makes SVG safe to accept, and a floor plan
            // is the reason SVG is accepted at all.
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; sandbox".to_string(),
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            // The address *is* the hash, so these bytes can never change.
            (
                header::CACHE_CONTROL,
                "public, max-age=31536000, immutable".to_string(),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// `GET /assets` — what is stored and what it costs.
///
/// Authenticated, and the reason is not tidiness: this is the one call that
/// would turn an unguessable id into a guessable one.
pub async fn list_assets(State(s): State<AppState>, _: DashboardsRead) -> Response {
    let store = s.store.assets();
    match tokio::task::spawn_blocking(move || {
        let items = store.list()?;
        let total: u64 = items.iter().map(|r| r.size).sum();
        Ok::<_, anyhow::Error>((items, total))
    })
    .await
    {
        Ok(Ok((items, total))) => Json(json!({
            "assets": items,
            "total_bytes": total,
            "max_bytes_per_asset": MAX_ASSET_BYTES,
        }))
        .into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `DELETE /assets/{id}`.
///
/// Nothing reference-counts. A page still pointing at a deleted asset shows its
/// empty state, exactly as it does today when a URL goes stale — and that is
/// the honest trade against auto-deletion, which is the half that loses data.
pub async fn delete_asset(
    State(s): State<AppState>,
    _: DashboardsWrite,
    Path(id): Path<String>,
) -> Response {
    let store = s.store.assets();
    match tokio::task::spawn_blocking(move || store.delete(&id)).await {
        Ok(Ok(true)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(false)) => err(StatusCode::NOT_FOUND, "no such asset"),
        Ok(Err(_)) => err(StatusCode::BAD_REQUEST, "not an asset id"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `DELETE /assets/group/{group}` — everything that arrived together.
///
/// One floor plan import is one group, so removing the plan does not mean
/// hunting forty textures by hand.
pub async fn delete_asset_group(
    State(s): State<AppState>,
    _: DashboardsWrite,
    Path(group): Path<String>,
) -> Response {
    let store = s.store.assets();
    match tokio::task::spawn_blocking(move || store.delete_group(&group)).await {
        Ok(Ok(n)) => Json(json!({ "deleted": n })).into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppState, AppStateParams};
    use axum::body::Body;
    use axum::http::Request;
    use hc_auth::user::Role;
    use hc_auth::JwtService;
    use hc_core::EventBus;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// A 1x1 PNG, so the bytes are a real file of the type they claim.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89,
    ];

    const SECRET: &[u8] = b"asset-test-secret-32-bytes-minimum!";

    async fn app_and_token() -> (axum::Router, String) {
        let base = std::env::temp_dir().join(format!("hc_assets_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let store = hc_state::StateStore::open(
            base.join("state.redb").to_str().unwrap(),
            base.join("history.db").to_str().unwrap(),
        )
        .await
        .unwrap();
        // A real user record, because `require_auth` re-checks the token's
        // generation counter against the store — a token for a uid that does
        // not exist is rejected, which is the behaviour, not an obstacle.
        let user = hc_auth::user::User {
            id: uuid::Uuid::new_v4(),
            username: "tester".into(),
            password_hash: String::new(),
            role: Role::Admin,
            created_at: chrono::Utc::now(),
            token_version: 0,
        };
        store.create_user(&user).await.unwrap();

        let jwt = JwtService::new_hs256(SECRET, 24);
        let token = jwt
            .issue(&user.id.to_string(), &user.username, Role::Admin, 0)
            .unwrap();
        let state = AppState::new(AppStateParams::new(store, EventBus::new(16), jwt));
        (crate::router(state, None), token)
    }

    fn upload(token: &str, ctype: &str, name: &str, bytes: &[u8]) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!("/api/v1/assets?name={name}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", ctype)
            .body(Body::from(bytes.to_vec()))
            .unwrap()
    }

    async fn json_of(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn a_stored_asset_reads_back_without_a_token() {
        // The property the whole design rests on. A browser sends no
        // Authorization header on an <img>, so if this ever needs one, every
        // wallpaper in the house breaks at :3001.
        let (app, token) = app_and_token().await;

        let resp = app
            .clone()
            .oneshot(upload(&token, "image/png", "wall.png", PNG))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = json_of(resp).await;
        let id = body["id"].as_str().unwrap().to_string();
        assert_eq!(
            id.len(),
            64,
            "the id is a sha256, which is what makes the
            public read defensible"
        );
        assert_eq!(body["size"], PNG.len());
        assert_eq!(body["name"], "wall.png");

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/assets/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "no Authorization header");

        let headers = resp.headers().clone();
        assert_eq!(headers[header::CONTENT_TYPE], "image/png");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        // What makes accepting SVG safe: an SVG opened directly cannot run.
        assert!(headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("default-src 'none'"));
        // Safe only because the address is the hash of the bytes.
        assert!(headers[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable"));

        let got = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&got[..], PNG, "the bytes come back byte for byte");
    }

    #[tokio::test]
    async fn the_same_file_twice_is_one_asset() {
        // What lets the .sh3d import hand over every texture and let core work
        // out which are new.
        let (app, token) = app_and_token().await;
        let a = json_of(
            app.clone()
                .oneshot(upload(&token, "image/png", "one.png", PNG))
                .await
                .unwrap(),
        )
        .await;
        let b = json_of(
            app.clone()
                .oneshot(upload(&token, "image/png", "two.png", PNG))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(a["id"], b["id"]);
    }

    #[tokio::test]
    async fn writing_and_listing_need_a_token_even_though_reading_does_not() {
        let (app, _token) = app_and_token().await;

        // The listing especially: it is the one call that would turn an
        // unguessable id into a guessable one.
        for (method, uri) in [("GET", "/api/v1/assets"), ("POST", "/api/v1/assets")] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("Content-Type", "image/png")
                        .body(Body::from(PNG.to_vec()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri} should need a token"
            );
        }
    }

    #[tokio::test]
    async fn only_pictures_and_fonts() {
        let (app, token) = app_and_token().await;
        for ctype in ["text/html", "application/zip", "application/javascript", ""] {
            let resp = app
                .clone()
                .oneshot(upload(&token, ctype, "x", PNG))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "accepted {ctype:?}"
            );
        }
        // A parameterised type is still the type.
        let resp = app
            .clone()
            .oneshot(upload(&token, "image/png; charset=binary", "x", PNG))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn an_empty_body_is_not_an_asset() {
        let (app, token) = app_and_token().await;
        let resp = app
            .oneshot(upload(&token, "image/png", "x", b""))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_id_that_is_not_an_id_is_a_404_and_nothing_else() {
        // The public read takes its path segment straight from the URL, so this
        // is the one that matters: no traversal, no 500, no disclosure.
        let (app, _t) = app_and_token().await;
        for bad in [
            "..%2f..%2fetc%2fpasswd",
            "not-a-hash",
            "AAAA5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824cafe",
            "2cf24dba",
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/api/v1/assets/{bad}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "for {bad:?}");
        }
    }

    #[tokio::test]
    async fn deleting_needs_a_token_and_then_the_read_stops_working() {
        let (app, token) = app_and_token().await;
        let a = json_of(
            app.clone()
                .oneshot(upload(&token, "image/png", "gone.png", PNG))
                .await
                .unwrap(),
        )
        .await;
        let id = a["id"].as_str().unwrap().to_string();

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/assets/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/assets/{id}"))
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/assets/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
