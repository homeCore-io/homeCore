//! Displays and PINs (H3, H5).
//!
//! **A display** — a kitchen panel, a hall tablet — is its own identity, not a
//! person's (D7). An admin makes one here and gets its key once; the key is an
//! API key marked `display`, holding [`display_scopes`]: read the house,
//! command devices, run scenes. Never an admin, never an author.
//!
//! **A PIN** lets a person at a display use the full app as themselves for a
//! few minutes (N9). They set it themselves, with their password; it is
//! unique in the house, because the PIN alone says who is at the display.
//! Only a display may exchange a PIN for a session, and failures are counted
//! per display, so guessing is slow wherever it is tried.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use hc_auth::user::display_scopes;
use hc_auth::{api_key, hash_pin, verify_password, Actor, Claims, User, UserInfo};
use hc_state::ApiKeyRecord;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::{audit, auth_middleware::AuthUser, AppState};

/// How long a PIN's session lasts.
pub const PIN_SESSION_SECS: u64 = 5 * 60;
/// Wrong PINs a display may try in [`PIN_WINDOW`] before it must wait.
const PIN_MAX_FAILURES: usize = 5;
const PIN_WINDOW: Duration = Duration::from_secs(15 * 60);

fn err(status: StatusCode, msg: &str) -> axum::response::Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

fn summary(r: &ApiKeyRecord) -> serde_json::Value {
    json!({
        "id": r.id,
        "name": r.label,
        "created_at": r.created_at,
        "last_used_at": r.last_used_at,
        "revoked": r.revoked_at.is_some(),
    })
}

/// The display a request comes from, if it comes from one.
async fn display_of(s: &AppState, claims: &Claims) -> Option<ApiKeyRecord> {
    let Some(Actor::ApiKey { id, .. }) = claims.actor else {
        return None;
    };
    match s.store.get_api_key_by_id(id).await {
        Ok(Some(r)) if r.display && r.is_usable(Utc::now()) => Some(r),
        _ => None,
    }
}

// ---------- displays (admins) ----------

/// `GET /displays`
pub async fn list_displays(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
) -> impl IntoResponse {
    if !claims.is_admin() {
        return err(StatusCode::FORBIDDEN, "only an admin manages displays");
    }
    match s.store.list_api_keys().await {
        Ok(keys) => {
            let mut out: Vec<&ApiKeyRecord> = keys.iter().filter(|k| k.display).collect();
            out.sort_by(|a, b| a.label.cmp(&b.label));
            Json(out.into_iter().map(summary).collect::<Vec<_>>()).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct CreateDisplayBody {
    pub name: String,
}

/// `POST /displays` — the key is in the answer, and never again.
pub async fn create_display(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<CreateDisplayBody>,
) -> impl IntoResponse {
    if !claims.is_admin() {
        return err(StatusCode::FORBIDDEN, "only an admin manages displays");
    }
    let name = body.name.trim();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, "give the display a name");
    }
    // Made by an admin, owned by nobody's account in particular: the owner
    // is who made it, for the record.
    let owner_uid = claims.actor().effective_uid().unwrap_or_default();

    let mut tries = 0;
    let (key, record) = loop {
        let candidate = match api_key::generate() {
            Ok(k) => k,
            Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "key generation failed"),
        };
        match s
            .store
            .api_key_prefix_exists(&candidate.lookup_prefix)
            .await
        {
            Ok(false) => {
                let record = ApiKeyRecord {
                    id: Uuid::new_v4(),
                    prefix: candidate.lookup_prefix.clone(),
                    hash: candidate.hash.clone(),
                    owner_uid,
                    scopes: display_scopes(),
                    label: name.to_string(),
                    created_at: Utc::now(),
                    last_used_at: None,
                    expires_at: None,
                    allowed_cidrs: Vec::new(),
                    revoked_at: None,
                    display: true,
                };
                break (candidate, record);
            }
            Ok(true) if tries < 3 => tries += 1,
            Ok(true) => return err(StatusCode::INTERNAL_SERVER_ERROR, "key generation failed"),
            Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable"),
        }
    };
    if s.store.create_api_key(&record).await.is_err() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable");
    }
    let mut e = audit::entry_from_claims(&claims, "display.created")
        .with_target("display", record.id.to_string());
    e.detail = json!({ "name": record.label });
    audit::emit(&s, e).await;

    let mut out = summary(&record);
    out["token"] = json!(key.full_token);
    (StatusCode::CREATED, Json(out)).into_response()
}

