//! The running plugin: read Mysa, publish what it says, apply commands.
//!
//! ## One read for every thermostat
//!
//! Each poll lists the account's thermostats and reads all their states in one
//! `POST /v1/state/batch`. What each one can do (`GET /v1/capabilities`) is
//! read when it is first seen and again every few hours, since hardware
//! doesn't change and Mysa limits requests per app and per customer.
//!
//! ## Notices are a set, not a log
//!
//! Each is raised while its condition holds and cleared when it stops: no
//! client id yet, not signed in, sign-in ending soon, a token Mysa won't take,
//! a permission missing, Mysa unreachable, told to wait, no thermostats.
//!
//! ## Waiting when told to
//!
//! A `429` or `503` carries `Retry-After`. Nothing is sent to Mysa until it has
//! passed, polls included.
//!
//! ## Reconcile is guarded
//!
//! A thermostat is only retired after a list call that succeeded. A failed
//! fetch says nothing about which thermostats exist.

use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{DevicePublisher, PluginNotices};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::api::{ApiError, Capabilities, Device, MysaApi, State};
use crate::auth::Auth;
use crate::config::MysaSection;
use crate::device::{self, Now, Write};

pub const NOTICE_NOT_CONFIGURED: &str = "not_configured";
pub const NOTICE_NOT_LINKED: &str = "not_linked";
pub const NOTICE_RELINK_DUE: &str = "relink_due";
pub const NOTICE_AUTH_REJECTED: &str = "auth_rejected";
pub const NOTICE_SCOPE: &str = "missing_permission";
pub const NOTICE_UNREACHABLE: &str = "api_unreachable";
pub const NOTICE_RATE_LIMITED: &str = "rate_limited";
pub const NOTICE_NO_THERMOSTATS: &str = "no_thermostats";
pub const NOTICE_UNDESCRIBED: &str = "undescribed_thermostat";

/// How long after a write to read the thermostat back. Mysa answers `204`
/// once it accepts a change; the thermostat applies it shortly after.
const WRITE_SETTLE: Duration = Duration::from_millis(2000);

/// How often to read a thermostat's capabilities again.
const CAPABILITIES_TTL: chrono::Duration = chrono::Duration::hours(6);

/// The least time between two writes to one thermostat. Mysa asks clients not
/// to automate rapid changes, and limits writes more tightly than reads.
const WRITE_SPACING: Duration = Duration::from_millis(1500);

/// Writes allowed in any 60 seconds, across every thermostat. Mysa allows a
/// customer 15 writes a minute through one app (30 in all, 2026-11-01); this
/// stays under it, so a scene changing every thermostat at once is spread
/// over a minute rather than refused.
const WRITES_PER_MINUTE: usize = 12;

/// The account's writes in the last `window` (a minute).
struct WriteBudget {
    window: Duration,
    sent: Mutex<std::collections::VecDeque<tokio::time::Instant>>,
}

impl Default for WriteBudget {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(60),
            sent: Mutex::default(),
        }
    }
}

impl WriteBudget {
    /// Wait until a write fits, then count it.
    async fn take(&self) {
        let minute = self.window;
        loop {
            let wait = {
                let mut sent = self.sent.lock().expect("budget mutex");
                let now = tokio::time::Instant::now();
                while sent
                    .front()
                    .is_some_and(|t| now.duration_since(*t) >= minute)
                {
                    sent.pop_front();
                }
                if sent.len() < WRITES_PER_MINUTE {
                    sent.push_back(now);
                    return;
                }
                minute - now.duration_since(*sent.front().expect("full"))
            };
            tokio::time::sleep(wait).await;
        }
    }
}

/// When Mysa sends no `Retry-After`.
const DEFAULT_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
struct Known {
    name: String,
    model: Option<String>,
    caps: Option<Capabilities>,
    caps_at: Option<chrono::DateTime<chrono::Utc>>,
    state: Option<State>,
    registered: bool,
    last_write: Option<std::time::Instant>,
}

