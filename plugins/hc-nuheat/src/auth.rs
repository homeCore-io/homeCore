//! Getting — and keeping — a bearer token for the NuHeat OpenAPI.
//!
//! Every endpoint is `security: [{oauth2: []}]`, and the old session API that
//! third-party NuHeat integrations used to call (`POST /api/authenticate/user`,
//! returning a `SessionId`) is gone: it answers 404. So OAuth2 against
//! `identity.mynuheat.com` is the only way in, and which flow is available
//! depends entirely on the client id.
//!
//! ## What the identity server actually permits
//!
//! Measured against the live server rather than read off the documentation,
//! which disagrees with itself about this:
//!
//! | client | grant | result |
//! |---|---|---|
//! | `swagger` | implicit (`token`, `id_token token`), scope `openapi` | accepted |
//! | `swagger` | implicit **+ `offline_access`** | rejected |
//! | `swagger` | `code`, hybrid | rejected |
//! | `swagger` | any redirect_uri but NuHeat's own | rejected |
//! | `swagger`, `js` | password, device_code | `unauthorized_client` |
//!
//! `swagger` is the client id NuHeat's own Swagger UI ships
//! (`/swagger/index.js`), and it is registered and usable by anyone. That makes
//! it the no-paperwork path — but implicit cannot issue refresh tokens, so what
//! it yields is a **one-hour access token and nothing to renew it with**.
//!
//! ## The two modes, and why both exist
//!
//! [`AuthMode::AccessToken`] is that path: the operator signs in at NuHeat, the
//! token lands in the fragment of NuHeat's own redirect page, and they paste it
//! into the `link_account` action. It works today with no application to file,
//! and it needs redoing every hour, so it is for trying the plugin out rather
//! than for leaving it running.
//!
//! [`AuthMode::OAuth`] is the authorization-code flow against a client id
//! issued by NuHeat support. With `offline_access` it returns a refresh token
//! (15 days, rolling), which is what lets the plugin run unattended: this
//! module renews the access token in the background and persists each new
//! refresh token as it arrives.
//!
//! Both modes converge on [`Auth::bearer`], so nothing above this module knows
//! which one is in play.
//!
//! ## Where the tokens live
//!
//! In **core's durable learned state** (`homecore/plugins/{id}/state`), via
//! [`PluginStateWriter`] — not in the config file. This follows hc-hue, which
//! persists its bridge `app_key` the same way and for the same reason: the
//! config file is core-owned and watched, so writing a token into it would trip
//! the hot-reload watcher and restart the plugin mid-flow. It is the same
//! exposure hc-hue's app_key already has.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use plugin_sdk_rs::PluginStateWriter;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

pub const IDENTITY_BASE: &str = "https://identity.mynuheat.com";
pub const API_BASE: &str = "https://api.mynuheat.com";

/// The client id NuHeat's own Swagger UI is configured with, and the only one
/// a third party can use without applying for access. Implicit grant only.
pub const PUBLIC_CLIENT_ID: &str = "swagger";

/// The one redirect URI registered for [`PUBLIC_CLIENT_ID`]. Anything else —
/// localhost included — is rejected at `/connect/authorize`, which is why the
/// token has to be copied out of the browser by hand in that mode.
pub const PUBLIC_REDIRECT_URI: &str = "https://api.mynuheat.com/swagger/oauth2-redirect.html";

/// Renew this long before the access token actually expires.
///
/// A poll that starts just inside the deadline and takes a moment to reach the
/// API would otherwise 401 on a token that was valid when it was checked.
const RENEW_MARGIN: Duration = Duration::minutes(5);

/// Warn the operator this long before an unrenewable token runs out.
///
/// Only meaningful in [`AuthMode::AccessToken`] — there is nothing to renew,
/// so the only useful response is telling them to paste a new one before the
/// plugin goes blind rather than after.
const EXPIRY_WARNING: Duration = Duration::minutes(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Paste a token obtained through the implicit flow. No client id needed;
    /// expires hourly with no way to renew.
    #[default]
    AccessToken,
    /// Authorization code + PKCE against your own client id, with
    /// `offline_access` for a refresh token. Runs unattended.
    ///
    /// Renamed explicitly: `rename_all = "snake_case"` turns `OAuth` into
    /// `o_auth`, which is not what the config descriptor offers or what anyone
    /// would type.
    #[serde(rename = "oauth")]
    OAuth,
}

