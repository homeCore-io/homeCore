//! Getting, and keeping, a bearer token for the NuHeat OpenAPI.
//!
//! Every endpoint is `security: [{oauth2: []}]`. The old session API older
//! integrations used (`POST /api/authenticate/user`, email and password, no
//! client id) lived on `www.mynuheat.com`, which is decommissioned, so OAuth2
//! against `identity.mynuheat.com` is the only way in.
//!
//! ## Whose client id
//!
//! **homeCore's.** NuHeat issued homeCore one client (2026-11-01):
//! [`CLIENT_ID`], authorization code with PKCE (S256), scopes `openid openapi
//! offline_access`, no client secret. Everyone signs in under it with their
//! own NuHeat account, as with Hue Remote or Ecobee. A PKCE client has no
//! secret to keep, so shipping the id in an open-source plugin is what it is
//! for.
//!
//! An operator who has their own NuHeat client can still use it:
//! `[nuheat.auth] client_id` (and `client_secret` for a confidential one)
//! override the built-in id. Nothing else changes.
//!
//! ## Where the browser comes back
//!
//! NuHeat registers exact redirect URIs. homeCore has two:
//!
//! - [`RELAY_REDIRECT`], a static page on homecore.io. It reads `state`, finds
//!   the address of the person's own homeCore in it, and sends the browser
//!   back there with the `code`. That works wherever homeCore runs (a server,
//!   Docker) and from any phone or laptop.
//! - [`LOOPBACK_REDIRECT`], which nothing listens on. The person copies the
//!   address the browser shows and pastes it into the plugin's page.
//!
//! The code is useless on its own: exchanging it needs the PKCE verifier,
//! which never leaves this process. That is what makes a public relay page
//! safe to route it through.
//!
//! ## `state`
//!
//! Random, single use, good for ten minutes, and checked on the way back. It
//! may carry the address to return to: `<nonce>.<base64url(address)>`. The
//! relay page needs it; this plugin only ever compares the whole string with
//! the one it sent.
//!
//! ## Refresh tokens rotate
//!
//! NuHeat's refresh tokens are one-time use: each refresh returns a new one
//! and retires the old. So a refresh is serialised (two at once would spend
//! the same token, and the loser would un-link the plugin), and the new token
//! is persisted before the new access token is used. They also have an
//! absolute lifetime of 30 days from sign-in ([`REFRESH_LIFETIME`]), whatever
//! the use, so [`Auth::relink_due`] says when the person has to sign in again,
//! and the plugin warns ahead of it.
//!
//! ## Where the tokens live
//!
//! In core's durable learned state (`homecore/plugins/{id}/state`), via
//! [`PluginStateWriter`], not the config file. The config file is core-owned
//! and watched, so writing a token into it would restart the plugin mid-flow.
//! hc-hue keeps its bridge key the same way.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use plugin_sdk_rs::PluginStateWriter;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// The client NuHeat issued homeCore: authorization code + PKCE, no secret.
pub const CLIENT_ID: &str = "homeCore-22UyU-56Kt-IoP22";
/// The relay page on homecore.io, registered against [`CLIENT_ID`].
pub const RELAY_REDIRECT: &str = "https://homecore.io/oauth/nuheat/callback";
/// The loopback address, also registered. Nothing listens on it.
pub const LOOPBACK_REDIRECT: &str = "http://127.0.0.1:8127/nuheat/callback";

pub const IDENTITY_BASE: &str = "https://identity.mynuheat.com";
pub const API_BASE: &str = "https://api.mynuheat.com";

/// `offline_access` is what returns a refresh token.
const SCOPES: &str = "openid openapi offline_access";

/// Renew this long before the access token expires, so a poll that starts
/// just inside the deadline doesn't 401 on its way to the API.
const RENEW_MARGIN: Duration = Duration::minutes(5);

/// NuHeat's absolute refresh-token lifetime: 30 days from signing in, however
/// often the token is used.
pub const REFRESH_LIFETIME: Duration = Duration::days(30);

