//! Signing in to NuHeat: the actions the plugin's page calls, and a streaming
//! one for when the page isn't there.
//!
//! The page's way, [`Via::Relay`]:
//!
//! 1. `begin_link` with the page's own address. The plugin starts a PKCE
//!    sign-in and returns NuHeat's address; the page sends the browser there.
//! 2. The person signs in on NuHeat's site. NuHeat sends the browser to the
//!    relay page on homecore.io, which sends it back to the page with `code`
//!    and `state`.
//! 3. `complete_link` with them. The plugin exchanges the code (only it has
//!    the verifier), then proves the token by reading the account before
//!    saying it is linked.
//!
//! The fallback, [`Via::Loopback`]: NuHeat sends the browser to
//! `127.0.0.1:8127`, where nothing answers, and the person pastes that address
//! into `complete_link` (or the streaming `link_account`, from the generic
//! action list).

use anyhow::Result;
use plugin_sdk_rs::{PluginNotices, StreamContext, StreamingAction};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{info, warn};

use crate::api::NuHeatApi;
use crate::auth::{Auth, Returned, Via};
use crate::runtime::{NOTICE_AUTH_REJECTED, NOTICE_NOT_LINKED, NOTICE_RELINK_DUE};

/// What the link and sign-out actions need to do their work.
#[derive(Clone)]
pub struct LinkHandle {
    pub auth: Arc<Auth>,
    pub api: NuHeatApi,
    pub notices: PluginNotices,
    /// Cuts the poll interval short, so a freshly linked account shows its
    /// thermostats at once.
    pub wake: tokio::sync::mpsc::Sender<()>,
}

impl LinkHandle {
    /// `begin_link {via, return_to}` → `{url, redirect_uri}`.
    pub fn begin(&self, params: &Value) -> Value {
        let via = Via::parse(params["via"].as_str());
        match self.auth.begin(via, params["return_to"].as_str()) {
            Ok(b) => json!({
                "status": "ok",
                "url": b.url,
                "redirect_uri": b.redirect_uri,
                "state": b.state,
            }),
            Err(e) => json!({ "status": "error", "error": e.to_string() }),
        }
    }

    /// `complete_link {code, state}` or `{pasted}` → the account, once proven.
    pub async fn complete(&self, params: &Value) -> Value {
        let back = match params["pasted"].as_str() {
            Some(p) => Returned::from_pasted(p),
            None => Returned {
                code: params["code"].as_str().map(str::to_owned),
                state: params["state"].as_str().map(str::to_owned),
                error: params["error"].as_str().map(str::to_owned),
            },
        };
        match self.finish(&back).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "NuHeat account linking failed");
                json!({ "status": "error", "error": e.to_string() })
            }
        }
    }

    async fn finish(&self, back: &Returned) -> Result<Value> {
        self.auth.complete(back).await?;
        // Prove it before claiming it: a token NuHeat won't accept would
        // otherwise show up as "linked, but no thermostats".
        let bearer = self.auth.bearer().await?;
        let account = self
            .api
            .account(&bearer)
            .await
            .map_err(|e| anyhow::anyhow!("NuHeat didn't accept the sign-in: {e}"))?;
        self.auth.set_account(account.user_name.clone()).await;
        for n in [NOTICE_NOT_LINKED, NOTICE_AUTH_REJECTED, NOTICE_RELINK_DUE] {
            self.notices.clear(n);
        }
        let _ = self.wake.try_send(());
        info!(
            account = account.user_name.as_deref().unwrap_or("unknown"),
            "Linked to a NuHeat account"
        );
        Ok(json!({
            "status": "ok",
            "linked": true,
            "account": account.user_name,
            "relink_due": self.auth.relink_due().map(|t| t.to_rfc3339()),
        }))
    }

    /// `sign_out` → forget the sign-in. Devices stay registered: signing out
    /// is usually a step in signing in again, and unregistering would take
    /// every rule and card with it.
    pub async fn sign_out(&self) -> Value {
        self.auth.sign_out().await;
        self.notices.clear(NOTICE_RELINK_DUE);
        info!("Signed out of NuHeat and cleared the stored tokens");
        json!({ "status": "ok", "signed_out": true })
    }
}

pub fn streaming_link(handle: LinkHandle) -> StreamingAction {
    StreamingAction::new("link_account", move |ctx, _params| {
        let h = handle.clone();
        async move { link_account(ctx, h).await }
    })
}

/// The generic action list's way in: open NuHeat, paste the address back.
async fn link_account(ctx: StreamContext, h: LinkHandle) -> Result<()> {
    let begun = match h.auth.begin(Via::Loopback, None) {
        Ok(b) => b,
        Err(e) => return ctx.error(e.to_string()).await,
    };
    ctx.progress(
        None,
        Some("Waiting for you to sign in at NuHeat"),
        Some(&begun.url),
    )
    .await?;
    let response = ctx
        .awaiting_user_with_schema(
            format!(
                "Open this link and sign in to NuHeat:\n\n{}\n\nNuHeat then sends your \
                 browser to an address that won't load. That's expected: copy the whole \
                 address from the address bar and paste it here.",
                begun.url
            ),
            json!({ "pasted": { "type": "string", "required": true, "label": "The address you landed on" } }),
        )
        .await?;
    if ctx.is_canceled() {
        return ctx.canceled().await;
    }
    let pasted = response["pasted"].as_str().unwrap_or_default();
    ctx.progress(None, Some("Checking the sign-in with NuHeat"), None)
        .await?;
    match h.finish(&Returned::from_pasted(pasted)).await {
        Ok(v) => ctx.complete(v).await,
        Err(e) => {
            warn!(error = %e, "NuHeat account linking failed");
            ctx.error(format!("Couldn't sign in: {e}")).await
        }
    }
}