pub struct Runtime {
    pub api: MysaApi,
    pub auth: Arc<Auth>,
    pub publisher: DevicePublisher,
    pub notices: PluginNotices,
    pub area: Option<String>,
    pub only_devices: Vec<String>,
    /// Mysa id → what is known about it.
    known: Mutex<HashMap<String, Known>>,
    /// Nothing goes to Mysa before this.
    wait_until: Mutex<Option<std::time::Instant>>,
    writes: WriteBudget,
}

impl Runtime {
    pub fn new(
        api: MysaApi,
        auth: Arc<Auth>,
        publisher: DevicePublisher,
        notices: PluginNotices,
        cfg: &MysaSection,
    ) -> Self {
        Self {
            api,
            auth,
            publisher,
            notices,
            area: cfg.area.clone(),
            only_devices: cfg
                .only_devices
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            known: Mutex::new(HashMap::new()),
            wait_until: Mutex::new(None),
            writes: WriteBudget::default(),
        }
    }

    fn wanted(&self, id: &str) -> bool {
        self.only_devices.is_empty() || self.only_devices.iter().any(|d| d == id)
    }

    fn mysa_id_for(&self, device_id: &str) -> Option<String> {
        let guard = self.known.lock().expect("known mutex");
        guard
            .keys()
            .find(|id| device::device_id(id) == device_id)
            .cloned()
    }

    /// Whether Mysa asked this plugin to wait, and for how long still.
    fn waiting(&self) -> Option<Duration> {
        let guard = self.wait_until.lock().expect("wait mutex");
        guard.and_then(|t| t.checked_duration_since(std::time::Instant::now()))
    }

    fn wait_for(&self, d: Option<Duration>) {
        *self.wait_until.lock().expect("wait mutex") =
            Some(std::time::Instant::now() + d.unwrap_or(DEFAULT_WAIT));
    }