/// Say a new sign-in is due this long before it is.
pub const RELINK_WARNING: Duration = Duration::days(5);

/// How long a sign-in that was started may take to come back.
const PENDING_TTL: Duration = Duration::minutes(10);

/// The longest return address `state` will carry.
const MAX_RETURN_LEN: usize = 512;

/// How this plugin signs in: homeCore's client, unless the operator set their
/// own, and where NuHeat's identity server is.
#[derive(Debug, Clone, Default)]
pub struct AuthSettings {
    /// An operator's own client id. Empty means [`CLIENT_ID`].
    pub client_id: Option<String>,
    /// Only for an operator's own confidential client.
    pub client_secret: Option<String>,
    /// Replaces [`RELAY_REDIRECT`]: for an operator's own client, or a test.
    pub redirect_uri: Option<String>,
    /// Replaces [`LOOPBACK_REDIRECT`].
    pub loopback_redirect_uri: Option<String>,
    /// Replaces [`IDENTITY_BASE`]: a stand-in, in tests.
    pub identity_base: Option<String>,
}

/// The tokens, as they are persisted and restored.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tokens {
    pub access_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub refresh_token: Option<String>,
    /// When the person signed in: the refresh token's 30 days count from here.
    pub linked_at: Option<DateTime<Utc>>,
    /// Their NuHeat account's name, as the API said when they linked.
    pub account: Option<String>,
}

fn parse_time(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

impl Tokens {
    fn from_state(state: &Value) -> Self {
        let auth = &state["auth"];
        Self {
            access_token: auth["access_token"].as_str().map(str::to_owned),
            expires_at: parse_time(&auth["expires_at"]),
            refresh_token: auth["refresh_token"].as_str().map(str::to_owned),
            linked_at: parse_time(&auth["linked_at"]),
            account: auth["account"].as_str().map(str::to_owned),
        }
    }

    /// The whole `auth` object, every key present, so one write replaces it.
    fn to_state(&self) -> Value {
        json!({
            "auth": {
                "access_token": self.access_token,
                "expires_at": self.expires_at.map(|t| t.to_rfc3339()),
                "refresh_token": self.refresh_token,
                "linked_at": self.linked_at.map(|t| t.to_rfc3339()),
                "account": self.account,
            }
        })
    }

    fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        match (&self.access_token, self.expires_at) {
            (Some(t), Some(exp)) if !t.is_empty() => exp - now > RENEW_MARGIN,
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

/// Which registered address the browser comes back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// The relay page on homecore.io, which sends the browser home.
    Relay,
    /// The loopback address: the person copies the address bar.
    Loopback,
}

impl Via {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("loopback") => Self::Loopback,
            _ => Self::Relay,
        }
    }
}

/// A sign-in that was started and hasn't come back yet.
#[derive(Debug, Clone)]
struct Pending {
    state: String,
    verifier: String,
    redirect_uri: String,
    started: DateTime<Utc>,
}

/// What [`Auth::begin`] hands the plugin's page.
#[derive(Debug, Clone)]
pub struct Begun {
    /// Where to send the browser to sign in at NuHeat.
    pub url: String,
    pub redirect_uri: String,
    pub state: String,
}