/// The tokens, as they are persisted and restored.
#[derive(Debug, Clone, Default)]
pub struct Tokens {
    pub access_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub refresh_token: Option<String>,
}

impl Tokens {
    fn from_state(state: &Value) -> Self {
        let auth = &state["auth"];
        Self {
            access_token: auth["access_token"].as_str().map(str::to_owned),
            expires_at: auth["expires_at"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc)),
            refresh_token: auth["refresh_token"].as_str().map(str::to_owned),
        }
    }

    fn to_state(&self) -> Value {
        json!({
            "auth": {
                "access_token": self.access_token,
                "expires_at": self.expires_at.map(|t| t.to_rfc3339()),
                "refresh_token": self.refresh_token,
            }
        })
    }

    /// Usable right now, with enough margin left to finish a request.
    fn is_fresh(&self) -> bool {
        match (&self.access_token, self.expires_at) {
            (Some(t), Some(exp)) if !t.is_empty() => exp - Utc::now() > RENEW_MARGIN,
            // A token with no known expiry is one an operator pasted without
            // the `expires_in` alongside it. Trust it and let a 401 be the
            // thing that disproves it.
            (Some(t), None) => !t.is_empty(),
            _ => false,
        }
    }
}

/// What the token endpoint returns.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// One in-flight PKCE authorization, held between `authorize_url` and the
/// `code` the operator pastes back.
#[derive(Debug, Clone)]
pub struct PendingAuthorization {
    pub url: String,
    pub verifier: String,
    pub state: String,
}

pub struct Auth {
    mode: AuthMode,
    client_id: String,
    client_secret: Option<String>,
    redirect_uri: String,
    http: reqwest::Client,
    tokens: Arc<Mutex<Tokens>>,
    writer: PluginStateWriter,
}

impl Auth {
    pub fn new(
        mode: AuthMode,
        client_id: Option<String>,
        client_secret: Option<String>,
        redirect_uri: Option<String>,
        http: reqwest::Client,
        writer: PluginStateWriter,
    ) -> Self {
        // In AccessToken mode the ids are fixed: they are the only pair the
        // identity server will accept without a registration, so letting an
        // operator override them would only produce a rejected authorize.
        let (client_id, redirect_uri) = match mode {
            AuthMode::AccessToken => (
                PUBLIC_CLIENT_ID.to_string(),
                PUBLIC_REDIRECT_URI.to_string(),
            ),
            AuthMode::OAuth => (
                client_id.unwrap_or_default(),
                redirect_uri.unwrap_or_default(),
            ),
        };
        Self {
            mode,
            client_id,
            client_secret: client_secret.filter(|s| !s.is_empty()),
            redirect_uri,
            http,
            tokens: Arc::new(Mutex::new(Tokens::default())),
            writer,
        }
    }

    pub fn mode(&self) -> AuthMode {
        self.mode
    }

    /// Adopt tokens from core's retained learned-state document.
    ///
    /// Called from the SDK state handler, which fires on connect and on every
    /// later change — so a token this plugin persisted before a restart is back
    /// in place before the first poll.
    pub fn adopt_persisted_state(&self, state: &Value) {
        let restored = Tokens::from_state(state);
        if restored.access_token.is_none() && restored.refresh_token.is_none() {
            return;
        }
        let mut guard = self.tokens.lock().expect("token mutex");
        // Only take what we do not already have. The retained doc echoes back
        // after our own write, and adopting it wholesale would overwrite a
        // newer token with the one that provoked the echo.
        if guard.access_token.is_none() && guard.refresh_token.is_none() {
            *guard = restored;
            info!("Restored NuHeat tokens from core's learned state");
        }
    }