    /// One call to Mysa with a fresh token; on a `401`, renew once and try
    /// again, as Mysa's docs say.
    async fn call<T, F, Fut>(&self, f: F) -> Result<T, CallError>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        if let Some(left) = self.waiting() {
            return Err(CallError::Waiting(left));
        }
        let bearer = self.auth.bearer().await.map_err(CallError::Token)?;
        match f(bearer).await {
            Err(ApiError::Unauthorized) => {
                self.auth.renew_next();
                let bearer = self.auth.bearer().await.map_err(CallError::Token)?;
                f(bearer).await.map_err(CallError::Api)
            }
            other => other.map_err(CallError::Api),
        }
    }

    /// One poll: list, read every state, publish, reconcile.
    pub async fn poll(&self) {
        if !self.auth.is_configured() {
            self.notices.raise(
                PluginNotice::warning(
                    NOTICE_NOT_CONFIGURED,
                    "This plugin has no Mysa client id, so it can't sign in.",
                )
                .with_remedy(
                    "Update the plugin. If Mysa issued you a client of your own, enter its \
                     id in the settings below.",
                ),
            );
            return;
        }
        self.notices.clear(NOTICE_NOT_CONFIGURED);
        if !self.auth.is_linked() {
            self.not_linked("Not signed in to Mysa, so no thermostats are published.");
            return;
        }

        let devices = match self
            .call(|b| async move { self.api.devices(&b).await })
            .await
        {
            Ok(list) => list,
            Err(e) => return self.raise_for(&e),
        };
        self.clear_transport_notices();
        self.notices.clear(NOTICE_NOT_LINKED);

        // Mysa ends a sign-in 30 days after it was made, used or not.
        match self.auth.relink_due() {
            Some(due) if self.auth.relink_soon(chrono::Utc::now()) => self.notices.raise(
                PluginNotice::warning(
                    NOTICE_RELINK_DUE,
                    format!(
                        "Mysa ends this sign-in on {}, after which no thermostats are published.",
                        due.format("%B %-d")
                    ),
                )
                .with_remedy(
                    "Use \"Link Mysa account\" on this page before then. It takes a minute and \
                     starts another 30 days.",
                ),
            ),
            _ => self.notices.clear(NOTICE_RELINK_DUE),
        }

        let wanted: Vec<&Device> = devices.iter().filter(|d| self.wanted(&d.id)).collect();
        if wanted.is_empty() {
            self.notices.raise(
                PluginNotice::warning(
                    NOTICE_NO_THERMOSTATS,
                    if devices.is_empty() {
                        "This Mysa account shared no thermostats with homeCore."
                    } else {
                        "No thermostats match the device ids this plugin is configured for."
                    },
                )
                .with_remedy(
                    "Sign in again and share the thermostats on Mysa's consent page, or empty \
                     \"Only these Mysa device ids\".",
                ),
            );
        } else {
            self.notices.clear(NOTICE_NO_THERMOSTATS);
        }

        // What each can do: first time seen, then every few hours.
        let now = chrono::Utc::now();
        for d in &wanted {
            let stale = {
                let guard = self.known.lock().expect("known mutex");
                guard
                    .get(&d.id)
                    .and_then(|k| k.caps_at)
                    .is_none_or(|at| now - at > CAPABILITIES_TTL)
            };
            if stale {
                let id = d.id.clone();
                match self
                    .call(|b| {
                        let id = id.clone();
                        async move { self.api.capabilities(&b, &id).await }
                    })
                    .await
                {
                    Ok(caps) => {
                        // What Mysa allows, for checking against its app.
                        for (mode, m) in
                            [("heat", &caps.climate.heat), ("cool", &caps.climate.cool)]
                        {
                            if let Some(sp) = m
                                .as_ref()
                                .and_then(|m| m.setpoint.as_ref().or(m.heat_setpoint.as_ref()))
                            {
                                debug!(
                                    device = %d.id,
                                    mode,
                                    controllable = sp.user_controllable,
                                    values = ?sp.values,
                                    "Mysa's setpoints"
                                );
                            }
                        }
                        let mut guard = self.known.lock().expect("known mutex");
                        let k = guard.entry(d.id.clone()).or_default();
                        k.caps = Some(caps);
                        k.caps_at = Some(now);
                    }
                    Err(e) => {
                        warn!(device = %d.id, error = %e, "Couldn't read a thermostat's capabilities");
                        if matches!(e, CallError::Waiting(_)) {
                            return;
                        }
                    }
                }
            }
        }

        let ids: Vec<String> = wanted.iter().map(|d| d.id.clone()).collect();
        let states = if ids.is_empty() {
            HashMap::new()
        } else {
            match self
                .call(|b| {
                    let ids = ids.clone();
                    async move { self.api.states(&b, &ids).await }
                })
                .await
            {
                Ok(s) => s,
                Err(e) => return self.raise_for(&e),
            }
        };

        let mut undescribed = Vec::new();
        for d in &wanted {
            let state = states.get(&d.id);
            if let Some(err) = state.and_then(|s| s.error.as_deref()) {
                debug!(device = %d.id, error = err, "Mysa couldn't read this thermostat this time");
            }
            match self
                .publish(d, state.filter(|s| s.error.is_none()), None)
                .await
            {
                Ok(described) => {
                    if !described {
                        undescribed.push(if d.name.is_empty() {
                            d.id.clone()
                        } else {
                            d.name.clone()
                        });
                    }
                }
                Err(e) => warn!(device = %d.id, error = %e, "Could not publish a thermostat"),
            }
        }
        if undescribed.is_empty() {
            self.notices.clear(NOTICE_UNDESCRIBED);
        } else {
            self.notices.raise(
                PluginNotice::warning(
                    NOTICE_UNDESCRIBED,
                    format!(
                        "Mysa doesn't describe what {} can do yet, so it is shown read-only.",
                        undescribed.join(", ")
                    ),
                )
                .with_remedy(
                    "This usually clears once the thermostat has reported to Mysa. If it \
                     doesn't, it's a model Mysa's API doesn't describe yet.",
                ),
            );
        }

        let live: HashSet<String> = wanted.iter().map(|d| device::device_id(&d.id)).collect();
        match self.publisher.reconcile_devices(live).await {
            Ok(report) => {
                for id in &report.stale_unregistered {
                    info!(device_id = %id, "Retired a thermostat that is no longer shared");
                }
            }
            Err(e) => warn!(error = %e, "Reconcile failed; stale devices may linger"),
        }
        let seen: HashSet<&str> = wanted.iter().map(|d| d.id.as_str()).collect();
        self.known
            .lock()
            .expect("known mutex")
            .retain(|id, _| seen.contains(id.as_str()));
    }

    /// Register (once), then publish state and availability. Returns whether
    /// Mysa describes the thermostat's controls.
    async fn publish(
        &self,
        d: &Device,
        state: Option<&State>,
        caused_by: Option<&Value>,
    ) -> anyhow::Result<bool> {
        let device_id = device::device_id(&d.id);
        let name = if d.name.trim().is_empty() {
            format!("Mysa {}", d.model.as_deref().unwrap_or(&d.id))
        } else {
            d.name.clone()
        };
        let (caps, first_time) = {
            let mut guard = self.known.lock().expect("known mutex");
            let k = guard.entry(d.id.clone()).or_default();
            k.name = name.clone();
            k.model = d.model.clone();
            if let Some(s) = state {
                k.state = Some(s.clone());
            }
            let first = !k.registered && k.caps.is_some();
            if first {
                k.registered = true;
            }
            (k.caps.clone(), first)
        };
        let Some(caps) = caps else {
            // Not drawn until it's known what it can do.
            return Ok(true);
        };

        if first_time {
            self.publisher
                .register_device_full(
                    &device_id,
                    &name,
                    Some("thermostat"),
                    self.area.as_deref(),
                    None,
                )
                .await?;
            self.publisher.subscribe_commands(&device_id).await?;
            self.publisher
                .register_device_schema(&device_id, &device::device_schema(&caps))
                .await?;
            info!(device_id = %device_id, name = %name, "Registered a Mysa thermostat");
        }

        if let Some(s) = state {
            let payload = device::state_payload(s, &caps, d.model.as_deref());
            match caused_by {
                Some(cmd) => {
                    self.publisher
                        .publish_state_for_command(&device_id, &payload, cmd, "mysa")
                        .await?
                }
                None => self.publisher.publish_state(&device_id, &payload).await?,
            }
            self.publisher
                .set_available(&device_id, s.connected)
                .await?;
        }
        Ok(caps.published)
    }

    /// Apply one command to one thermostat.
    pub async fn apply(&self, device_id: &str, payload: &Value) {
        let Some(id) = self.mysa_id_for(device_id) else {
            warn!(
                device_id,
                "Command for a thermostat this plugin does not own"
            );
            return;
        };
        let (caps, state, model, name, last_write) = {
            let guard = self.known.lock().expect("known mutex");
            let k = guard.get(&id).cloned().unwrap_or_default();
            (k.caps, k.state, k.model, k.name, k.last_write)
        };
        let Some(caps) = caps else {
            warn!(
                device_id,
                "Command before this thermostat's controls are known"
            );
            return;
        };
        let writes = match device::interpret(
            payload,
            &caps,
            Now {
                state: state.as_ref(),
            },
        ) {
            Ok(w) => w,
            Err(rejection) => {
                warn!(device_id, %rejection, "Rejected a command");
                return;
            }
        };

        // Space writes to one thermostat out; Mysa limits writes tightly.
        if let Some(at) = last_write {
            if let Some(left) = WRITE_SPACING.checked_sub(at.elapsed()) {
                tokio::time::sleep(left).await;
            }
        }

        for w in &writes {
            self.writes.take().await;
            let id2 = id.clone();
            let result = self
                .call(|b| {
                    let id = id2.clone();
                    let w = w.clone();
                    async move { self.send(&b, &id, &w).await }
                })
                .await;
            if let Some(k) = self.known.lock().expect("known mutex").get_mut(&id) {
                k.last_write = Some(std::time::Instant::now());
            }
            if let Err(e) = result {
                error!(device_id, error = %e, "Mysa refused a command");
                self.raise_for(&e);
                // No state publish: the thermostat still does the old thing.
                return;
            }
        }
        self.clear_transport_notices();

        // Read back what the thermostat now reports, rather than echoing.
        tokio::time::sleep(WRITE_SETTLE).await;
        let id2 = id.clone();
        match self
            .call(|b| {
                let id = id2.clone();
                async move { self.api.state(&b, &id).await }
            })
            .await
        {
            Ok(s) => {
                let d = Device {
                    id: id.clone(),
                    name,
                    model,
                    ..Default::default()
                };
                if let Err(e) = self.publish(&d, Some(&s), Some(payload)).await {
                    warn!(device_id, error = %e, "Could not publish after a command");
                }
            }
            Err(e) => debug!(device_id, error = %e, "Could not read back after a command"),
        }
    }

    async fn send(&self, bearer: &str, id: &str, w: &Write) -> Result<(), ApiError> {
        match w {
            Write::Mode(m) => self.api.set_mode(bearer, id, m).await,
            Write::Heat(c) => self.api.set_heat(bearer, id, *c).await,
            Write::Cool(c) => self.api.set_cool(bearer, id, *c).await,
            Write::Auto(t) => self.api.set_auto(bearer, id, t).await,
            Write::Fan { mode, speed } => self.api.set_fan(bearer, id, mode, *speed).await,
            Write::Lockout(l) => self.api.set_lockout(bearer, id, *l).await,
            Write::Brightness { active, idle } => {
                self.api.set_brightness(bearer, id, *active, *idle).await
            }
            Write::Swing { axis, position } => {
                self.api.set_swing(bearer, id, axis, *position).await
            }
            Write::Hold => self.api.hold(bearer, id).await,
            Write::Resume => self.api.resume(bearer, id).await,
        }
    }

    /// Turn a failure into the notice that names it.
    fn raise_for(&self, e: &CallError) {
        match e {
            CallError::Waiting(left) => {
                debug!(seconds = left.as_secs(), "Waiting, as Mysa asked");
            }
            CallError::Token(err) => {
                if self.auth.is_linked() {
                    self.notices.raise(
                        PluginNotice::error(
                            NOTICE_UNREACHABLE,
                            format!("Couldn't renew the Mysa sign-in: {err}"),
                        )
                        .with_remedy("Check this machine's internet connection."),
                    );
                } else {
                    self.not_linked("Mysa ended the sign-in, so no thermostats are published.");
                }
            }
            CallError::Api(ApiError::Unauthorized) => self.notices.raise(
                PluginNotice::error(NOTICE_AUTH_REJECTED, "Mysa rejected this plugin's sign-in.")
                    .with_remedy("Use \"Link Mysa account\" on this page to sign in again."),
            ),
            CallError::Api(ApiError::InsufficientScope) => self.notices.raise(
                PluginNotice::error(
                    NOTICE_SCOPE,
                    "The Mysa sign-in doesn't allow something this plugin asked for.",
                )
                .with_remedy(
                    "Sign in again with \"Link Mysa account\" and approve everything on Mysa's \
                     consent page.",
                ),
            ),
            CallError::Api(ApiError::RateLimited { retry_after }) => {
                self.wait_for(*retry_after);
                let secs = retry_after.unwrap_or(DEFAULT_WAIT).as_secs();
                self.notices.raise(
                    PluginNotice::warning(
                        NOTICE_RATE_LIMITED,
                        format!("Mysa asked this plugin to wait. Trying again in about {secs} s."),
                    )
                    .with_remedy("Increase \"Check every\" on this page if it keeps happening."),
                );
            }
            CallError::Api(ApiError::Transport(msg)) => self.notices.raise(
                PluginNotice::error(NOTICE_UNREACHABLE, format!("Can't reach Mysa: {msg}"))
                    .with_remedy("Check this machine's internet connection."),
            ),
            CallError::Api(ApiError::InvalidRequest) | CallError::Api(ApiError::NotFound) => {
                // About one command or one thermostat, not the plugin as a whole.
                warn!(error = %e, "Mysa refused a request");
            }
            CallError::Api(ApiError::Api { status, code }) => self.notices.raise(
                PluginNotice::error(
                    NOTICE_UNREACHABLE,
                    format!("Mysa answered {status} ({code})."),
                )
                .with_remedy("Usually passing; the next check tries again."),
            ),
        }
    }

    fn clear_transport_notices(&self) {
        for n in [
            NOTICE_UNREACHABLE,
            NOTICE_RATE_LIMITED,
            NOTICE_AUTH_REJECTED,
            NOTICE_SCOPE,
        ] {
            self.notices.clear(n);
        }
    }

    fn not_linked(&self, message: &str) {
        self.notices.clear(NOTICE_RELINK_DUE);
        self.notices.raise(
            PluginNotice::warning(NOTICE_NOT_LINKED, message)
                .with_remedy("Use \"Link Mysa account\" on this page."),
        );
    }

    /// A snapshot for the `status` action, which the plugin's page reads.
    pub fn status(&self) -> Value {
        let guard = self.known.lock().expect("known mutex");
        let mut thermostats: Vec<Value> = guard
            .iter()
            .map(|(id, k)| {
                serde_json::json!({
                    "device_id": device::device_id(id),
                    "mysa_id": id,
                    "name": k.name,
                    "model": k.model,
                    "connected": k.state.as_ref().map(|s| s.connected),
                    "described": k.caps.as_ref().map(|c| c.published),
                })
            })
            .collect();
        thermostats.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        serde_json::json!({
            "configured": self.auth.is_configured(),
            "relay_ready": self.auth.relay_ready(),
            "linked": self.auth.is_linked(),
            "own_client": self.auth.uses_own_client(),
            "linked_at": self.auth.linked_at().map(|t| t.to_rfc3339()),
            "relink_due": self.auth.relink_due().map(|t| t.to_rfc3339()),
            "token_expires_in_secs": self.auth.expires_in().map(|d| d.num_seconds()),
            "waiting_secs": self.waiting().map(|d| d.as_secs()),
            "thermostats": thermostats,
        })
    }
}