/// What came back from NuHeat: a code and its state, or everything pasted.
#[derive(Debug, Clone, Default)]
pub struct Returned {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

impl Returned {
    /// From what someone pasted: the address they landed on, or a bare code.
    pub fn from_pasted(pasted: &str) -> Self {
        let pasted = pasted.trim();
        if pasted.contains('?') || pasted.contains("code=") || pasted.contains("error=") {
            Self {
                code: query_param(pasted, "code"),
                state: query_param(pasted, "state"),
                error: query_param(pasted, "error"),
            }
        } else {
            Self {
                code: (!pasted.is_empty()).then(|| pasted.to_string()),
                ..Self::default()
            }
        }
    }
}

pub struct Auth {
    client_id: String,
    client_secret: Option<String>,
    own_client: bool,
    relay_redirect: String,
    loopback_redirect: String,
    identity_base: String,
    http: reqwest::Client,
    tokens: Arc<Mutex<Tokens>>,
    pending: Mutex<Option<Pending>>,
    /// One refresh at a time: refresh tokens are spent by using them.
    refreshing: tokio::sync::Mutex<()>,
    writer: PluginStateWriter,
}

fn trimmed(v: &Option<String>) -> Option<String> {
    v.as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

impl Auth {
    pub fn new(settings: &AuthSettings, http: reqwest::Client, writer: PluginStateWriter) -> Self {
        let own = trimmed(&settings.client_id);
        Self {
            own_client: own.is_some(),
            client_id: own.unwrap_or_else(|| CLIENT_ID.to_string()),
            client_secret: trimmed(&settings.client_secret),
            relay_redirect: trimmed(&settings.redirect_uri)
                .unwrap_or_else(|| RELAY_REDIRECT.to_string()),
            loopback_redirect: trimmed(&settings.loopback_redirect_uri)
                .unwrap_or_else(|| LOOPBACK_REDIRECT.to_string()),
            identity_base: trimmed(&settings.identity_base)
                .unwrap_or_else(|| IDENTITY_BASE.to_string())
                .trim_end_matches('/')
                .to_string(),
            http,
            tokens: Arc::new(Mutex::new(Tokens::default())),
            pending: Mutex::new(None),
            refreshing: tokio::sync::Mutex::new(()),
            writer,
        }
    }

    /// Whether the operator set a client of their own.
    pub fn uses_own_client(&self) -> bool {
        self.own_client
    }

    /// Adopt tokens from core's retained learned-state document.
    ///
    /// Fires on connect and on every later change. Only an empty memory takes
    /// it: the retained document echoes our own writes, and adopting an echo
    /// could put back a refresh token that has since been spent.
    pub fn adopt_persisted_state(&self, state: &Value) {
        let restored = Tokens::from_state(state);
        if restored.refresh_token.is_none() && restored.access_token.is_none() {
            return;
        }
        let mut guard = self.tokens.lock().expect("token mutex");
        if guard.access_token.is_none() && guard.refresh_token.is_none() {
            *guard = restored;
            info!("Restored NuHeat tokens from core's learned state");
        }
    }

    /// Start a sign-in: the address to open at NuHeat.
    ///
    /// `return_to` is the address of this homeCore's page for the plugin; the
    /// relay page sends the browser back there. It is only used with
    /// [`Via::Relay`], and only if it is an http(s) address.
    pub fn begin(&self, via: Via, return_to: Option<&str>) -> Result<Begun> {
        let redirect_uri = match via {
            Via::Relay => self.relay_redirect.clone(),
            Via::Loopback => self.loopback_redirect.clone(),
        };
        let nonce = random_urlsafe(24);
        let state = match (via, return_to.and_then(return_address)) {
            (Via::Relay, Some(back)) => format!(
                "{nonce}.{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(back.as_bytes())
            ),
            _ => nonce,
        };
        let verifier = random_urlsafe(48);
        let challenge = pkce_challenge(&verifier);
        let url = format!(
            "{}/connect/authorize?client_id={}&response_type=code&scope={}&redirect_uri={}\
             &state={}&code_challenge={challenge}&code_challenge_method=S256",
            self.identity_base,
            urlencoding::encode(&self.client_id),
            urlencoding::encode(SCOPES),
            urlencoding::encode(&redirect_uri),
            urlencoding::encode(&state),
        );
        *self.pending.lock().expect("pending mutex") = Some(Pending {
            state: state.clone(),
            verifier,
            redirect_uri: redirect_uri.clone(),
            started: Utc::now(),
        });
        Ok(Begun {
            url,
            redirect_uri,
            state,
        })
    }

    /// Finish a sign-in from what came back, and keep the tokens.
    ///
    /// The sign-in that was started is used up whatever happens: a second try
    /// starts again.
    pub async fn complete(&self, back: &Returned) -> Result<()> {
        if let Some(error) = &back.error {
            // `access_denied` is the person pressing No on NuHeat's consent page.
            self.pending.lock().expect("pending mutex").take();
            bail!("NuHeat said {error}. Nothing was linked.");
        }
        let code = back
            .code
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| anyhow!("there was no code in that"))?;
        let pending = self
            .pending
            .lock()
            .expect("pending mutex")
            .take()
            .ok_or_else(|| anyhow!("no sign-in is waiting; start it again"))?;
        if Utc::now() - pending.started > PENDING_TTL {
            bail!("that sign-in took over ten minutes; start it again");
        }
        // A bare code has no state to check, and needs none: NuHeat bound it
        // to this sign-in's PKCE challenge, so no other code can be swapped in.
        if let Some(state) = &back.state {
            if *state != pending.state {
                bail!("that came back from a different sign-in; start it again");
            }
        }
        let mut form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", pending.redirect_uri.clone()),
            ("client_id", self.client_id.clone()),
            ("code_verifier", pending.verifier.clone()),
        ];
        if let Some(secret) = &self.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        let mut tokens = self.post_token(&form).await?;
        if tokens.refresh_token.is_none() {
            // Without one the plugin stops after an hour: worth saying now.
            warn!("NuHeat returned no refresh token; the plugin will need signing in again within the hour");
        }
        tokens.linked_at = Some(Utc::now());
        self.store(tokens).await;
        Ok(())
    }