    /// A bearer token good for the next request, renewing if it can.
    pub async fn bearer(&self) -> Result<String> {
        {
            let guard = self.tokens.lock().expect("token mutex");
            if guard.is_fresh() {
                return Ok(guard.access_token.clone().expect("fresh implies present"));
            }
        }

        let refresh_token = {
            let guard = self.tokens.lock().expect("token mutex");
            guard.refresh_token.clone()
        };

        match (self.mode, refresh_token) {
            (AuthMode::OAuth, Some(rt)) if !rt.is_empty() => {
                self.refresh(&rt).await.context("renewing the access token")
            }
            (AuthMode::OAuth, _) => {
                bail!("not linked to a NuHeat account yet — run the \"Link NuHeat account\" action")
            }
            // Implicit tokens cannot be renewed; the pasted one is all there is.
            (AuthMode::AccessToken, _) => {
                let guard = self.tokens.lock().expect("token mutex");
                match guard.access_token.clone() {
                    Some(t) if !t.is_empty() => Ok(t),
                    _ => bail!(
                        "no access token — run the \"Link NuHeat account\" action and paste a fresh one"
                    ),
                }
            }
        }
    }

    /// How long the current token has left, when that is knowable.
    pub fn expires_in(&self) -> Option<Duration> {
        let guard = self.tokens.lock().expect("token mutex");
        guard.expires_at.map(|exp| exp - Utc::now())
    }

    /// Whether the operator should be told to paste a new token soon.
    ///
    /// Always false in [`AuthMode::OAuth`], where running low is this module's
    /// problem to solve rather than the operator's.
    pub fn is_expiring_unrenewably(&self) -> bool {
        self.mode == AuthMode::AccessToken
            && self
                .expires_in()
                .is_some_and(|left| left < EXPIRY_WARNING && left > Duration::zero())
    }

    pub fn is_linked(&self) -> bool {
        let guard = self.tokens.lock().expect("token mutex");
        guard.access_token.is_some() || guard.refresh_token.is_some()
    }

    /// The URL the operator opens to sign in, plus the PKCE secret that has to
    /// survive until they paste the result back.
    pub fn begin_authorization(&self) -> Result<PendingAuthorization> {
        let state = random_urlsafe(16);
        let verifier = random_urlsafe(48);
        let challenge = pkce_challenge(&verifier);

        let url = match self.mode {
            // Implicit: the access token comes back in the URL *fragment* of
            // NuHeat's own redirect page, so there is no code to exchange and
            // no PKCE to verify — the operator copies the token itself.
            AuthMode::AccessToken => format!(
                "{IDENTITY_BASE}/connect/authorize?client_id={}&response_type=token\
                 &scope=openapi&redirect_uri={}&state={state}&nonce={}",
                urlencoding::encode(&self.client_id),
                urlencoding::encode(&self.redirect_uri),
                random_urlsafe(12),
            ),
            AuthMode::OAuth => {
                if self.client_id.is_empty() {
                    bail!("oauth mode needs a client id — set nuheat.auth.client_id");
                }
                if self.redirect_uri.is_empty() {
                    bail!("oauth mode needs a redirect uri — set nuheat.auth.redirect_uri");
                }
                format!(
                    "{IDENTITY_BASE}/connect/authorize?client_id={}&response_type=code\
                     &scope={}&redirect_uri={}&state={state}\
                     &code_challenge={challenge}&code_challenge_method=S256",
                    urlencoding::encode(&self.client_id),
                    urlencoding::encode("openid openapi offline_access"),
                    urlencoding::encode(&self.redirect_uri),
                )
            }
        };

        Ok(PendingAuthorization {
            url,
            verifier,
            state,
        })
    }

    /// Take what the operator pasted and turn it into stored tokens.
    ///
    /// Accepts either half of the problem: in implicit mode they paste the
    /// token (or the whole redirect URL containing it), and in oauth mode the
    /// authorization code (or the redirect URL containing that).
    pub async fn complete_authorization(
        &self,
        pasted: &str,
        pending: &PendingAuthorization,
    ) -> Result<()> {
        let pasted = pasted.trim();
        if pasted.is_empty() {
            bail!("nothing was pasted");
        }
        match self.mode {
            AuthMode::AccessToken => {
                let (token, expires_in) = parse_implicit_response(pasted)
                    .ok_or_else(|| anyhow!("could not find an access_token in what was pasted"))?;
                let tokens = Tokens {
                    access_token: Some(token),
                    expires_at: expires_in.map(|s| Utc::now() + Duration::seconds(s)),
                    refresh_token: None,
                };
                self.store(tokens).await
            }
            AuthMode::OAuth => {
                // If the paste is a whole redirect URL it carries the `state`
                // we sent, and it has to match. This is the CSRF check the
                // authorization-code flow is specified with: without it, a link
                // an attacker crafted could hand this plugin *their* code and
                // bind the operator's homeCore to the attacker's NuHeat
                // account. A bare code carries no state and cannot be checked,
                // which is the operator's own deliberate paste and is allowed.
                if let Some(returned) = extract_query_param(pasted, "state") {
                    if returned != pending.state {
                        bail!(
                            "the sign-in did not come back with the value it was sent \
                             (state mismatch) — start the link again rather than reusing an \
                             old link"
                        );
                    }
                }
                let code =
                    extract_query_param(pasted, "code").unwrap_or_else(|| pasted.to_string());
                self.exchange_code(&code, &pending.verifier).await
            }
        }
    }

