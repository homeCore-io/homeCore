//! Admin password reset must terminate the target's sessions — and only the
//! target's. Split from `session_invalidation.rs` into its own binary because
//! the login rate limiter is process-global; see `tests/common/mod.rs`.

mod common;
use anyhow::Result;
use common::*;
use hc_api_types::auth::SetPasswordRequest;
use hc_auth::Role;
use tempfile::TempDir;

#[tokio::test]
async fn admin_reset_invalidates_target_but_not_bystanders() -> Result<()> {
    let tmp = TempDir::new()?;
    let h = Harness::start(&tmp).await?;

    let admin = make_user(&h, "root", "adminpassword123", Role::Admin).await?;
    let victim = make_user(&h, "bob", "bobspassword123", Role::User).await?;
    let bystander = make_user(&h, "carol", "carolspassword123", Role::User).await?;

    let admin_c = h.client(Some(&admin.token));
    let victim_c = h.client(Some(&victim.token));
    let bystander_c = h.client(Some(&bystander.token));

    assert_ok(&victim_c, "victim token before reset").await;
    assert_ok(&bystander_c, "bystander token before reset").await;

    // Admin resets bob's password — the compromised-account path.
    admin_c
        .patch::<_, serde_json::Value>(
            &format!("/auth/users/{}/password", victim.user.id),
            &SetPasswordRequest {
                new_password: "freshpassword123".into(),
            },
        )
        .await?;

    assert_unauthorized(&victim_c, "victim token after admin reset").await;

    // Nobody else is disturbed — not the admin who performed the reset, and
    // not an unrelated account. A revocation that logs everyone out would
    // "pass" the victim assertion while being useless.
    assert_ok(&admin_c, "admin's own token after resetting someone else").await;
    assert_ok(&bystander_c, "unrelated user's token after a reset").await;

    h.stop().await;
    Ok(())
}