    /// Record the account's name once the API has answered with it.
    pub async fn set_account(&self, name: Option<String>) {
        let tokens = {
            let mut guard = self.tokens.lock().expect("token mutex");
            guard.account = name;
            guard.clone()
        };
        self.persist(&tokens).await;
    }

    /// A bearer token good for the next request, renewing it if need be.
    pub async fn bearer(&self) -> Result<String> {
        if let Some(t) = self.fresh_token() {
            return Ok(t);
        }
        // Serialised, then checked again: whoever waited may find the token
        // the one before them just fetched.
        let _one_at_a_time = self.refreshing.lock().await;
        if let Some(t) = self.fresh_token() {
            return Ok(t);
        }
        let refresh_token = self
            .tokens
            .lock()
            .expect("token mutex")
            .refresh_token
            .clone();
        match refresh_token {
            Some(rt) if !rt.is_empty() => self.refresh(&rt).await,
            _ => bail!("not linked to a NuHeat account"),
        }
    }

    fn fresh_token(&self) -> Option<String> {
        let guard = self.tokens.lock().expect("token mutex");
        guard
            .is_fresh(Utc::now())
            .then(|| guard.access_token.clone())
            .flatten()
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
        let mut tokens = match self.post_token(&form).await {
            Ok(t) => t,
            Err(e) if e.to_string().contains("invalid_grant") => {
                // Spent, revoked or past its 30 days: it will never work again.
                self.store(Tokens::default()).await;
                bail!("NuHeat ended this sign-in ({e}); link the account again");
            }
            Err(e) => return Err(e).context("renewing the access token"),
        };
        let kept = self.tokens.lock().expect("token mutex").clone();
        // A server that doesn't rotate omits the refresh token, and the old
        // one stays good. Dropping it would un-link the plugin.
        if tokens.refresh_token.is_none() {
            tokens.refresh_token = Some(refresh_token.to_string());
        }
        tokens.linked_at = kept.linked_at;
        tokens.account = kept.account;
        let access = tokens
            .access_token
            .clone()
            .ok_or_else(|| anyhow!("the token endpoint returned no access token"))?;
        self.store(tokens).await;
        info!("Renewed the NuHeat access token");
        Ok(access)
    }

