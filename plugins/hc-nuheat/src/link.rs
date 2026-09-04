//! The `link_account` streaming action — signing in to NuHeat from the plugin
//! page.
//!
//! NuHeat only registers redirect URIs it controls, so there is no callback
//! this plugin could serve and no way to catch the token as the browser
//! receives it. The operator has to carry it across by hand. A streaming
//! action is what makes that bearable: the drawer shows the link, waits, takes
//! the paste, and — the part that matters — *proves the token works* by calling
//! `GET /api/v2/Account` before saying it is linked. Without that check the
//! only signal a bad paste gives is thermostats that never appear.
//!
//! The same action serves both auth modes. What the operator pastes differs —
//! a token in `access_token` mode, a `code` in `oauth` mode — so the prompt
//! and the schema field change with it, and [`crate::auth::Auth`] knows what to
//! do with either.

use anyhow::Result;
use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{ManagementHandle, PluginNotices, StreamContext, StreamingAction};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{info, warn};

use crate::api::NuHeatApi;
use crate::auth::{Auth, AuthMode};
use crate::runtime::{NOTICE_NOT_CONFIGURED, NOTICE_NOT_LINKED, NOTICE_TOKEN_EXPIRING};

/// What the link and sign-out actions need to do their work.
#[derive(Clone)]
pub struct LinkHandle {
    pub auth: Arc<Auth>,
    pub api: NuHeatApi,
    pub notices: PluginNotices,
    /// Nudges the poll loop to run immediately rather than waiting out its
    /// interval, so a freshly linked account shows its thermostats at once.
    pub wake: tokio::sync::mpsc::Sender<()>,
}

pub fn register_actions(mgmt: ManagementHandle, handle: LinkHandle) -> ManagementHandle {
    let link = handle.clone();
    let out = handle;
    mgmt.with_streaming_action(StreamingAction::new("link_account", move |ctx, params| {
        let h = link.clone();
        async move { link_account(ctx, params, h).await }
    }))
    .with_streaming_action(StreamingAction::new("sign_out", move |ctx, _params| {
        let h = out.clone();
        async move { sign_out(ctx, h).await }
    }))
}

async fn link_account(ctx: StreamContext, _params: Value, h: LinkHandle) -> Result<()> {
    let pending = match h.auth.begin_authorization() {
        Ok(p) => p,
        // A misconfigured oauth mode — no client id, no redirect URI. Saying
        // so here beats sending the operator to an identity-server error page
        // that only says "invalid request".
        Err(e) => return ctx.error(e.to_string()).await,
    };

    let (prompt, field, label) = match h.auth.mode() {
        AuthMode::AccessToken => (
            format!(
                "Open this link, sign in to NuHeat, and copy what lands in the address bar.\n\n\
                 {}\n\n\
                 The page will look empty — that is expected. The whole URL is fine; it \
                 contains the token after the # sign.\n\n\
                 Note: this token expires in one hour and NuHeat does not allow it to be \
                 renewed. For a plugin you leave running, ask NuHeat support for a client \
                 id and switch this plugin to \"Your own client id\".",
                pending.url
            ),
            "token",
            "Pasted URL or token",
        ),
        AuthMode::OAuth => (
            format!(
                "Open this link, sign in to NuHeat, and copy the address you are returned \
                 to.\n\n{}\n\nIt contains a `code` parameter. The whole URL is fine.",
                pending.url
            ),
            "code",
            "Pasted URL or code",
        ),
    };

    ctx.progress(
        None,
        Some("Waiting for you to sign in at NuHeat"),
        Some(&pending.url),
    )
    .await?;

    let response = ctx
        .awaiting_user_with_schema(
            prompt,
            json!({ field: { "type": "string", "required": true, "label": label } }),
        )
        .await?;

    if ctx.is_canceled() {
        return ctx.canceled().await;
    }

    let pasted = response
        .get(field)
        .and_then(Value::as_str)
        // Accept the other field name too. The two modes differ only in which
        // one the schema advertises, and an operator switching modes mid-setup
        // should not hit a silent empty string.
        .or_else(|| response.get("token").and_then(Value::as_str))
        .or_else(|| response.get("code").and_then(Value::as_str))
        .unwrap_or_default();

    if let Err(e) = h.auth.complete_authorization(pasted, &pending).await {
        warn!(error = %e, "NuHeat account linking failed");
        return ctx.error(format!("could not sign in: {e}")).await;
    }

    // Prove it before claiming it. A token that parses but is not accepted
    // otherwise shows up as "linked, but no thermostats", which sends the
    // operator looking at their thermostats instead of at their token.
    ctx.progress(None, Some("Checking the token against your account"), None)
        .await?;

    let bearer = match h.auth.bearer().await {
        Ok(b) => b,
        Err(e) => {
            return ctx
                .error(format!("no usable token after signing in: {e}"))
                .await
        }
    };

    match h.api.account(&bearer).await {
        Ok(account) => {
            // A sign-in that reached NuHeat proves the credentials are there,
            // so both notices are disproved at once.
            h.notices.clear(NOTICE_NOT_CONFIGURED);
            h.notices.clear(NOTICE_NOT_LINKED);
            h.notices.clear(NOTICE_TOKEN_EXPIRING);
            let _ = h.wake.try_send(());
            let expires_in_secs = h.auth.expires_in().map(|d| d.num_seconds());
            info!(
                user = account.user_name.as_deref().unwrap_or("unknown"),
                "Linked to a NuHeat account"
            );
            // The hourly-expiry caveat is repeated on success on purpose: it is
            // the moment the operator is most likely to walk away believing
            // this is finished.
            if h.auth.mode() == AuthMode::AccessToken {
                h.notices.raise(
                    PluginNotice::info(
                        NOTICE_TOKEN_EXPIRING,
                        "Signed in with a pasted token, which NuHeat expires after an hour.",
                    )
                    .with_remedy(
                        "For unattended use, ask NuHeat support for a client id and switch \
                         this plugin to \"Your own client id\".",
                    ),
                );
            }
            ctx.complete(json!({
                "linked": true,
                "account": account.user_name,
                "temperature_scale": account.temperature_scale,
                "expires_in_secs": expires_in_secs,
                "renewable": h.auth.mode() == AuthMode::OAuth,
            }))
            .await
        }
        Err(e) => {
            warn!(error = %e, "The pasted NuHeat credential did not work");
            ctx.error(format!("NuHeat did not accept that: {e}")).await
        }
    }
}

async fn sign_out(ctx: StreamContext, h: LinkHandle) -> Result<()> {
    h.auth.sign_out().await?;
    h.notices.clear(NOTICE_TOKEN_EXPIRING);
    h.notices.raise(
        PluginNotice::warning(
            NOTICE_NOT_LINKED,
            "Not signed in to a NuHeat account, so no thermostats are published.",
        )
        .with_remedy("Use the \"Link NuHeat account\" button on this page."),
    );
    info!("Signed out of NuHeat and cleared the stored tokens");
    // Devices are deliberately left registered. Signing out is usually a step
    // in re-linking, and unregistering would take every rule and dashboard
    // reference with it. `DELETE /api/v1/plugins/{id}/devices` is the deliberate
    // way to clear them.
    ctx.complete(json!({ "signed_out": true })).await
}
