//! Getting, and keeping, a bearer token for the Mysa Consumer API.
//!
//! Mysa signs people in with OAuth 2.0: authorization code with PKCE (S256),
//! a public client id and no client secret. Sign-in and tokens are on
//! [`IDENTITY_BASE`]; the API is on [`API_BASE`].
//!
//! ## Whose client id
//!
//! **homeCore's**, [`CLIENT_ID`], issued by Mysa on 2026-11-01: a public
//! client, PKCE S256, no secret, shown on Mysa's consent page as "homeCore"
//! published by Simplex Technology. An operator who has a Mysa client of
//! their own sets `[mysa.auth] client_id` instead. Mysa clients have no
//! secret, so there is no secret setting.
//!
//! ## Where the browser comes back
//!
//! Mysa matches redirect URIs exactly, port included. homeCore asked for two:
//!
//! - [`RELAY_REDIRECT`], a static page on homecore.io. It reads `state`, finds
//!   the address of the person's own homeCore in it, and sends the browser
//!   back there with the `code`, **only if that address is on a home network**
//!   ([`at_home`]; the page applies the same rule). Mysa registered it on
//!   2026-11-02 after reviewing the live page, and asked to be told before
//!   any change to how the page handles the code or where it sends the
//!   browser.
//! - [`LOOPBACK_REDIRECT`], which nothing listens on. The person copies the
//!   address the browser shows and pastes it into the plugin's page: for a
//!   homeCore reached at an address that isn't on a home network.
//!
//! The code is useless on its own: exchanging it needs the PKCE verifier,
//! which never leaves this process. Mysa wants it exchanged within 60
//! seconds, which the relay does without anyone noticing; pasting has to be
//! quick.
//!
//! ## `state`
//!
//! Random, single use, good for ten minutes, and checked on the way back. It
//! may carry the address to return to: `<nonce>.<base64url(address)>`.
//!
//! ## Refresh tokens don't rotate, and don't last
//!
//! A refresh answer carries no new refresh token, so the one from signing in
//! is kept. It stops working 30 days after that sign-in, however often it is
//! used ([`REFRESH_LIFETIME`]), so [`Auth::relink_due`] says when the person
//! has to sign in again and the plugin warns ahead of it. Signing in again
//! anywhere retires the old one (only the newest works), and Mysa answers
//! `invalid_grant` from then on.
//!
//! ## Where the tokens live
//!
//! In core's durable learned state (`homecore/plugins/{id}/state`), via
//! [`PluginStateWriter`], not the config file, which core owns and watches.
//! They are never logged.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use plugin_sdk_rs::PluginStateWriter;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// The client Mysa issued homeCore: authorization code + PKCE, no secret.
/// A public client id, so it can live in the source.
pub const CLIENT_ID: &str = "5jbbk15i5u3n7mh0pq3pv6div5";
/// The relay page on homecore.io.
pub const RELAY_REDIRECT: &str = "https://homecore.io/oauth/mysa/callback";
/// Whether Mysa has registered [`RELAY_REDIRECT`] against [`CLIENT_ID`]: yes,
/// since 2026-11-02. Mysa approved the page as it was then; tell Mysa before
/// changing how it handles the code or where it sends the browser.
pub const RELAY_REGISTERED: bool = true;
/// The loopback address, also registered. Nothing listens on it.
pub const LOOPBACK_REDIRECT: &str = "http://127.0.0.1:8127/mysa/callback";

pub const IDENTITY_BASE: &str = "https://mysa-backend.mysa.cloud";
pub const API_BASE: &str = "https://consumer-api.mysa.cloud";

/// Everything homeCore asked Mysa for: read and control thermostats, read
/// schedules and hold or resume them (`schedules.write`; the plugin never
/// edits a schedule program), and energy history.
pub const SCOPES: &str = "consumer-api/devices.read consumer-api/devices.control \
consumer-api/schedules.read consumer-api/schedules.write consumer-api/energy.read";

/// Renew this long before the access token expires, so a poll that starts
/// just inside the deadline doesn't 401 on its way to the API.
const RENEW_MARGIN: Duration = Duration::minutes(5);

