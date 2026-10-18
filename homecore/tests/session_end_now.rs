//! Ending a session — from another device, or by signing out — must stop its
//! access token at once, not when it expires a day later. Own binary: the
//! login rate limiter is process-global (see `tests/common/mod.rs`).

mod common;
use anyhow::Result;
use common::*;
use hc_api_types::auth::{LoginRequest, LoginResponse, RefreshRequest};
use hc_auth::Role;
use tempfile::TempDir;

#[tokio::test]
async fn ending_a_session_stops_its_access_token_now() -> Result<()> {
    let tmp = TempDir::new()?;
    let h = Harness::start(&tmp).await?;

    let phone = make_user(&h, "sam", "samspassword123", Role::User).await?;
    let laptop: LoginResponse = h
        .client(None)
        .post(
            "/auth/login",
            &LoginRequest {
                username: "sam".into(),
                password: "samspassword123".into(),
            },
        )
        .await?;
    let phone_c = h.client(Some(&phone.token));
    let laptop_c = h.client(Some(&laptop.token));
    assert_ok(&phone_c, "phone before").await;
    assert_ok(&laptop_c, "laptop before").await;

    // The laptop sees both, and knows which is itself.
    let list: Vec<serde_json::Value> = laptop_c.get("/auth/sessions").await?;
    assert_eq!(list.len(), 2);
    assert_eq!(list.iter().filter(|s| s["current"] == true).count(), 1);
    let phone_session = list
        .iter()
        .find(|s| s["current"] == false)
        .and_then(|s| s["id"].as_str())
        .unwrap()
        .to_string();

    // From the laptop, end the lost phone: it stops now.
    laptop_c
        .delete(&format!("/auth/sessions/{phone_session}"))
        .await?;
    assert_unauthorized(&phone_c, "phone after it was ended").await;
    assert_ok(&laptop_c, "laptop, which ended it").await;

    // Signing the laptop out stops it too.
    let _: serde_json::Value = h
        .client(None)
        .post(
            "/auth/logout",
            &RefreshRequest {
                refresh_token: laptop.refresh_token.clone().unwrap(),
            },
        )
        .await
        .or_else(|e| {
            // 204 has no body to decode; that's success.
            if format!("{e}").contains("EOF") || format!("{e}").contains("decod") {
                Ok(serde_json::Value::Null)
            } else {
                Err(e)
            }
        })?;
    assert_unauthorized(&laptop_c, "laptop after signing out").await;

    h.stop().await;
    Ok(())
}
