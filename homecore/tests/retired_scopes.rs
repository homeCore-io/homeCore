//! Dashboards, skins and the asset store left core. Keys made before that
//! still hold `dashboards:*` and `skins:*` in their stored scope lists. An
//! upgrade must not strand them: the key still loads, still works for what it
//! can still do, the removed routes answer an ordinary 404, and the retired
//! scopes are never written back.

mod common;
use anyhow::Result;
use common::*;
use hc_api_types::api_keys::{ApiKeySummary, CreateApiKeyRequest, UpdateApiKeyRequest};
use hc_api_types::auth::{LoginRequest, LoginResponse};
use hc_auth::{api_key, hash_password, Role, User};
use hc_state::{ApiKeyRecord, StateStore};
use tempfile::TempDir;
use uuid::Uuid;

fn has_retired(scopes: &[String]) -> bool {
    scopes.iter().any(|s| hc_auth::user::is_retired_scope(s))
}

#[tokio::test]
async fn a_key_stored_with_retired_scopes_still_works() -> Result<()> {
    let tmp = TempDir::new()?;

    // What an install upgraded from an older build has on disk: an admin, and
    // a key granted a dashboard scope beside a live one.
    let owner = User {
        id: Uuid::new_v4(),
        username: "root".into(),
        password_hash: hash_password("adminpassword123")?,
        role: Role::Admin,
        created_at: chrono::Utc::now(),
        token_version: 0,
        pin_hash: None,
    };
    let key = api_key::generate()?;
    let key_id = Uuid::new_v4();
    {
        let store = StateStore::open(
            tmp.path().join("state.redb").to_str().unwrap(),
            tmp.path().join("history.db").to_str().unwrap(),
        )
        .await?;
        store.create_user(&owner).await?;
        store
            .create_api_key(&ApiKeyRecord {
                id: key_id,
                prefix: key.lookup_prefix.clone(),
                hash: key.hash.clone(),
                owner_uid: owner.id,
                scopes: vec![
                    "devices:read".into(),
                    "dashboards:read".into(),
                    "skins:write".into(),
                ],
                label: "old wall panel".into(),
                created_at: chrono::Utc::now(),
                last_used_at: None,
                expires_at: None,
                allowed_cidrs: vec![],
                revoked_at: None,
                display: false,
            })
            .await?;
    }

    let h = Harness::start(&tmp).await?;

    // The key loads and is judged on what it still means.
    let key_c = h.client(Some(&key.full_token));
    let _: serde_json::Value = key_c.get("/devices").await?;

    // The routes the retired scopes guarded are gone: an ordinary 404.
    for path in ["/dashboards", "/skins", "/assets"] {
        let err = key_c
            .get::<serde_json::Value>(path)
            .await
            .expect_err(&format!("{path} should be gone"));
        assert!(format!("{err}").contains("404"), "{path}: {err}");
    }

    let login: LoginResponse = h
        .client(None)
        .post(
            "/auth/login",
            &LoginRequest {
                username: "root".into(),
                password: "adminpassword123".into(),
            },
        )
        .await?;
    let admin_c = h.client(Some(&login.token));

    // A listing does not present them as if they meant something.
    let keys: Vec<ApiKeySummary> = admin_c.get("/auth/api-keys").await?;
    let listed = keys.iter().find(|k| k.id == key_id).expect("key listed");
    assert_eq!(listed.scopes, vec!["devices:read".to_string()]);

    // A client editing the key sends back an old list; that is not refused,
    // and the retired scopes do not survive the write.
    let updated: ApiKeySummary = admin_c
        .patch(
            &format!("/auth/api-keys/{key_id}"),
            &UpdateApiKeyRequest {
                label: None,
                scopes: Some(vec!["devices:read".into(), "dashboards:write".into()]),
                expires_in_days: None,
                allowed_cidrs: None,
            },
        )
        .await?;
    assert!(!has_retired(&updated.scopes), "{:?}", updated.scopes);

    // Nor are they written onto a new key — the create response is the
    // stored record, unfiltered.
    let made: serde_json::Value = admin_c
        .post(
            "/auth/api-keys",
            &CreateApiKeyRequest {
                label: "new".into(),
                scopes: vec!["devices:read".into(), "skins:read".into()],
                expires_in_days: None,
                allowed_cidrs: vec![],
                owner_uid: None,
            },
        )
        .await?;
    let made_scopes: Vec<String> = serde_json::from_value(made["scopes"].clone())?;
    assert_eq!(made_scopes, vec!["devices:read".to_string()]);

    h.stop().await;
    Ok(())
}