#[derive(Deserialize)]
pub struct RenameDisplayBody {
    pub name: String,
}

/// `PATCH /displays/{id}` — a new name; the key, and the tablet using it,
/// carry on as they were.
pub async fn rename_display(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<RenameDisplayBody>,
) -> impl IntoResponse {
    if !claims.is_admin() {
        return err(StatusCode::FORBIDDEN, "only an admin manages displays");
    }
    let name = body.name.trim();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, "give the display a name");
    }
    let mut record = match s.store.get_api_key_by_id(id).await {
        Ok(Some(r)) if r.display && r.revoked_at.is_none() => r,
        Ok(_) => return err(StatusCode::NOT_FOUND, "no such display"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let previous = std::mem::replace(&mut record.label, name.to_string());
    if let Err(e) = s.store.update_api_key(&record).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    if previous != record.label {
        let mut e = audit::entry_from_claims(&claims, "display.renamed")
            .with_target("display", id.to_string());
        e.detail = json!({ "name": previous, "renamed_to": record.label });
        audit::emit(&s, e).await;
    }
    Json(summary(&record)).into_response()
}

/// `DELETE /displays/{id}` — the tablet is signed out on its next request.
pub async fn delete_display(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    if !claims.is_admin() {
        return err(StatusCode::FORBIDDEN, "only an admin manages displays");
    }
    match s.store.get_api_key_by_id(id).await {
        Ok(Some(r)) if r.display => {}
        Ok(_) => return err(StatusCode::NOT_FOUND, "no such display"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
    if let Err(e) = s.store.revoke_api_key(id).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    let e =
        audit::entry_from_claims(&claims, "display.removed").with_target("display", id.to_string());
    audit::emit(&s, e).await;
    StatusCode::NO_CONTENT.into_response()
}

// ---------- PINs (H5) ----------

/// Whether a PIN is one homeCore accepts: 4 to 8 digits.
pub fn valid_pin(pin: &str) -> bool {
    (4..=8).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit())
}

/// The person whose PIN this is, if anyone's. Every stored PIN is checked,
/// so how long it takes doesn't say which person, or whether any, matched.
async fn whose_pin(users: Vec<User>, pin: String, except: Option<Uuid>) -> Option<User> {
    tokio::task::spawn_blocking(move || {
        let mut found = None;
        for u in users {
            if Some(u.id) == except {
                continue;
            }
            if let Some(hash) = &u.pin_hash {
                if verify_password(&pin, hash) && found.is_none() {
                    found = Some(u);
                }
            }
        }
        found
    })
    .await
    .ok()
    .flatten()
}

fn failures() -> &'static Mutex<HashMap<Uuid, Vec<Instant>>> {
    static F: OnceLock<Mutex<HashMap<Uuid, Vec<Instant>>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Seconds this display must wait before trying again, if it must.
fn locked_out(display: Uuid, now: Instant) -> Option<u64> {
    let mut map = failures().lock().ok()?;
    let list = map.entry(display).or_default();
    list.retain(|t| now.duration_since(*t) < PIN_WINDOW);
    if list.len() < PIN_MAX_FAILURES {
        return None;
    }
    let oldest = list.first().copied()?;
    Some((PIN_WINDOW - now.duration_since(oldest)).as_secs().max(1))
}

fn record_failure(display: Uuid, now: Instant) {
    if let Ok(mut map) = failures().lock() {
        map.entry(display).or_default().push(now);
    }
}

fn forget_failures(display: Uuid) {
    if let Ok(mut map) = failures().lock() {
        map.remove(&display);
    }
}

#[derive(Deserialize)]
pub struct PinSessionBody {
    pub pin: String,
}

/// `POST /auth/pin-session` — from a display only: the person whose PIN this
/// is, signed in for [`PIN_SESSION_SECS`], with no refresh token.
pub async fn pin_session(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<PinSessionBody>,
) -> impl IntoResponse {
    let Some(display) = display_of(&s, &claims).await else {
        return err(
            StatusCode::FORBIDDEN,
            "only a display can be unlocked with a PIN",
        );
    };
    let now = Instant::now();
    if let Some(wait) = locked_out(display.id, now) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", wait.to_string())],
            Json(json!({ "error": "too many wrong PINs — try again later", "retry_after_seconds": wait })),
        )
            .into_response();
    }
    let users = match s.store.list_users().await {
        Ok(u) => u,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let found = if valid_pin(&body.pin) {
        whose_pin(users, body.pin, None).await
    } else {
        None
    };
    let Some(user) = found else {
        record_failure(display.id, now);
        let mut e = audit::entry_from_claims(&claims, "auth.pin_failed")
            .with_target("display", display.id.to_string());
        e.result = hc_state::AuditResult::Denied;
        audit::emit(&s, e).await;
        return err(StatusCode::UNAUTHORIZED, "that PIN isn't right");
    };
    forget_failures(display.id);

    let token = match s.jwt.issue_for(
        &user.id.to_string(),
        &user.username,
        user.role,
        user.token_version,
        PIN_SESSION_SECS,
    ) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mut e = audit::entry_from_claims(&claims, "auth.pin_session")
        .with_target("user", user.id.to_string());
    e.detail = json!({ "display": display.label, "username": user.username });
    audit::emit(&s, e).await;

    Json(json!({
        "token": token,
        "token_type": "Bearer",
        "expires_in": PIN_SESSION_SECS,
        "user": UserInfo::from(&user),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct SetPinBody {
    pub pin: String,
    /// The person's password: a PIN is a way in, so setting one needs the
    /// key to the front door.
    pub password: String,
}

async fn signed_in_user(
    s: &AppState,
    claims: &Claims,
) -> Result<User, Box<axum::response::Response>> {
    let Actor::User { uid, .. } = claims.actor() else {
        return Err(err(StatusCode::FORBIDDEN, "only a person has a PIN").into());
    };
    match s.store.get_user_by_id(uid).await {
        Ok(Some(u)) => Ok(u),
        Ok(None) => Err(err(StatusCode::NOT_FOUND, "user not found").into()),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into()),
    }
}

/// `PUT /auth/pin` — set your own PIN.
pub async fn set_pin(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Json(body): Json<SetPinBody>,
) -> impl IntoResponse {
    let mut user = match signed_in_user(&s, &claims).await {
        Ok(u) => u,
        Err(r) => return *r,
    };
    let (password, hash) = (body.password.clone(), user.password_hash.clone());
    let right = tokio::task::spawn_blocking(move || verify_password(&password, &hash))
        .await
        .unwrap_or(false);
    if !right {
        return err(StatusCode::UNAUTHORIZED, "that password isn't right");
    }
    if !valid_pin(&body.pin) {
        return err(StatusCode::BAD_REQUEST, "a PIN is 4 to 8 digits");
    }
    let users = match s.store.list_users().await {
        Ok(u) => u,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    if whose_pin(users, body.pin.clone(), Some(user.id))
        .await
        .is_some()
    {
        // It has to say so: the PIN alone names the person at a display.
        return err(
            StatusCode::CONFLICT,
            "someone else uses that PIN — choose another",
        );
    }
    let pin = body.pin;
    match tokio::task::spawn_blocking(move || hash_pin(&pin)).await {
        Ok(Ok(h)) => user.pin_hash = Some(h),
        _ => return err(StatusCode::INTERNAL_SERVER_ERROR, "could not store the PIN"),
    }
    if let Err(e) = s.store.update_user(&user).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    let e =
        audit::entry_from_claims(&claims, "user.pin_set").with_target("user", user.id.to_string());
    audit::emit(&s, e).await;
    StatusCode::NO_CONTENT.into_response()
}

/// `DELETE /auth/pin` — no PIN: this person can't unlock a display.
pub async fn clear_pin(State(s): State<AppState>, AuthUser(claims): AuthUser) -> impl IntoResponse {
    let mut user = match signed_in_user(&s, &claims).await {
        Ok(u) => u,
        Err(r) => return *r,
    };
    user.pin_hash = None;
    if let Err(e) = s.store.update_user(&user).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    let e = audit::entry_from_claims(&claims, "user.pin_cleared")
        .with_target("user", user.id.to_string());
    audit::emit(&s, e).await;
    StatusCode::NO_CONTENT.into_response()
}

/// `DELETE /auth/users/{id}/pin` — an admin clears someone else's PIN, so a
/// person who has forgotten theirs can choose another on Me. An admin never
/// sets one: a PIN says who is at the display, so only its owner should know
/// it.
pub async fn clear_user_pin(
    State(s): State<AppState>,
    AuthUser(claims): AuthUser,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    if !claims.is_admin() {
        return err(StatusCode::FORBIDDEN, "admin role required");
    }
    let mut user = match s.store.get_user_by_id(id).await {
        Ok(Some(u)) => u,
        Ok(None) => return err(StatusCode::NOT_FOUND, "user not found"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    if user.pin_hash.is_none() {
        return StatusCode::NO_CONTENT.into_response();
    }
    user.pin_hash = None;
    if let Err(e) = s.store.update_user(&user).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    let mut e =
        audit::entry_from_claims(&claims, "user.pin_cleared").with_target("user", id.to_string());
    e.detail = json!({ "by_admin": true });
    audit::emit(&s, e).await;
    StatusCode::NO_CONTENT.into_response()
}

/// What `/auth/me` says for a display's key: the display, not its maker.
pub async fn me_as_display(s: &AppState, claims: &Claims) -> Option<serde_json::Value> {
    let d = display_of(s, claims).await?;
    Some(json!({
        "id": d.id,
        "username": d.label,
        "role": "display",
        "kind": "display",
        "scopes": d.scopes,
        "created_at": d.created_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_is_four_to_eight_digits() {
        assert!(valid_pin("1234"));
        assert!(valid_pin("12345678"));
        assert!(!valid_pin("123"));
        assert!(!valid_pin("123456789"));
        assert!(!valid_pin("12a4"));
        assert!(!valid_pin(""));
    }

    #[test]
    fn wrong_pins_lock_a_display_out_for_a_while() {
        let d = Uuid::new_v4();
        let t0 = Instant::now();
        for _ in 0..PIN_MAX_FAILURES {
            assert!(locked_out(d, t0).is_none());
            record_failure(d, t0);
        }
        let wait = locked_out(d, t0).expect("locked out");
        assert!(wait > 60);
        // Another display isn't.
        assert!(locked_out(Uuid::new_v4(), t0).is_none());
        // Once the window passes, it may try again.
        assert!(locked_out(d, t0 + PIN_WINDOW + Duration::from_secs(1)).is_none());
        forget_failures(d);
    }

    #[test]
    fn a_display_may_see_and_use_the_house_and_nothing_more() {
        let s = display_scopes();
        for want in [
            "devices:read",
            "devices:command",
            "scenes:run",
            "areas:read",
        ] {
            assert!(s.iter().any(|x| x == want), "{want}");
        }
        for never in [
            "users:write",
            "users:read",
            "api_keys:admin",
            "plugins:write",
            "scenes:write",
            "automations:write",
            "content:write",
            "areas:write",
            // Using a device is not changing it: a display can't rename,
            // move or delete one.
            "devices:write",
        ] {
            assert!(!s.iter().any(|x| x == never), "{never}");
        }
    }

    // ---- through the real router ----

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use tower::ServiceExt;

    struct House {
        app: axum::Router,
        admin: String,
        person: String,
    }

    async fn house() -> House {
        let base = std::env::temp_dir().join(format!("hc_api_displays_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let store = hc_state::StateStore::open(
            &base.join("state.redb").to_string_lossy(),
            &base.join("history.sqlite").to_string_lossy(),
        )
        .await
        .unwrap();
        let mut ids = Vec::new();
        for (name, role) in [("ada", hc_auth::Role::Admin), ("bo", hc_auth::Role::User)] {
            let u = User {
                id: Uuid::new_v4(),
                username: name.into(),
                // Light parameters: verification reads them from the hash, and the
                // password strength isn't what these tests are about.
                password_hash: hash_pin("correct horse").unwrap(),
                role,
                created_at: Utc::now(),
                token_version: 0,
                pin_hash: None,
            };
            store.create_user(&u).await.unwrap();
            ids.push(u);
        }
        let jwt = hc_auth::JwtService::new_hs256(b"display-test-secret-32-bytes-min!", 24);
        let token = |u: &User| {
            jwt.issue(&u.id.to_string(), &u.username, u.role, 0)
                .unwrap()
        };
        let (admin, person) = (token(&ids[0]), token(&ids[1]));
        let state = crate::AppState::new(crate::AppStateParams::new(
            store,
            hc_core::EventBus::new(16),
            jwt,
        ));
        House {
            app: crate::router(state, None),
            admin,
            person,
        }
    }

    async fn call(
        app: &axum::Router,
        method: &str,
        path: &str,
        token: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(format!("/api/v1{path}"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(std::net::SocketAddr::from((
                [10, 9, 8, 7],
                1234,
            ))));
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn a_display_is_its_own_identity_and_never_an_admin() {
        let h = house().await;
        let (st, _) = call(&h.app, "POST", "/displays", &h.person, json!({"name": "x"})).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "only an admin makes displays");

        let (st, made) = call(
            &h.app,
            "POST",
            "/displays",
            &h.admin,
            json!({"name": "Kitchen"}),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let key = made["token"].as_str().unwrap().to_string();

        let (st, me) = call(&h.app, "GET", "/auth/me", &key, json!(null)).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(me["role"], "display");
        assert_eq!(me["username"], "Kitchen");

        // Reads the house; may not touch users, keys or displays.
        assert_eq!(
            call(&h.app, "GET", "/devices", &key, json!(null)).await.0,
            StatusCode::OK
        );
        let user = json!({"username": "eve", "password": "pw-pw-pw-pw", "role": "admin"});
        assert_eq!(
            call(&h.app, "POST", "/auth/users", &key, user).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&h.app, "GET", "/displays", &key, json!(null)).await.0,
            StatusCode::FORBIDDEN
        );
        // It may run a scene (here: one that doesn't exist), not edit one.
        let run = call(
            &h.app,
            "POST",
            &format!("/scenes/{}/activate", Uuid::new_v4()),
            &key,
            json!({}),
        )
        .await;
        assert_ne!(run.0, StatusCode::FORBIDDEN);

        let (_, list) = call(&h.app, "GET", "/displays", &h.admin, json!(null)).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert!(list[0].get("token").is_none(), "the key is shown once");

        let id = made["id"].as_str().unwrap();
        assert_eq!(
            call(
                &h.app,
                "DELETE",
                &format!("/displays/{id}"),
                &h.admin,
                json!(null)
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&h.app, "GET", "/auth/me", &key, json!(null)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_pin_unlocks_a_display_as_its_person_for_a_few_minutes() {
        let h = house().await;
        let (_, made) = call(
            &h.app,
            "POST",
            "/displays",
            &h.admin,
            json!({"name": "Hall"}),
        )
        .await;
        let key = made["token"].as_str().unwrap().to_string();

        // Setting a PIN takes your password.
        let wrong = json!({"pin": "2468", "password": "nope"});
        assert_eq!(
            call(&h.app, "PUT", "/auth/pin", &h.person, wrong).await.0,
            StatusCode::UNAUTHORIZED
        );
        let short = json!({"pin": "12", "password": "correct horse"});
        assert_eq!(
            call(&h.app, "PUT", "/auth/pin", &h.person, short).await.0,
            StatusCode::BAD_REQUEST
        );
        let set = json!({"pin": "2468", "password": "correct horse"});
        assert_eq!(
            call(&h.app, "PUT", "/auth/pin", &h.person, set).await.0,
            StatusCode::NO_CONTENT
        );
        // Nobody else may use it: the PIN alone says who is there.
        let same = json!({"pin": "2468", "password": "correct horse"});
        assert_eq!(
            call(&h.app, "PUT", "/auth/pin", &h.admin, same).await.0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            call(&h.app, "GET", "/auth/me", &h.person, json!(null))
                .await
                .1["has_pin"],
            true
        );

        // Only a display may swap a PIN for a session.
        let pin = json!({"pin": "2468"});
        assert_eq!(
            call(&h.app, "POST", "/auth/pin-session", &h.person, pin.clone())
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let (st, session) = call(&h.app, "POST", "/auth/pin-session", &key, pin).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(session["user"]["username"], "bo");
        assert_eq!(session["expires_in"], PIN_SESSION_SECS);
        let token = session["token"].as_str().unwrap();
        assert_eq!(
            call(&h.app, "GET", "/auth/me", token, json!(null)).await.1["username"],
            "bo"
        );

        // Wrong PINs: a few, then the display waits.
        for _ in 0..PIN_MAX_FAILURES {
            let (st, _) = call(
                &h.app,
                "POST",
                "/auth/pin-session",
                &key,
                json!({"pin": "0000"}),
            )
            .await;
            assert_eq!(st, StatusCode::UNAUTHORIZED);
        }
        let (st, _) = call(
            &h.app,
            "POST",
            "/auth/pin-session",
            &key,
            json!({"pin": "2468"}),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::TOO_MANY_REQUESTS,
            "even the right PIN waits"
        );
    }
}