/// Mysa's refresh-token lifetime: 30 days from signing in, however often the
/// token is used.
pub const REFRESH_LIFETIME: Duration = Duration::days(30);

/// Say a new sign-in is due this long before it is.
pub const RELINK_WARNING: Duration = Duration::days(5);

/// How long a sign-in that was started may take to come back.
const PENDING_TTL: Duration = Duration::minutes(10);

/// The longest return address `state` will carry.
const MAX_RETURN_LEN: usize = 512;

/// How this plugin signs in: homeCore's client, unless the operator set their
/// own, and where Mysa's sign-in host is.
#[derive(Debug, Clone, Default)]
pub struct AuthSettings {
    /// An operator's own client id. Empty means [`CLIENT_ID`].
    pub client_id: Option<String>,
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
    /// Kept for the page; Mysa's API has no account call, so it stays empty.
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
    /// Where to send the browser to sign in at Mysa.
    pub url: String,
    pub redirect_uri: String,
    pub state: String,
}

/// What came back from Mysa: a code and its state, or everything pasted.
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
    own_client: bool,
    own_relay: bool,
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
            own_relay: trimmed(&settings.redirect_uri).is_some(),
            client_id: own.unwrap_or_else(|| CLIENT_ID.to_string()),
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

    /// Whether there is a client id to sign in with: homeCore's or the
    /// operator's own.
    pub fn is_configured(&self) -> bool {
        !self.client_id.is_empty()
    }

    /// Whether the operator set a client of their own.
    pub fn uses_own_client(&self) -> bool {
        self.own_client
    }

    /// Whether signing in can come back through a relay page: homeCore's,
    /// once Mysa registers it, or the operator's own `redirect_uri`.
    pub fn relay_ready(&self) -> bool {
        self.own_relay || RELAY_REGISTERED
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
            info!("Restored Mysa tokens from core's learned state");
        }
    }

    /// Start a sign-in: the address to open at Mysa.
    ///
    /// `return_to` is the address of this homeCore's page for the plugin; the
    /// relay page sends the browser back there. It is only used with
    /// [`Via::Relay`], and only if it is an http(s) address.
    pub fn begin(&self, via: Via, return_to: Option<&str>) -> Result<Begun> {
        if !self.is_configured() {
            bail!("this plugin has no Mysa client id, and none is set in its settings");
        }
        if via == Via::Relay {
            if !self.relay_ready() {
                bail!(
                    "Mysa hasn't registered homecore.io's return page yet; \
                     sign in by pasting the address instead"
                );
            }
            if return_to.is_some_and(|r| return_address(r).is_none()) {
                bail!(
                    "this page isn't on a home network, so homecore.io won't send \
                     Mysa's answer back to it; sign in by pasting the address instead"
                );
            }
        }
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
            "{}/oauth/authorize?client_id={}&response_type=code&scope={}&redirect_uri={}\
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
            // `access_denied` is the person pressing Deny on Mysa's consent page.
            self.pending.lock().expect("pending mutex").take();
            bail!("Mysa said {error}. Nothing was linked.");
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
        // A bare code has no state to check, and needs none: Mysa bound it
        // to this sign-in's PKCE challenge, so no other code can be swapped in.
        if let Some(state) = &back.state {
            if *state != pending.state {
                bail!("that came back from a different sign-in; start it again");
            }
        }
        let form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", pending.redirect_uri.clone()),
            ("client_id", self.client_id.clone()),
            ("code_verifier", pending.verifier.clone()),
        ];
        let mut tokens = self.post_token(&form).await?;
        if tokens.refresh_token.is_none() {
            // Without one the plugin stops after an hour: worth saying now.
            warn!("Mysa returned no refresh token; the plugin will need signing in again within the hour");
        }
        tokens.linked_at = Some(Utc::now());
        self.store(tokens).await;
        Ok(())
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
            _ => bail!("not linked to a Mysa account"),
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
        let form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token.to_string()),
            ("client_id", self.client_id.clone()),
        ];
        let mut tokens = match self.post_token(&form).await {
            Ok(t) => t,
            Err(e) if e.to_string().contains("invalid_grant") => {
                // Past its 30 days, replaced by a newer sign-in, or revoked: it
                // will never work again.
                self.store(Tokens::default()).await;
                bail!("Mysa ended this sign-in ({e}); link the account again");
            }
            Err(e) => return Err(e).context("renewing the access token"),
        };
        let kept = self.tokens.lock().expect("token mutex").clone();
        // Mysa's refresh answer has no refresh token: the one from signing in
        // stays good. Dropping it would un-link the plugin.
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
        info!("Renewed the Mysa access token");
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
            .post(format!("{}/oauth/token", self.identity_base))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .context("reaching Mysa's sign-in host")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            // OAuth2's own reason: `invalid_grant` for a refresh token past
            // its 30 days or replaced, `invalid_client` for a client id Mysa
            // doesn't know.
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
    /// Persisted first, so a restart finds what this run is using. Kept in
    /// memory even if persisting fails; that costs the next restart a sign-in,
    /// not this run.
    async fn store(&self, tokens: Tokens) {
        self.persist(&tokens).await;
        *self.tokens.lock().expect("token mutex") = tokens;
    }

    async fn persist(&self, tokens: &Tokens) {
        if let Err(e) = self.writer.persist(&tokens.to_state()).await {
            warn!(error = %e, "Could not keep the Mysa sign-in; it will be needed again after a restart");
        }
    }

    /// Treat the access token as spent, so the next [`Self::bearer`] renews it.
    /// For a 401 from the API: renew and retry once.
    pub fn renew_next(&self) {
        let mut guard = self.tokens.lock().expect("token mutex");
        guard.expires_at = Some(Utc::now());
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
    (matches!(url.scheme(), "http" | "https") && at_home(&url)).then(|| url.to_string())
}