    async fn post_token(&self, form: &[(&str, String)]) -> Result<Tokens> {
        // Encoded by hand: the workspace builds reqwest without the `form`
        // feature.
        let body = form
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let response = self
            .http
            .post(format!("{}/connect/token", self.identity_base))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .context("reaching the NuHeat identity server")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            // OAuth2's own reason: `invalid_grant` for a spent or expired
            // refresh token, `invalid_client` for a client NuHeat set up
            // differently from how it was described.
            let reason = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_owned))
                .unwrap_or_else(|| status.to_string());
            bail!("the identity server refused: {reason}");
        }
        let parsed: TokenResponse =
            serde_json::from_str(&body).context("reading the token response")?;
        Ok(Tokens {
            access_token: Some(parsed.access_token),
            expires_at: parsed.expires_in.map(|s| Utc::now() + Duration::seconds(s)),
            refresh_token: parsed.refresh_token,
            linked_at: None,
            account: None,
        })
    }

    /// Persist the tokens, then use them.
    ///
    /// Persisted first: once NuHeat has issued a new refresh token the old one
    /// is spent, so a restart that found only the old one would be signed out.
    /// Kept in memory even if persisting fails, because the old token can't be
    /// put back; that costs the next restart a sign-in, not this run.
    async fn store(&self, tokens: Tokens) {
        self.persist(&tokens).await;
        *self.tokens.lock().expect("token mutex") = tokens;
    }

    async fn persist(&self, tokens: &Tokens) {
        if let Err(e) = self.writer.persist(&tokens.to_state()).await {
            warn!(error = %e, "Could not keep the NuHeat sign-in; it will be needed again after a restart");
        }
    }

    /// Forget the sign-in, in memory and in core's learned state.
    pub async fn sign_out(&self) {
        self.pending.lock().expect("pending mutex").take();
        self.store(Tokens::default()).await;
    }

    pub fn is_linked(&self) -> bool {
        let guard = self.tokens.lock().expect("token mutex");
        guard.refresh_token.is_some() || guard.access_token.is_some()
    }

    pub fn account(&self) -> Option<String> {
        self.tokens.lock().expect("token mutex").account.clone()
    }

    /// How long the current access token has left.
    pub fn expires_in(&self) -> Option<Duration> {
        let guard = self.tokens.lock().expect("token mutex");
        guard.expires_at.map(|exp| exp - Utc::now())
    }

    pub fn linked_at(&self) -> Option<DateTime<Utc>> {
        self.tokens.lock().expect("token mutex").linked_at
    }

    /// When the person has to sign in again: 30 days after they last did.
    pub fn relink_due(&self) -> Option<DateTime<Utc>> {
        let guard = self.tokens.lock().expect("token mutex");
        guard.refresh_token.as_ref()?;
        guard.linked_at.map(|t| t + REFRESH_LIFETIME)
    }

    /// Whether that is close enough to say so.
    pub fn relink_soon(&self, now: DateTime<Utc>) -> bool {
        self.relink_due()
            .is_some_and(|due| due - now < RELINK_WARNING)
    }

    #[cfg(test)]
    fn set_tokens(&self, tokens: Tokens) {
        *self.tokens.lock().expect("token mutex") = tokens;
    }

    #[cfg(test)]
    fn tokens(&self) -> Tokens {
        self.tokens.lock().expect("token mutex").clone()
    }
}

/// The return address `state` may carry: an http(s) URL, not too long.
fn return_address(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.len() > MAX_RETURN_LEN {
        return None;
    }
    let url = reqwest::Url::parse(raw).ok()?;
    matches!(url.scheme(), "http" | "https")
        .then(|| url.host_str().is_some())
        .filter(|&has_host| has_host)
        .map(|_| url.to_string())
}

/// The return address inside a `state`, as the relay page reads it.
#[cfg(test)]
fn return_address_in(state: &str) -> Option<String> {
    let (_, packed) = state.split_once('.')?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(packed)
        .ok()?;
    return_address(&String::from_utf8(bytes).ok()?)
}

