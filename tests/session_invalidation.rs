//! Password changes must terminate live sessions.
//!
//! Before `User::token_version` existed, a JWT stayed valid for its full
//! expiry regardless of what happened to the password behind it. Resetting a
//! compromised account changed what the attacker would have to type next time
//! and nothing else — the session they already held kept working.
//!
//! The admin-reset half lives in `session_invalidation_admin.rs`; see
//! `tests/common/mod.rs` for why they are separate binaries.

mod common;
use anyhow::Result;
use common::*;
use hc_api_types::api_keys::{CreateApiKeyRequest, CreateApiKeyResponse};
use hc_api_types::auth::{ChangePasswordRequest, LoginRequest, LoginResponse, RefreshRequest};
use hc_auth::Role;
use hc_cli::client::{Client, Transport};
use tempfile::TempDir;

#[tokio::test]
async fn self_password_change_invalidates_own_token_and_refresh() -> Result<()> {
    let tmp = TempDir::new()?;
    let h = Harness::start(&tmp).await?;

    let old_pw = "originalpassword1";
    let new_pw = "replacementpassword1";
    let login = make_user(&h, "alice", old_pw, Role::Admin).await?;
    let old_token = login.token.clone();
    let old_refresh = login
        .refresh_token
        .clone()
        .expect("login should mint a refresh token");

    let alice = h.client(Some(&old_token));
    assert_ok(&alice, "token before password change").await;

    // Change the password using that very token.
    alice
        .post::<_, serde_json::Value>(
            "/auth/change-password",
            &ChangePasswordRequest {
                current_password: old_pw.into(),
                new_password: new_pw.into(),
            },
        )
        .await?;

    // The token that authorised the change is itself now dead.
    assert_unauthorized(&alice, "access token after self password change").await;

    // And the refresh token cannot resurrect it. This is the half that makes
    // the difference between a real revocation and a cosmetic one.
    let refreshed = h
        .client(None)
        .post::<_, serde_json::Value>(
            "/auth/refresh",
            &RefreshRequest {
                refresh_token: old_refresh,
            },
        )
        .await;
    assert!(
        refreshed.is_err(),
        "revoked refresh token must not mint a new access token"
    );

    // The new password works and yields a token that does.
    let relogin: LoginResponse = h
        .client(None)
        .post(
            "/auth/login",
            &LoginRequest {
                username: "alice".into(),
                password: new_pw.into(),
            },
        )
        .await?;
    assert_ok(&h.client(Some(&relogin.token)), "token from new password").await;

    h.stop().await;
    Ok(())
}

#[tokio::test]
async fn api_key_survives_owner_password_change() -> Result<()> {
    let tmp = TempDir::new()?;
    let h = Harness::start(&tmp).await?;

    let old_pw = "servicepassword123";
    let login = make_user(&h, "svc", old_pw, Role::Admin).await?;

    let uds = Client::new(Transport::Uds {
        socket: h.uds_path.clone(),
    });
    let key: CreateApiKeyResponse = uds
        .post(
            "/auth/api-keys",
            &CreateApiKeyRequest {
                label: "integration".into(),
                scopes: vec!["devices:read".into()],
                expires_in_days: None,
                allowed_cidrs: vec![],
                owner_uid: Some(login.user.id),
            },
        )
        .await?;

    let key_c = h.client(Some(&key.token));
    assert_ok(&key_c, "api key before owner password change").await;

    h.client(Some(&login.token))
        .post::<_, serde_json::Value>(
            "/auth/change-password",
            &ChangePasswordRequest {
                current_password: old_pw.into(),
                new_password: "differentpassword123".into(),
            },
        )
        .await?;

    // Deliberate: an API key is a separate credential with its own revocation
    // path. Killing integrations because a human rotated their password would
    // be a surprise with no visible cause.
    assert_ok(&key_c, "api key after owner password change").await;

    h.stop().await;
    Ok(())
}