/// Whether an address is on the person's own network, where the relay page
/// will send a code: localhost, a loopback or private IPv4, an IPv6 loopback
/// or unique local address, or a name only local DNS answers (`.local`,
/// `.home.arpa`, `.internal`). The same rule as the page's `atHome`; Mysa
/// checks that the page never forwards anywhere else.
pub fn at_home(url: &reqwest::Url) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr};
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return ip.is_loopback() || ip.is_private();
    }
    if let Ok(ip) = host.parse::<Ipv6Addr>() {
        return ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00;
    }
    host == "localhost"
        || [".local", ".home.arpa", ".internal"]
            .iter()
            .any(|end| host.len() > end.len() && host.ends_with(end))
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

    /// homeCore's id is still empty, so tests sign in as an operator's own.
    fn auth_with(settings: AuthSettings) -> Auth {
        let settings = AuthSettings {
            client_id: settings.client_id.or(Some("test-client".into())),
            // As if Mysa had registered the relay page for this client.
            redirect_uri: settings.redirect_uri.or(Some(RELAY_REDIRECT.into())),
            ..settings
        };
        Auth::new(
            &settings,
            reqwest::Client::new(),
            PluginStateWriter::test_instance("plugin.mysa"),
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
    fn homecores_client_signs_in_through_the_registered_relay_or_by_pasting() {
        let a = Auth::new(
            &AuthSettings::default(),
            reqwest::Client::new(),
            PluginStateWriter::test_instance("plugin.mysa"),
        );
        assert_eq!(CLIENT_ID, "5jbbk15i5u3n7mh0pq3pv6div5");
        assert!(a.is_configured());
        // Mysa registered the relay (2026-11-02): a home address signs in through it.
        assert!(a.relay_ready());
        let relay = a.begin(Via::Relay, Some("http://192.168.1.20/x")).unwrap();
        assert_eq!(relay.redirect_uri, RELAY_REDIRECT);
        assert_eq!(
            param(&relay.url, "redirect_uri").as_deref(),
            Some(RELAY_REDIRECT)
        );
        // Pasting still works, for a homeCore reached from outside.
        let b = a.begin(Via::Loopback, None).unwrap();
        assert_eq!(param(&b.url, "client_id").as_deref(), Some(CLIENT_ID));
        assert_eq!(b.redirect_uri, LOOPBACK_REDIRECT);
    }

    #[test]
    fn with_no_client_id_it_says_so_and_starts_nothing() {
        let a = Auth::new(
            &AuthSettings::default(),
            reqwest::Client::new(),
            PluginStateWriter::test_instance("plugin.mysa"),
        );
        // An empty id can only come from a build without one; say so.
        let blank = Auth {
            client_id: String::new(),
            ..a
        };
        assert!(!blank.is_configured());
        let e = blank.begin(Via::Loopback, None).unwrap_err();
        assert!(e.to_string().contains("no Mysa client id"), "{e}");
    }

    #[test]
    fn a_code_is_sent_home_only_to_a_home_network() {
        let home = [
            "http://10.0.0.5:8080/settings/plugins/plugin.mysa",
            "https://192.168.1.20/x",
            "http://172.20.1.1",
            "http://127.0.0.1:5180",
            "http://localhost:5180",
            "http://homecore.local",
            "http://hc.home.arpa",
            "http://hub.internal",
            "http://[fd12:3456::1]/x",
            "http://[::1]",
        ];
        let away = [
            "http://evil.com",
            "http://192.168.1.2.evil.com",
            "http://homecore",
            "http://a.local.evil.com",
            "http://user:pw@10.0.0.1",
            "http://172.32.0.1",
            "http://169.254.1.1",
            "http://8.8.8.8",
            "http://100.64.1.1",
            "http://[2001:db8::1]",
            "http://[fe80::1]",
            "http://evil.lan",
            "http://.local",
        ];
        for u in home {
            assert!(at_home(&reqwest::Url::parse(u).unwrap()), "{u} is home");
        }
        for u in away {
            let ok = reqwest::Url::parse(u).map(|u| at_home(&u)).unwrap_or(false);
            assert!(!ok, "{u} is not home");
            assert!(
                auth().begin(Via::Relay, Some(u)).is_err(),
                "{u} was accepted"
            );
        }
    }

    #[test]
    fn signs_in_at_mysa_with_pkce_every_scope_and_no_secret() {
        let b = auth().begin(Via::Relay, None).unwrap();
        assert!(b
            .url
            .starts_with("https://mysa-backend.mysa.cloud/oauth/authorize?"));
        assert_eq!(param(&b.url, "client_id").as_deref(), Some("test-client"));
        assert_eq!(param(&b.url, "response_type").as_deref(), Some("code"));
        assert_eq!(
            param(&b.url, "code_challenge_method").as_deref(),
            Some("S256")
        );
        let scope = param(&b.url, "scope").unwrap();
        for s in [
            "consumer-api/devices.read",
            "consumer-api/devices.control",
            "consumer-api/schedules.read",
            "consumer-api/schedules.write",
            "consumer-api/energy.read",
        ] {
            assert!(scope.split(' ').any(|x| x == s), "{s} missing from {scope}");
        }
        assert_eq!(
            param(&b.url, "redirect_uri").as_deref(),
            Some(RELAY_REDIRECT)
        );
        assert_eq!(RELAY_REDIRECT, "https://homecore.io/oauth/mysa/callback");
        assert_eq!(LOOPBACK_REDIRECT, "http://127.0.0.1:8127/mysa/callback");
        assert!(!b.url.contains("secret"));
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
    fn the_verifier_is_long_enough_and_from_the_allowed_characters() {
        let v = random_urlsafe(48);
        assert!((43..=128).contains(&v.len()), "{}", v.len());
        assert!(v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)));
    }

    #[test]
    fn the_loopback_is_the_other_address_and_carries_no_return() {
        let b = auth()
            .begin(Via::Loopback, Some("https://home.example/x"))
            .unwrap();
        assert_eq!(b.redirect_uri, LOOPBACK_REDIRECT);
        assert!(!b.state.contains('.'));
    }

    #[test]
    fn state_is_random_and_carries_the_return_address_for_the_relay() {
        let a = auth();
        let back = "http://192.168.1.20:3000/settings/plugins/plugin.mysa";
        let one = a.begin(Via::Relay, Some(back)).unwrap();
        let two = a.begin(Via::Relay, Some(back)).unwrap();
        assert_ne!(one.state, two.state);
        assert_eq!(return_address_in(&one.state).as_deref(), Some(back));
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
            assert!(
                auth().begin(Via::Relay, Some(bad)).is_err(),
                "{bad} was accepted"
            );
        }
        // With no address at all, the relay page shows the address to paste.
        let b = auth().begin(Via::Relay, None).unwrap();
        assert!(!b.state.contains('.'));
    }

    #[test]
    fn what_was_pasted_is_read_as_an_address_or_a_bare_code() {
        let r = Returned::from_pasted("http://127.0.0.1:8127/mysa/callback?code=C%2B1&state=S1");
        assert_eq!(r.code.as_deref(), Some("C+1"));
        assert_eq!(r.state.as_deref(), Some("S1"));
        let r = Returned::from_pasted("  THECODE ");
        assert_eq!(r.code.as_deref(), Some("THECODE"));
        let r = Returned::from_pasted(
            "https://homecore.io/oauth/mysa/callback?error=access_denied&state=S",
        );
        assert_eq!(r.error.as_deref(), Some("access_denied"));
    }

    #[tokio::test]
    async fn a_code_is_exchanged_form_encoded_with_the_client_id_and_verifier() {
        let (base, seen) = token_server(vec![(
            200,
            r#"{"access_token":"A1","refresh_token":"R1","token_type":"Bearer","expires_in":3600}"#
                .into(),
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
        assert!(body.contains("code=CODE"));
        assert!(body.contains("code_verifier="));
        assert!(body.contains("client_id=test-client"));
        assert!(body.contains(&format!(
            "redirect_uri={}",
            urlencoding::encode(RELAY_REDIRECT)
        )));
        assert!(!body.contains("client_secret"));
        let t = a.tokens();
        assert_eq!(t.refresh_token.as_deref(), Some("R1"));
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
    async fn denying_at_mysa_links_nothing() {
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
    async fn a_refresh_keeps_the_refresh_token_from_signing_in_and_runs_one_at_a_time() {
        // Mysa's refresh answer has no refresh_token.
        let (base, seen) = token_server(vec![(
            200,
            r#"{"access_token":"A2","token_type":"Bearer","expires_in":3600}"#.into(),
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
            account: None,
        });
        let (x, y) = tokio::join!(a.bearer(), a.bearer());
        assert_eq!(x.unwrap(), "A2");
        assert_eq!(y.unwrap(), "A2");
        assert_eq!(seen.lock().unwrap().len(), 1);
        let body = seen.lock().unwrap()[0].clone();
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=R1"));
        assert!(body.contains("client_id=test-client"));
        let t = a.tokens();
        assert_eq!(t.refresh_token.as_deref(), Some("R1"));
        // Refreshing doesn't move the 30 days.
        assert_eq!(t.linked_at, Some(linked));
    }

    #[tokio::test]
    async fn after_a_401_the_next_bearer_renews() {
        let (base, seen) = token_server(vec![(
            200,
            r#"{"access_token":"A2","expires_in":3600}"#.into(),
        )])
        .await;
        let a = auth_with(AuthSettings {
            identity_base: Some(base),
            ..AuthSettings::default()
        });
        a.set_tokens(Tokens {
            access_token: Some("A1".into()),
            expires_at: Some(Utc::now() + Duration::minutes(50)),
            refresh_token: Some("R1".into()),
            linked_at: Some(Utc::now()),
            account: None,
        });
        assert_eq!(a.bearer().await.unwrap(), "A1");
        a.renew_next();
        assert_eq!(a.bearer().await.unwrap(), "A2");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn invalid_grant_signs_the_plugin_out() {
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
            account: None,
        };
        let back = Tokens::from_state(&t.to_state());
        assert_eq!(back.refresh_token, t.refresh_token);
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