/// One query parameter of a pasted address.
fn query_param(pasted: &str, name: &str) -> Option<String> {
    let query = pasted.split_once('?').map(|(_, q)| q).unwrap_or(pasted);
    let query = query.split('#').next().unwrap_or(query);
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| {
            urlencoding::decode(&v.replace('+', " "))
                .ok()
                .map(|s| s.into_owned())
        })?
    })
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// `bytes` of the OS-seeded CSPRNG, base64url: what a PKCE verifier and a
/// `state` need to be unguessable.
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn auth_with(settings: AuthSettings) -> Auth {
        Auth::new(
            &settings,
            reqwest::Client::new(),
            PluginStateWriter::test_instance("plugin.nuheat"),
        )
    }

    fn auth() -> Auth {
        auth_with(AuthSettings::default())
    }

    fn param(url: &str, name: &str) -> Option<String> {
        query_param(url, name)
    }

    /// A stand-in token endpoint: answers each request with the next body,
    /// and counts the requests and keeps their bodies.
    async fn token_server(answers: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        let next = Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 16384];
                let mut got = 0;
                // Read until the body the headers promise has arrived.
                loop {
                    let n = sock.read(&mut buf[got..]).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    got += n;
                    let text = String::from_utf8_lossy(&buf[..got]).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if body.len() >= len {
                            seen2.lock().unwrap().push(body.to_string());
                            break;
                        }
                    }
                }
                let i = next.fetch_add(1, Ordering::SeqCst);
                let (status, body) = answers.get(i).cloned().unwrap_or((500, "{}".to_string()));
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        (base, seen)
    }

    #[test]
    fn signs_in_as_homecore_with_pkce_and_no_secret() {
        let b = auth().begin(Via::Relay, None).unwrap();
        assert!(b
            .url
            .starts_with("https://identity.mynuheat.com/connect/authorize?"));
        assert_eq!(param(&b.url, "client_id").as_deref(), Some(CLIENT_ID));
        assert_eq!(param(&b.url, "response_type").as_deref(), Some("code"));
        assert_eq!(
            param(&b.url, "code_challenge_method").as_deref(),
            Some("S256")
        );
        assert!(param(&b.url, "scope").unwrap().contains("offline_access"));
        assert_eq!(
            param(&b.url, "redirect_uri").as_deref(),
            Some(RELAY_REDIRECT)
        );
        assert!(!b.url.contains("secret"));
        assert!(!auth().uses_own_client());
    }

    #[test]
    fn the_challenge_is_the_verifiers_sha256() {
        // RFC 7636 appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn an_own_client_replaces_homecores_and_the_loopback_is_the_other_address() {
        let a = auth_with(AuthSettings {
            client_id: Some("  mine  ".into()),
            ..AuthSettings::default()
        });
        assert!(a.uses_own_client());
        let b = a
            .begin(Via::Loopback, Some("https://home.example/x"))
            .unwrap();
        assert_eq!(param(&b.url, "client_id").as_deref(), Some("mine"));
        assert_eq!(b.redirect_uri, LOOPBACK_REDIRECT);
        // The loopback never carries a return address: nothing would read it.
        assert!(!b.state.contains('.'));
    }

    #[test]
    fn state_is_random_and_carries_the_return_address_for_the_relay() {
        let a = auth();
        let one = a
            .begin(
                Via::Relay,
                Some("http://192.168.1.20:3000/settings/plugins/plugin.nuheat"),
            )
            .unwrap();
        let two = a
            .begin(
                Via::Relay,
                Some("http://192.168.1.20:3000/settings/plugins/plugin.nuheat"),
            )
            .unwrap();
        assert_ne!(one.state, two.state);
        assert_eq!(
            return_address_in(&one.state).as_deref(),
            Some("http://192.168.1.20:3000/settings/plugins/plugin.nuheat")
        );
        assert_eq!(
            param(&one.url, "state").as_deref(),
            Some(one.state.as_str())
        );
    }

    #[test]
    fn only_an_http_address_is_carried_back() {
        for bad in [
            "javascript:alert(1)",
            "data:text/html,x",
            "file:///etc/passwd",
            "not a url",
            "//evil",
        ] {
            let b = auth().begin(Via::Relay, Some(bad)).unwrap();
            assert!(!b.state.contains('.'), "{bad} was carried");
        }
        let long = format!("https://h.example/{}", "a".repeat(600));
        assert!(!auth()
            .begin(Via::Relay, Some(&long))
            .unwrap()
            .state
            .contains('.'));
    }

    #[test]
    fn what_was_pasted_is_read_as_an_address_or_a_bare_code() {
        let r = Returned::from_pasted(
            "http://127.0.0.1:8127/nuheat/callback?code=C%2B1&scope=openid&state=S1",
        );
        assert_eq!(r.code.as_deref(), Some("C+1"));
        assert_eq!(r.state.as_deref(), Some("S1"));
        let r = Returned::from_pasted("  THECODE ");
        assert_eq!(r.code.as_deref(), Some("THECODE"));
        assert_eq!(r.state, None);
        let r = Returned::from_pasted(
            "https://homecore.io/oauth/nuheat/callback?error=access_denied&state=S",
        );
        assert_eq!(r.error.as_deref(), Some("access_denied"));
    }

    #[tokio::test]
    async fn a_code_is_exchanged_with_the_verifier_and_the_sign_in_is_kept() {
        let (base, seen) = token_server(vec![(
            200,
            r#"{"access_token":"A1","refresh_token":"R1","expires_in":3600}"#.into(),
        )])
        .await;
        let a = auth_with(AuthSettings {
            identity_base: Some(base),
            ..AuthSettings::default()
        });
        let b = a.begin(Via::Relay, None).unwrap();
        a.complete(&Returned {
            code: Some("CODE".into()),
            state: Some(b.state.clone()),
            error: None,
        })
        .await
        .unwrap();
        let body = seen.lock().unwrap()[0].clone();
        assert!(body.contains("grant_type=authorization_code"));
        assert!(body.contains("code_verifier="));
        assert!(body.contains(&format!("client_id={}", urlencoding::encode(CLIENT_ID))));
        assert!(!body.contains("client_secret"));
        let t = a.tokens();
        assert_eq!(t.refresh_token.as_deref(), Some("R1"));
        assert!(t.linked_at.is_some());
        let due = a.relink_due().unwrap();
        assert!((due - Utc::now() - REFRESH_LIFETIME).num_seconds().abs() < 5);
        assert_eq!(a.bearer().await.unwrap(), "A1");
    }

    #[tokio::test]
    async fn a_wrong_state_or_a_second_use_is_refused() {
        let (base, _) = token_server(vec![]).await;
        let a = auth_with(AuthSettings {
            identity_base: Some(base),
            ..AuthSettings::default()
        });
        let b = a.begin(Via::Relay, None).unwrap();
        let wrong = a
            .complete(&Returned {
                code: Some("C".into()),
                state: Some(format!("{}x", b.state)),
                error: None,
            })
            .await
            .unwrap_err();
        assert!(wrong.to_string().contains("different sign-in"), "{wrong}");
        // The sign-in was used up by that attempt.
        let again = a
            .complete(&Returned {
                code: Some("C".into()),
                state: Some(b.state),
                error: None,
            })
            .await
            .unwrap_err();
        assert!(again.to_string().contains("start it again"), "{again}");
    }

    #[tokio::test]
    async fn refusing_at_nuheat_links_nothing() {
        let a = auth();
        a.begin(Via::Relay, None).unwrap();
        let e = a
            .complete(&Returned::from_pasted("https://x/cb?error=access_denied"))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("access_denied"));
        assert!(!a.is_linked());
    }

    #[tokio::test]
    async fn a_rotated_refresh_token_replaces_the_spent_one_and_refreshes_run_one_at_a_time() {
        let (base, seen) = token_server(vec![(
            200,
            r#"{"access_token":"A2","refresh_token":"R2","expires_in":3600}"#.into(),
        )])
        .await;
        let a = Arc::new(auth_with(AuthSettings {
            identity_base: Some(base),
            ..AuthSettings::default()
        }));
        let linked = Utc::now() - Duration::days(3);
        a.set_tokens(Tokens {
            access_token: Some("A1".into()),
            expires_at: Some(Utc::now() - Duration::minutes(1)),
            refresh_token: Some("R1".into()),
            linked_at: Some(linked),
            account: Some("me@example.com".into()),
        });
        // Two callers at once: one refresh, both get the new token.
        let (x, y) = tokio::join!(a.bearer(), a.bearer());
        assert_eq!(x.unwrap(), "A2");
        assert_eq!(y.unwrap(), "A2");
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(seen.lock().unwrap()[0].contains("refresh_token=R1"));
        let t = a.tokens();
        assert_eq!(t.refresh_token.as_deref(), Some("R2"));
        // Refreshing doesn't move the 30 days.
        assert_eq!(t.linked_at, Some(linked));
        assert_eq!(t.account.as_deref(), Some("me@example.com"));
    }

    #[tokio::test]
    async fn a_spent_refresh_token_signs_the_plugin_out() {
        let (base, _) = token_server(vec![(400, r#"{"error":"invalid_grant"}"#.into())]).await;
        let a = auth_with(AuthSettings {
            identity_base: Some(base),
            ..AuthSettings::default()
        });
        a.set_tokens(Tokens {
            refresh_token: Some("R1".into()),
            linked_at: Some(Utc::now() - Duration::days(31)),
            ..Tokens::default()
        });
        let e = a.bearer().await.unwrap_err();
        assert!(e.to_string().contains("link the account again"), "{e}");
        assert!(!a.is_linked());
    }

    #[test]
    fn a_sign_in_is_due_again_thirty_days_after_it_was_made() {
        let a = auth();
        let now = Utc::now();
        a.set_tokens(Tokens {
            refresh_token: Some("R".into()),
            linked_at: Some(now - Duration::days(24)),
            ..Tokens::default()
        });
        assert!(!a.relink_soon(now));
        a.set_tokens(Tokens {
            refresh_token: Some("R".into()),
            linked_at: Some(now - Duration::days(26)),
            ..Tokens::default()
        });
        assert!(a.relink_soon(now));
        // Nothing is due when nothing is linked.
        a.set_tokens(Tokens::default());
        assert_eq!(a.relink_due(), None);
    }

    #[test]
    fn tokens_round_trip_through_the_learned_state_document() {
        let t = Tokens {
            access_token: Some("A".into()),
            expires_at: Some(Utc::now()),
            refresh_token: Some("R".into()),
            linked_at: Some(Utc::now()),
            account: Some("acct".into()),
        };
        let back = Tokens::from_state(&t.to_state());
        assert_eq!(back.refresh_token, t.refresh_token);
        assert_eq!(back.account, t.account);
        assert_eq!(
            back.linked_at.map(|x| x.timestamp()),
            t.linked_at.map(|x| x.timestamp())
        );
    }

    #[test]
    fn the_retained_echo_does_not_overwrite_a_newer_token() {
        let a = auth();
        a.set_tokens(Tokens {
            refresh_token: Some("R2".into()),
            ..Tokens::default()
        });
        a.adopt_persisted_state(&json!({ "auth": { "refresh_token": "R1" } }));
        assert_eq!(a.tokens().refresh_token.as_deref(), Some("R2"));
        let fresh = auth();
        fresh.adopt_persisted_state(&json!({ "auth": { "refresh_token": "R1" } }));
        assert_eq!(fresh.tokens().refresh_token.as_deref(), Some("R1"));
    }

    #[test]
    fn a_token_inside_the_renewal_margin_is_not_fresh() {
        let now = Utc::now();
        let t = Tokens {
            access_token: Some("A".into()),
            expires_at: Some(now + Duration::minutes(3)),
            ..Tokens::default()
        };
        assert!(!t.is_fresh(now));
        let t = Tokens {
            expires_at: Some(now + Duration::minutes(30)),
            ..t
        };
        assert!(t.is_fresh(now));
    }
}