    async fn exchange_code(&self, code: &str, verifier: &str) -> Result<()> {
        let mut form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", self.redirect_uri.clone()),
            ("client_id", self.client_id.clone()),
            ("code_verifier", verifier.to_string()),
        ];
        if let Some(secret) = &self.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        let tokens = self.post_token(&form).await?;
        self.store(tokens).await
    }

    async fn refresh(&self, refresh_token: &str) -> Result<String> {
        let mut form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token.to_string()),
            ("client_id", self.client_id.clone()),
        ];
        if let Some(secret) = &self.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        let mut tokens = self.post_token(&form).await?;
        // A rotating server returns a new refresh token and retires the old
        // one; a non-rotating one omits it and the existing token stays valid.
        // Dropping the old one on an omission would un-link the plugin at the
        // next restart.
        if tokens.refresh_token.is_none() {
            tokens.refresh_token = Some(refresh_token.to_string());
        }
        let access = tokens
            .access_token
            .clone()
            .ok_or_else(|| anyhow!("token endpoint returned no access_token"))?;
        self.store(tokens).await?;
        info!("Renewed the NuHeat access token");
        Ok(access)
    }

    async fn post_token(&self, form: &[(&str, String)]) -> Result<Tokens> {
        // Encoded by hand rather than with reqwest's `form`: the workspace
        // builds reqwest with default features off, and this keeps the token
        // request from depending on which of them are enabled.
        let body = form
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        let response = self
            .http
            .post(format!("{IDENTITY_BASE}/connect/token"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .context("reaching the NuHeat identity server")?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            // `error` is OAuth2's own machine-readable reason —
            // `invalid_grant` for an expired refresh token,
            // `unauthorized_client` for a grant this client id may not use.
            // Surfacing it verbatim is the difference between "re-link" and
            // "your client id does not permit this flow".
            let reason = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_owned))
                .unwrap_or_else(|| status.to_string());
            bail!("the identity server refused the request: {reason}");
        }

        let parsed: TokenResponse =
            serde_json::from_str(&body).context("parsing the token response")?;
        Ok(Tokens {
            access_token: Some(parsed.access_token),
            expires_at: parsed.expires_in.map(|s| Utc::now() + Duration::seconds(s)),
            refresh_token: parsed.refresh_token,
        })
    }

    /// Hold the tokens in memory and mirror them to core's learned state.
    ///
    /// The in-memory update happens whether or not the persist succeeds: a
    /// broker hiccup should cost the *next restart* its credentials, not this
    /// running process the token it just obtained.
    async fn store(&self, tokens: Tokens) -> Result<()> {
        let delta = {
            let mut guard = self.tokens.lock().expect("token mutex");
            *guard = tokens;
            guard.to_state()
        };
        if let Err(e) = self.writer.persist(&delta).await {
            warn!(error = %e, "Could not persist NuHeat tokens; re-linking will be needed after a restart");
        }
        Ok(())
    }

    /// Forget everything, in memory and in core's learned state.
    pub async fn sign_out(&self) -> Result<()> {
        self.store(Tokens::default()).await
    }
}

