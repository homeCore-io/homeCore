//! A key must not be a way to more access than it was given. A display's key,
//! or a script's narrowed key, is owned by a person — often an admin — and
//! key management used to check a key's request only against that owner's
//! role. Whoever held the key could mint an admin key, widen itself, or drop
//! its address fence.

mod common;
use anyhow::Result;
use common::*;
use hc_api_types::api_keys::{CreateApiKeyRequest, CreateApiKeyResponse, UpdateApiKeyRequest};
use hc_auth::Role;
use tempfile::TempDir;

fn refused(r: Result<serde_json::Value>, what: &str) {
    match r {
        Ok(v) => panic!("{what}: expected 403, but it succeeded: {v}"),
        Err(e) => {
            let msg = format!("{e}");
            assert!(msg.contains("403"), "{what}: expected 403, got: {msg}");
        }
    }
}

#[tokio::test]
async fn a_key_cannot_make_or_widen_keys() -> Result<()> {
    let tmp = TempDir::new()?;
    let h = Harness::start(&tmp).await?;
    let admin = make_user(&h, "root", "adminpassword123", Role::Admin).await?;
    let admin_c = h.client(Some(&admin.token));

    // A narrow key, as a display or a script would hold.
    let narrow: CreateApiKeyResponse = admin_c
        .post(
            "/auth/api-keys",
            &CreateApiKeyRequest {
                label: "tablet".into(),
                scopes: vec!["devices:read".into()],
                expires_in_days: None,
                allowed_cidrs: vec!["127.0.0.0/8".into(), "10.0.10.0/24".into()],
                owner_uid: None,
            },
        )
        .await?;
    let key_c = h.client(Some(&narrow.token));
    // It works for what it was given, so a refusal below is the check, not
    // the address fence.
    let _: serde_json::Value = key_c.get("/devices").await?;

    refused(
        key_c
            .post(
                "/auth/api-keys",
                &CreateApiKeyRequest {
                    label: "mine now".into(),
                    scopes: vec!["users:write".into(), "api_keys:admin".into()],
                    expires_in_days: None,
                    allowed_cidrs: vec![],
                    owner_uid: None,
                },
            )
            .await,
        "a key minting an admin key",
    );
    refused(
        key_c
            .patch(
                &format!("/auth/api-keys/{}", narrow.id),
                &UpdateApiKeyRequest {
                    label: None,
                    scopes: Some(vec!["users:write".into()]),
                    expires_in_days: None,
                    allowed_cidrs: Some(vec![]),
                },
            )
            .await,
        "a key widening and unfencing itself",
    );

    // A person still can't give a key more than they hold themselves.
    let user = make_user(&h, "sam", "samspassword123", Role::User).await?;
    refused(
        h.client(Some(&user.token))
            .post(
                "/auth/api-keys",
                &CreateApiKeyRequest {
                    label: "too much".into(),
                    scopes: vec!["users:write".into()],
                    expires_in_days: None,
                    allowed_cidrs: vec![],
                    owner_uid: None,
                },
            )
            .await,
        "a person asking for a scope they lack",
    );

    // The admin can still manage the key.
    let _: serde_json::Value = admin_c
        .patch(
            &format!("/auth/api-keys/{}", narrow.id),
            &UpdateApiKeyRequest {
                label: Some("kitchen tablet".into()),
                scopes: None,
                expires_in_days: None,
                allowed_cidrs: None,
            },
        )
        .await?;

    h.stop().await;
    Ok(())
}