/// Why a call to Mysa didn't happen or didn't work.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("waiting {0:?} as Mysa asked")]
    Waiting(Duration),
    #[error("no usable token: {0}")]
    Token(anyhow::Error),
    #[error(transparent)]
    Api(ApiError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_limit_stops_everything_until_retry_after() {
        let (api, http) = MysaApi::new(Some("http://127.0.0.1:9")).unwrap();
        let auth = Arc::new(Auth::new(
            &crate::auth::AuthSettings {
                client_id: Some("c".into()),
                ..Default::default()
            },
            http,
            plugin_sdk_rs::PluginStateWriter::test_instance("plugin.mysa"),
        ));
        let rt = Runtime {
            api,
            auth,
            publisher: DevicePublisher::test_instance("plugin.mysa"),
            notices: PluginNotices::test_instance(),
            area: None,
            only_devices: vec![],
            known: Mutex::new(HashMap::new()),
            wait_until: Mutex::new(None),
            writes: WriteBudget::default(),
        };
        assert!(rt.waiting().is_none());
        rt.raise_for(&CallError::Api(ApiError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        }));
        let left = rt.waiting().unwrap();
        assert!(left > Duration::from_secs(25) && left <= Duration::from_secs(30));
    }

    #[test]
    fn only_listed_devices_are_wanted_when_a_list_is_set() {
        let (api, http) = MysaApi::new(None).unwrap();
        let auth = Arc::new(Auth::new(
            &Default::default(),
            http,
            plugin_sdk_rs::PluginStateWriter::test_instance("plugin.mysa"),
        ));
        let mut rt = Runtime {
            api,
            auth,
            publisher: DevicePublisher::test_instance("plugin.mysa"),
            notices: PluginNotices::test_instance(),
            area: None,
            only_devices: vec![],
            known: Mutex::new(HashMap::new()),
            wait_until: Mutex::new(None),
            writes: WriteBudget::default(),
        };
        assert!(rt.wanted("anything"));
        rt.only_devices = vec!["bb-1".into()];
        assert!(rt.wanted("bb-1"));
        assert!(!rt.wanted("ac-1"));
    }

    #[tokio::test]
    async fn writes_across_thermostats_stay_under_mysas_limit() {
        // A short window stands in for the minute.
        let window = Duration::from_millis(300);
        let b = WriteBudget {
            window,
            sent: Mutex::default(),
        };
        let start = tokio::time::Instant::now();
        for _ in 0..WRITES_PER_MINUTE {
            b.take().await;
        }
        assert!(start.elapsed() < window / 3, "the first ones go at once");
        b.take().await;
        assert!(start.elapsed() >= window, "the next waits for the window");
        assert_eq!(WriteBudget::default().window, Duration::from_secs(60));
        const {
            assert!(
                WRITES_PER_MINUTE < 15,
                "Mysa allows 15 per app per customer"
            )
        };
    }
}