/// Pull `access_token` (and `expires_in`) out of whatever the operator pasted.
///
/// Implicit responses arrive in the URL *fragment*, so the realistic pastes are
/// the bare token, `#access_token=...&expires_in=3600&...`, or the entire
/// redirect URL. All three are accepted rather than making them trim it — a
/// mis-trimmed token fails as an opaque 401 an hour of confusion later.
fn parse_implicit_response(pasted: &str) -> Option<(String, Option<i64>)> {
    let fragment = pasted
        .split_once('#')
        .map(|(_, f)| f)
        .unwrap_or(pasted)
        .trim_start_matches('#');

    if fragment.contains("access_token=") {
        let mut token = None;
        let mut expires = None;
        for pair in fragment.split('&') {
            match pair.split_once('=') {
                Some(("access_token", v)) => {
                    token = urlencoding::decode(v).ok().map(|s| s.into_owned())
                }
                Some(("expires_in", v)) => expires = v.parse::<i64>().ok(),
                _ => {}
            }
        }
        return token.map(|t| (t, expires));
    }

    // A bare token. JWTs are three base64url segments; anything with a space
    // or a slash-prefixed scheme is a paste of something else entirely.
    let candidate = pasted.trim();
    let looks_like_a_token = !candidate.is_empty()
        && !candidate.contains(char::is_whitespace)
        && !candidate.starts_with("http");
    looks_like_a_token.then(|| (candidate.to_string(), None))
}

