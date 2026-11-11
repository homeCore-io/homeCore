//! Room on, Room off and what Room off saved.
//!
//! `POST /areas/{id}/on` and `POST /areas/{id}/off` take the same permission
//! as commanding a device (`devices:command`, which `devices:write` implies),
//! since that is all they do, one light at a time. The work is
//! `hc_core::room_lights`, which the `RoomOn` and `RoomOff` rule actions use
//! too. `GET /areas/{id}/saved-lights` is a read of the room and takes
//! `areas:read`.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use hc_core::{room_lights, rooms};
use hc_types::device::DeviceChange;
use serde_json::json;
use uuid::Uuid;

use crate::auth_middleware::{AreasRead, DevicesCommand};
use crate::handlers::publish_device_command;
use crate::AppState;

/// `POST /areas/{id}/on`
pub async fn room_on(
    State(s): State<AppState>,
    DevicesCommand(claims): DevicesCommand,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    switch(s, claims, id, true).await
}

/// `POST /areas/{id}/off`
pub async fn room_off(
    State(s): State<AppState>,
    DevicesCommand(claims): DevicesCommand,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    switch(s, claims, id, false).await
}

async fn switch(
    s: AppState,
    claims: hc_auth::Claims,
    id: Uuid,
    on: bool,
) -> axum::response::Response {
    let room = match rooms::get(&s.store, id).await {
        Ok(Some(room)) => room,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "area not found" })),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };
    let correlation_id = Uuid::new_v4().to_string();
    let change = DeviceChange::homecore("api")
        .with_actor(Some(claims.uid), Some(claims.sub))
        .with_correlation_id(Some(correlation_id.clone()));
    let origin = room_lights::Origin {
        source: "api".into(),
        correlation_id: Some(correlation_id),
    };
    let outcome = room_lights::switch(
        &s.store,
        Some(&s.event_bus),
        &room,
        on,
        origin,
        |device_id, command| {
            let s = s.clone();
            let change = change.clone();
            async move { publish_device_command(&s, &device_id, command, change).await }
        },
    )
    .await;
    match outcome {
        Ok(outcome) => (StatusCode::OK, Json(json!(outcome))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `GET /areas/{id}/saved-lights` — what Room off last saved for the room;
/// 204 when it has saved nothing.
pub async fn saved_lights(
    State(s): State<AppState>,
    _: AreasRead,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let found = async {
        if s.store.get_area(id).await?.is_none() {
            return Ok(None);
        }
        s.store.room_lights_get(id).await.map(Some)
    };
    match found.await {
        Ok(Some(Some(saved))) => (StatusCode::OK, Json(json!(saved))).into_response(),
        Ok(Some(None)) => StatusCode::NO_CONTENT.into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "area not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use chrono::Utc;
    use hc_auth::{Role, User};
    use hc_types::device::DeviceState;
    use hc_types::event::Event;
    use serde_json::json;
    use tower::ServiceExt;
    use uuid::Uuid;

    const SECRET: &[u8] = b"room-lights-test-secret-32-bytes!";

    struct House {
        app: axum::Router,
        store: hc_state::StateStore,
        bus: hc_core::EventBus,
        jwt: hc_auth::JwtService,
        _dir: tempfile::TempDir,
    }

    impl House {
        async fn token(&self, role: Role) -> String {
            let u = User {
                id: Uuid::new_v4(),
                username: format!("{role:?}").to_lowercase(),
                password_hash: hc_auth::hash_pin("correct horse").unwrap(),
                role,
                created_at: Utc::now(),
                token_version: 0,
                pin_hash: None,
            };
            self.store.create_user(&u).await.unwrap();
            self.jwt
                .issue(&u.id.to_string(), &u.username, u.role, u.token_version)
                .unwrap()
        }

        async fn call(
            &self,
            method: &str,
            path: &str,
            token: &str,
        ) -> (StatusCode, serde_json::Value) {
            let mut req = Request::builder()
                .method(method)
                .uri(format!("/api/v1{path}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(ConnectInfo(std::net::SocketAddr::from((
                    [10, 9, 8, 7],
                    1234,
                ))));
            let res = self.app.clone().oneshot(req).await.unwrap();
            let status = res.status();
            let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
            )
        }
    }

    async fn house() -> (House, Uuid) {
        let dir = tempfile::tempdir().unwrap();
        let store = hc_state::StateStore::open(
            dir.path().join("state.redb").to_str().unwrap(),
            dir.path().join("history.db").to_str().unwrap(),
        )
        .await
        .unwrap();
        let (room, _) = hc_core::rooms::create(&store, None, "Den").await.unwrap();
        for (id, on) in [("lamp", true), ("ceiling", false)] {
            let mut d = DeviceState::new(id, id, "plugin.test");
            d.device_type = Some("light".into());
            d.area = Some("den".into());
            d.attributes.insert("on".into(), json!(on));
            store.upsert_device(&d).await.unwrap();
        }
        let bus = hc_core::EventBus::new(64);
        let state = crate::AppState::new(crate::AppStateParams::new(
            store.clone(),
            bus.clone(),
            hc_auth::JwtService::new_hs256(SECRET, 24),
        ));
        (
            House {
                app: crate::router(state, None),
                store,
                bus,
                jwt: hc_auth::JwtService::new_hs256(SECRET, 24),
                _dir: dir,
            },
            room.id,
        )
    }

    /// Whoever may command a device may switch a room; whoever may only
    /// read the house may not, though they may see what was saved.
    #[tokio::test]
    async fn switching_a_room_takes_the_permission_commanding_a_device_does() {
        let (h, room) = house().await;
        let viewer = h.token(Role::ReadOnly).await;
        let member = h.token(Role::HouseholdMember).await;

        let (st, _) = h.call("POST", &format!("/areas/{room}/off"), &viewer).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, _) = h.call("POST", &format!("/areas/{room}/on"), &viewer).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, _) = h
            .call("GET", &format!("/areas/{room}/saved-lights"), &viewer)
            .await;
        assert_eq!(st, StatusCode::NO_CONTENT, "nothing saved yet");

        let mut rx = h.bus.subscribe();
        let (st, body) = h.call("POST", &format!("/areas/{room}/off"), &member).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["room_id"], json!(room.to_string()));
        assert_eq!(body["on"], json!(false));
        assert_eq!(body["lights"], json!(["ceiling", "lamp"]));
        assert_eq!(body["snapshot_saved"], json!(true));
        assert_eq!(body["commands"][1]["command"]["on"], json!(false));
        let mut announced = false;
        while let Ok(e) = rx.try_recv() {
            if let Event::RoomLightsChanged { source, .. } = e {
                assert_eq!(source, "api");
                announced = true;
            }
        }
        assert!(announced);

        let (st, saved) = h
            .call("GET", &format!("/areas/{room}/saved-lights"), &viewer)
            .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(saved["room_id"], json!(room.to_string()));
        assert_eq!(saved["lights"].as_array().unwrap().len(), 2);

        let (st, body) = h.call("POST", &format!("/areas/{room}/on"), &member).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["snapshot_used"], json!(true));
        assert_eq!(body["lights"], json!(["lamp"]));

        let audit = h
            .store
            .query_audit(&hc_state::AuditQuery {
                target_kind: Some("area".into()),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        let kinds: Vec<&str> = audit.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(kinds, vec!["area.lights_on", "area.lights_off"]);

        let missing = Uuid::new_v4();
        let (st, _) = h
            .call("POST", &format!("/areas/{missing}/on"), &member)
            .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = h
            .call("GET", &format!("/areas/{missing}/saved-lights"), &viewer)
            .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }
}