/// Pull one query parameter out of a redirect URL, for the pasted `code`.
fn extract_query_param(pasted: &str, name: &str) -> Option<String> {
    let query = pasted.split_once('?').map(|(_, q)| q)?;
    let query = query.split('#').next().unwrap_or(query);
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| urlencoding::decode(v).ok().map(|s| s.into_owned()))?
    })
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// `bytes` of CSPRNG output, base64url-encoded.
///
/// `rand::rng()` is the OS-seeded CSPRNG that `thread_rng()` was renamed to —
/// which is what a PKCE verifier and an OAuth `state` both require, since both
/// are there to make a value unguessable to an attacker.
fn random_urlsafe(bytes: usize) -> String {
    use rand::RngExt;
    let mut rng: rand::rngs::ThreadRng = rand::rng();
    let buf: Vec<u8> = (0..bytes)
        .map(|_| rng.random_range(0..=255u32) as u8)
        .collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(mode: AuthMode) -> Auth {
        Auth::new(
            mode,
            Some("my-client".into()),
            None,
            Some("http://localhost:9/cb".into()),
            reqwest::Client::new(),
            PluginStateWriter::test_instance("plugin.nuheat"),
        )
    }

    #[test]
    fn a_pasted_fragment_yields_the_token_and_its_lifetime() {
        let pasted = "https://api.mynuheat.com/swagger/oauth2-redirect.html#access_token=abc.def.ghi&token_type=Bearer&expires_in=3600&scope=openapi";
        let (token, expires) = parse_implicit_response(pasted).expect("parses");
        assert_eq!(token, "abc.def.ghi");
        assert_eq!(expires, Some(3600));
    }

    #[test]
    fn a_bare_token_is_accepted_too() {
        let (token, expires) = parse_implicit_response("  abc.def.ghi  ").expect("parses");
        assert_eq!(token, "abc.def.ghi");
        assert_eq!(expires, None);
    }

    #[test]
    fn a_paste_that_is_not_a_token_is_refused() {
        assert!(parse_implicit_response("").is_none());
        assert!(parse_implicit_response("https://example.com/no/token/here").is_none());
        assert!(parse_implicit_response("two words").is_none());
    }

    #[test]
    fn an_authorization_code_is_found_in_a_redirect_url() {
        let url = "http://localhost:9/cb?code=THE-CODE&scope=openapi&state=xyz";
        assert_eq!(
            extract_query_param(url, "code").as_deref(),
            Some("THE-CODE")
        );
        assert_eq!(extract_query_param(url, "nope"), None);
    }

    /// The public client only works with NuHeat's own registered redirect, so
    /// an operator's config values must not be able to break it.
    #[test]
    fn access_token_mode_ignores_configured_client_details() {
        let a = auth(AuthMode::AccessToken);
        let pending = a.begin_authorization().expect("builds");
        assert!(pending.url.contains("client_id=swagger"), "{}", pending.url);
        assert!(
            pending.url.contains("response_type=token"),
            "{}",
            pending.url
        );
        // offline_access here is rejected by the identity server, so asking for
        // it would turn a working link into an error page.
        assert!(!pending.url.contains("offline_access"), "{}", pending.url);
    }

    #[test]
    fn oauth_mode_asks_for_a_refresh_token_and_uses_pkce() {
        let a = auth(AuthMode::OAuth);
        let pending = a.begin_authorization().expect("builds");
        assert!(
            pending.url.contains("response_type=code"),
            "{}",
            pending.url
        );
        assert!(pending.url.contains("offline_access"), "{}", pending.url);
        assert!(
            pending.url.contains("code_challenge_method=S256"),
            "{}",
            pending.url
        );
        assert_eq!(pkce_challenge(&pending.verifier).len(), 43);
    }

    #[test]
    fn oauth_mode_without_a_client_id_says_so_rather_than_building_a_broken_url() {
        let a = Auth::new(
            AuthMode::OAuth,
            None,
            None,
            Some("http://localhost:9/cb".into()),
            reqwest::Client::new(),
            PluginStateWriter::test_instance("plugin.nuheat"),
        );
        let err = a.begin_authorization().expect_err("no client id");
        assert!(err.to_string().contains("client id"), "{err}");
    }

    /// The failure this guards is losing the account on a restart: a
    /// non-rotating server omits `refresh_token` from a refresh response, and
    /// storing that verbatim would blank the one we still need.
    /// The CSRF check: a redirect that came back with someone else's `state`
    /// is refused rather than exchanged.
    #[test]
    fn a_mismatched_state_is_refused() {
        let a = auth(AuthMode::OAuth);
        let pending = a.begin_authorization().expect("builds");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(a.complete_authorization(
                "http://localhost:9/cb?code=THE-CODE&state=not-the-one-we-sent",
                &pending,
            ))
            .expect_err("state mismatch");
        assert!(err.to_string().contains("state mismatch"), "{err}");
    }

    /// A bare code has no state to check and is the operator's own paste.
    /// It must not be blocked by the check above — it reaches the token
    /// endpoint, which is where an invalid code is rejected.
    #[test]
    fn a_bare_code_is_not_blocked_by_the_state_check() {
        let a = auth(AuthMode::OAuth);
        let pending = a.begin_authorization().expect("builds");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(a.complete_authorization("THE-CODE", &pending))
            .expect_err("no server to exchange against in a test");
        assert!(
            !err.to_string().contains("state mismatch"),
            "a bare code must not be refused as a state mismatch: {err}"
        );
    }

    #[test]
    fn tokens_round_trip_through_the_learned_state_document() {
        let tokens = Tokens {
            access_token: Some("at".into()),
            expires_at: Some(Utc::now()),
            refresh_token: Some("rt".into()),
        };
        let restored = Tokens::from_state(&tokens.to_state());
        assert_eq!(restored.access_token.as_deref(), Some("at"));
        assert_eq!(restored.refresh_token.as_deref(), Some("rt"));
        assert!(restored.expires_at.is_some());
    }

    #[test]
    fn a_token_inside_the_renewal_margin_is_not_considered_fresh() {
        let nearly = Tokens {
            access_token: Some("at".into()),
            expires_at: Some(Utc::now() + Duration::minutes(1)),
            refresh_token: None,
        };
        assert!(!nearly.is_fresh());

        let good = Tokens {
            access_token: Some("at".into()),
            expires_at: Some(Utc::now() + Duration::minutes(30)),
            refresh_token: None,
        };
        assert!(good.is_fresh());
    }

    /// Restoring must not clobber a token this process just obtained — core
    /// echoes the retained document back after every write.
    #[test]
    fn the_retained_echo_does_not_overwrite_a_newer_token() {
        let a = auth(AuthMode::OAuth);
        {
            let mut guard = a.tokens.lock().unwrap();
            guard.access_token = Some("new".into());
        }
        a.adopt_persisted_state(&json!({"auth": {"access_token": "stale"}}));
        assert_eq!(
            a.tokens.lock().unwrap().access_token.as_deref(),
            Some("new")
        );
    }

    #[test]
    fn a_restart_picks_up_the_persisted_tokens() {
        let a = auth(AuthMode::OAuth);
        a.adopt_persisted_state(&json!({"auth": {"refresh_token": "rt"}}));
        assert!(a.is_linked());
    }

    #[test]
    fn an_unlinked_oauth_plugin_names_the_action_that_fixes_it() {
        let a = auth(AuthMode::OAuth);
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(a.bearer())
            .expect_err("not linked");
        assert!(err.to_string().contains("Link NuHeat account"), "{err}");
    }
}
