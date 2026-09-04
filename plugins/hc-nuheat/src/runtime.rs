//! The running plugin: poll NuHeat, publish what it says, apply commands.
//!
//! ## Notices, and why they are a set rather than a log
//!
//! Everything that can go wrong here is a *condition* an operator can act on —
//! not signed in, token expired, cloud unreachable, rate limited, no
//! thermostats on the account. Each is raised while it holds and cleared the
//! moment it stops, on every poll, because a notice that is only ever raised
//! leaves someone staring at a problem they already fixed.
//!
//! The classification in [`ApiError`] is what makes them distinguishable: an
//! expired token and an unreachable cloud produce different notices with
//! different remedies, rather than one "something went wrong".
//!
//! ## Why reconcile is guarded
//!
//! Devices here come from a cloud account, so an empty or partial fetch is a
//! *fetch failure*, not an empty account. Unregistering on one would delete
//! healthy thermostats — confidently, which is worse than leaving a zombie.
//! Reconcile therefore runs only after a fetch that actually succeeded, which
//! is the `all_sources_succeeded` shape the template describes.

use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::{DevicePublisher, PluginNotices};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::api::{ApiError, Mode, NuHeatApi, Thermostat};
use crate::auth::{Auth, AuthMode};
use crate::config::NuHeatSection;
use crate::device::{self, CommandContext, Intent, ScaleCheck};
use crate::units;

pub const NOTICE_NOT_CONFIGURED: &str = "not_configured";
pub const NOTICE_NOT_LINKED: &str = "not_linked";
pub const NOTICE_TOKEN_EXPIRING: &str = "token_expiring";
pub const NOTICE_AUTH_REJECTED: &str = "auth_rejected";
pub const NOTICE_UNREACHABLE: &str = "api_unreachable";
pub const NOTICE_RATE_LIMITED: &str = "rate_limited";
pub const NOTICE_NO_THERMOSTATS: &str = "no_thermostats";
pub const NOTICE_SCALE: &str = "implausible_reading";

/// How long to let NuHeat's cloud settle before reading back what a write did.
///
/// The API acknowledges a mode change before the thermostat has necessarily
/// reported it, so an immediate re-read can return the old setpoint. Publishing
/// that would show the operator their command being undone. A short wait gets
/// the common case right; the poll loop corrects the rest.
const WRITE_SETTLE: Duration = Duration::from_millis(1500);

pub struct Runtime {
    pub api: NuHeatApi,
    pub auth: Arc<Auth>,
    pub publisher: DevicePublisher,
    pub notices: PluginNotices,
    pub area: Option<String>,
    pub only_serials: Vec<String>,
    pub bare_setpoint_is_permanent: bool,
    pub default_hold_hours: Option<i64>,
    /// What this plugin will ask for, after the operator's floor-covering
    /// limits are applied to the thermostat's own range.
    pub limits: units::SetpointLimits,
    /// serial → last known setpoint in °C, for commands that name no
    /// temperature of their own, plus the device-id mapping in both directions.
    known: Mutex<HashMap<String, Known>>,
}

#[derive(Debug, Clone, Default)]
struct Known {
    setpoint_c: Option<f64>,
}

impl Runtime {
    pub fn new(
        api: NuHeatApi,
        auth: Arc<Auth>,
        publisher: DevicePublisher,
        notices: PluginNotices,
        cfg: &NuHeatSection,
    ) -> Self {
        Self {
            api,
            auth,
            publisher,
            notices,
            area: cfg.area.clone(),
            only_serials: cfg
                .only_serials
                .iter()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
            bare_setpoint_is_permanent: cfg.setpoint_holds_permanently,
            default_hold_hours: cfg.default_hold_hours,
            limits: cfg.setpoint_limits(),
            known: Mutex::new(HashMap::new()),
        }
    }

    /// Whether config says to publish this thermostat at all.
    fn wanted(&self, serial: &str) -> bool {
        self.only_serials.is_empty() || self.only_serials.contains(&serial.trim().to_lowercase())
    }

    fn serial_for(&self, device_id: &str) -> Option<String> {
        let guard = self.known.lock().expect("known mutex");
        guard
            .keys()
            .find(|serial| device::device_id(serial) == device_id)
            .cloned()
    }

    /// One poll: fetch every thermostat, publish it, reconcile if the fetch
    /// was trustworthy.
    pub async fn poll(&self) {
        // Two different problems with two different fixes, and telling them
        // apart is the whole point of raising them separately: a fresh install
        // has no API credentials, whereas a configured one has simply not
        // signed in yet. "Not signed in" sends someone to the wrong button.
        if !self.auth.is_configured() {
            self.notices.raise(
                PluginNotice::warning(
                    NOTICE_NOT_CONFIGURED,
                    "No NuHeat API credentials, so no thermostats are published.",
                )
                .with_remedy(
                    "Enter the client id and redirect URI NuHeat support issued you, under \
                     \"NuHeat account\" in this plugin's configuration. Request them from \
                     NuHeat at https://api.mynuheat.com/ if you do not have them yet.",
                ),
            );
            return;
        }
        self.notices.clear(NOTICE_NOT_CONFIGURED);

        let bearer = match self.auth.bearer().await {
            Ok(b) => b,
            Err(e) => {
                debug!(error = %e, "No usable NuHeat token this cycle");
                self.notices.raise(
                    PluginNotice::warning(
                        NOTICE_NOT_LINKED,
                        "Not signed in to a NuHeat account, so no thermostats are published.",
                    )
                    .with_remedy("Use the \"Link NuHeat account\" button on this page."),
                );
                return;
            }
        };
        self.notices.clear(NOTICE_NOT_LINKED);

        // The one-hour implicit token has no renewal path, so the only useful
        // thing to do is say so before it lapses rather than after.
        if self.auth.is_expiring_unrenewably() {
            self.notices.raise(
                PluginNotice::warning(
                    NOTICE_TOKEN_EXPIRING,
                    "This NuHeat token expires within the hour and cannot be renewed.",
                )
                .with_remedy(
                    "Use \"Link NuHeat account\" to paste a fresh one, or switch to a NuHeat \
                     client id so the plugin can keep itself signed in.",
                ),
            );
        } else if self.auth.mode() == AuthMode::OAuth {
            self.notices.clear(NOTICE_TOKEN_EXPIRING);
        }

        let thermostats = match self.api.thermostats(&bearer).await {
            Ok(list) => {
                self.clear_transport_notices();
                list
            }
            Err(e) => {
                self.raise_for(&e);
                // Nothing is published and nothing is reconciled. A failed
                // fetch says nothing about which thermostats exist.
                return;
            }
        };

        let wanted: Vec<&Thermostat> = thermostats
            .iter()
            .filter(|t| !t.serial_number.is_empty() && self.wanted(&t.serial_number))
            .collect();

        if wanted.is_empty() {
            self.notices.raise(
                PluginNotice::warning(
                    NOTICE_NO_THERMOSTATS,
                    if thermostats.is_empty() {
                        "This NuHeat account has no thermostats."
                    } else {
                        "No thermostats match the serial numbers this plugin is configured for."
                    },
                )
                .with_remedy(
                    "Check that the account is the one your thermostats are registered to, \
                     and that \"Only these serial numbers\" is empty or correct.",
                ),
            );
        } else {
            self.notices.clear(NOTICE_NO_THERMOSTATS);
        }

        let mut implausible = None;
        for t in &wanted {
            match self.publish(t, None).await {
                Ok(ScaleCheck::Implausible(wire)) => implausible = Some(wire),
                Ok(ScaleCheck::Ok) => {}
                Err(e) => {
                    warn!(serial = %t.serial_number, error = %e, "Could not publish a thermostat")
                }
            }
        }

        // The unit-scale tripwire. If this ever fires, the wire format is not
        // hundredths of a degree and every temperature this plugin publishes is
        // wrong by a factor of ten — worth an error notice rather than a log
        // line no one reads.
        match implausible {
            Some(wire) => self.notices.raise(
                PluginNotice::error(
                    NOTICE_SCALE,
                    format!(
                        "NuHeat reported a temperature ({wire}) that does not decode to a \
                         believable one. Readings are being withheld rather than published wrong."
                    ),
                )
                .with_remedy(
                    "This means NuHeat changed their temperature units. Please report it \
                     against hc-nuheat.",
                ),
            ),
            None => self.notices.clear(NOTICE_SCALE),
        }

        // Only now — after a fetch that succeeded — is it safe to retire what
        // is missing from it.
        let live: HashSet<String> = wanted
            .iter()
            .map(|t| device::device_id(&t.serial_number))
            .collect();
        match self.publisher.reconcile_devices(live).await {
            Ok(report) => {
                for id in &report.stale_unregistered {
                    info!(device_id = %id, "Retired a thermostat that is no longer on the account");
                }
            }
            Err(e) => warn!(error = %e, "Reconcile failed; stale devices may linger"),
        }
    }

    /// Register (idempotently), publish state and availability for one
    /// thermostat.
    ///
    /// `caused_by` carries the command that provoked this, when there was one,
    /// so the UI and the audit log can say what caused the change instead of
    /// showing an anonymous update.
    /// Returns the unit-scale verdict for this thermostat's readings, so the
    /// caller need not build the payload a second time to ask.
    async fn publish(
        &self,
        t: &Thermostat,
        caused_by: Option<&Value>,
    ) -> anyhow::Result<ScaleCheck> {
        let device_id = device::device_id(&t.serial_number);
        let name = t
            .name
            .as_deref()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(&t.serial_number);

        let first_time = {
            let mut guard = self.known.lock().expect("known mutex");
            let entry = guard.entry(t.serial_number.clone());
            matches!(entry, std::collections::hash_map::Entry::Vacant(_))
        };

        if first_time {
            self.publisher
                .register_device_full(
                    &device_id,
                    name,
                    Some("thermostat"),
                    self.area.as_deref(),
                    None,
                )
                .await?;
            // Registration and command subscription are separate calls, and
            // doing only the first is the classic silent failure: the device
            // appears, its state updates, and every command goes nowhere.
            self.publisher.subscribe_commands(&device_id).await?;
            // The schema carries the range clients render controls from, so it
            // has to be the *configured* range: a slider offering 30 °C on a
            // floor limited to 27 invites a command this plugin will silently
            // clamp.
            self.publisher
                .register_device_schema(&device_id, &device::device_schema(self.limits))
                .await?;
            info!(device_id = %device_id, name = %name, "Registered a NuHeat thermostat");
        }

        let (state, scale_check) = device::state_payload(t);

        {
            let mut guard = self.known.lock().expect("known mutex");
            let entry = guard.entry(t.serial_number.clone()).or_default();
            entry.setpoint_c = state["setpoint"].as_f64();
        }

        match caused_by {
            Some(command) => {
                self.publisher
                    .publish_state_for_command(&device_id, &state, command, "nuheat")
                    .await?
            }
            None => self.publisher.publish_state(&device_id, &state).await?,
        }

        // A thermostat NuHeat knows about but cannot reach is unavailable, not
        // absent — its rules and history stay put, its controls go grey.
        self.publisher.set_available(&device_id, t.online).await?;
        Ok(scale_check)
    }

    /// Apply one command to one device.
    pub async fn apply(&self, device_id: &str, payload: &Value) {
        let Some(serial) = self.serial_for(device_id) else {
            warn!(
                device_id,
                "Command for a thermostat this plugin does not own"
            );
            return;
        };

        let ctx = CommandContext {
            bare_setpoint_is_permanent: self.bare_setpoint_is_permanent,
            default_hold_hours: self.default_hold_hours,
            current_setpoint_c: self
                .known
                .lock()
                .expect("known mutex")
                .get(&serial)
                .and_then(|k| k.setpoint_c),
        };

        let intent = match device::interpret(payload, ctx, chrono::Utc::now()) {
            Ok(i) => i,
            Err(rejection) => {
                // Refused, not failed. The state is left exactly as it was, so
                // the UI snaps back to what the thermostat is really doing.
                warn!(device_id, %rejection, "Rejected a command");
                return;
            }
        };

        let bearer = match self.auth.bearer().await {
            Ok(b) => b,
            Err(e) => {
                warn!(device_id, error = %e, "Cannot apply a command without a token");
                return;
            }
        };

        if let Intent::Hold { celsius, .. } | Intent::PermanentHold { celsius } = &intent {
            if self.limits.would_clamp(*celsius) {
                info!(
                    device_id,
                    requested = celsius,
                    min = self.limits.min_c,
                    max = self.limits.max_c,
                    "Requested setpoint is outside the allowed range; clamping"
                );
            }
        }

        let result = match &intent {
            Intent::Auto => self.api.set_auto(&bearer, &serial).await,
            Intent::Hold {
                celsius,
                hold_until,
            } => {
                self.api
                    .set_hold(
                        &bearer,
                        &serial,
                        units::encode_celsius_within(*celsius, self.limits),
                        hold_until.as_deref(),
                    )
                    .await
            }
            Intent::PermanentHold { celsius } => {
                self.api
                    .set_permanent_hold(
                        &bearer,
                        &serial,
                        units::encode_celsius_within(*celsius, self.limits),
                    )
                    .await
            }
        };

        if let Err(e) = result {
            error!(device_id, error = %e, "NuHeat refused a command");
            self.raise_for(&e);
            // Deliberately no state publish. homeCore shows what the device is
            // actually doing, and it is still doing the old thing.
            return;
        }
        self.clear_transport_notices();

        // Read back what the thermostat now reports, rather than echoing the
        // command. This is the whole state contract in one call.
        tokio::time::sleep(WRITE_SETTLE).await;
        match self.api.thermostat(&bearer, &serial).await {
            Ok(t) => {
                if let Err(e) = self.publish(&t, Some(payload)).await {
                    warn!(device_id, error = %e, "Could not publish after a command");
                }
            }
            // The write succeeded; only the read-back did not. The next poll
            // publishes the truth, so this is a log line and not a notice.
            Err(e) => debug!(device_id, error = %e, "Could not read back after a command"),
        }
    }

    /// Turn an API failure into the notice that names it.
    fn raise_for(&self, e: &ApiError) {
        match e {
            ApiError::Unauthorized => {
                let remedy = match self.auth.mode() {
                    AuthMode::AccessToken => {
                        "Pasted NuHeat tokens last one hour. Use \"Link NuHeat account\" to \
                         paste a fresh one, or switch to a NuHeat client id so the plugin \
                         can keep itself signed in."
                    }
                    AuthMode::OAuth => {
                        "The refresh token may have expired (NuHeat expires them after about \
                         15 days of disuse). Use \"Link NuHeat account\" to sign in again."
                    }
                };
                self.notices.raise(
                    PluginNotice::error(
                        NOTICE_AUTH_REJECTED,
                        "NuHeat rejected this plugin's credentials.",
                    )
                    .with_remedy(remedy),
                );
            }
            ApiError::RateLimited { retry_after } => {
                let detail = retry_after
                    .map(|d| format!(" Retrying in about {} s.", d.as_secs()))
                    .unwrap_or_default();
                self.notices.raise(
                    PluginNotice::warning(
                        NOTICE_RATE_LIMITED,
                        format!("NuHeat is rate limiting this plugin.{detail}"),
                    )
                    .with_remedy("Increase \"Check every\" on this page if it keeps happening."),
                );
            }
            ApiError::Transport(msg) => {
                self.notices.raise(
                    PluginNotice::error(
                        NOTICE_UNREACHABLE,
                        format!("Cannot reach the NuHeat cloud: {msg}"),
                    )
                    .with_remedy("Check this machine's internet connection."),
                );
            }
            ApiError::Api { status, body } => {
                self.notices.raise(PluginNotice::error(
                    NOTICE_UNREACHABLE,
                    format!("The NuHeat API returned {status}: {body}"),
                ));
            }
        }
    }

    /// Everything a successful call disproves.
    fn clear_transport_notices(&self) {
        self.notices.clear(NOTICE_UNREACHABLE);
        self.notices.clear(NOTICE_RATE_LIMITED);
        self.notices.clear(NOTICE_AUTH_REJECTED);
    }

    /// A snapshot for the `status` management action.
    pub fn status(&self) -> Value {
        let guard = self.known.lock().expect("known mutex");
        serde_json::json!({
            "configured": self.auth.is_configured(),
            "linked": self.auth.is_linked(),
            "auth_mode": match self.auth.mode() {
                AuthMode::AccessToken => "access_token",
                AuthMode::OAuth => "oauth",
            },
            "token_expires_in_secs": self.auth.expires_in().map(|d| d.num_seconds()),
            "thermostats": guard.len(),
            "setpoint_min_c": self.limits.min_c,
            "setpoint_max_c": self.limits.max_c,
        })
    }
}

/// Modes are published as names; this keeps the mapping honest at compile time
/// by using the same enum the API module parses with.
#[allow(dead_code)]
fn _mode_names_are_shared(m: Mode) -> &'static str {
    m.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NuHeatSection;

    fn runtime(cfg: NuHeatSection) -> Runtime {
        let (api, http) = NuHeatApi::new().expect("client builds");
        let auth = Arc::new(Auth::new(
            AuthMode::AccessToken,
            None,
            None,
            None,
            http,
            plugin_sdk_rs::PluginStateWriter::test_instance("plugin.nuheat"),
        ));
        Runtime::new(
            api,
            auth,
            DevicePublisher::test_instance("plugin.nuheat"),
            PluginNotices::test_instance(),
            &cfg,
        )
    }

    #[test]
    fn an_empty_serial_filter_publishes_everything() {
        let rt = runtime(NuHeatSection::default());
        assert!(rt.wanted("12345678"));
        assert!(rt.wanted("anything"));
    }

    #[test]
    fn a_serial_filter_is_case_and_whitespace_insensitive() {
        let rt = runtime(NuHeatSection {
            only_serials: vec![" ABC123 ".into()],
            ..NuHeatSection::default()
        });
        assert!(rt.wanted("abc123"));
        assert!(rt.wanted("ABC123"));
        assert!(!rt.wanted("other"));
    }

    /// An empty string in the list is an operator deleting a row badly. Taken
    /// literally it would filter every thermostat out and leave them wondering
    /// where their devices went.
    #[test]
    fn blank_entries_in_the_serial_filter_are_ignored() {
        let rt = runtime(NuHeatSection {
            only_serials: vec!["".into(), "  ".into()],
            ..NuHeatSection::default()
        });
        assert!(rt.wanted("anything"));
    }

    /// A fresh install is unconfigured *and* unlinked, and the status has to
    /// distinguish them — they send an operator to different places.
    #[test]
    fn a_status_snapshot_reports_what_the_plugin_knows() {
        let rt = runtime(NuHeatSection::default());
        let status = rt.status();
        assert_eq!(status["configured"], serde_json::json!(false));
        assert_eq!(status["linked"], serde_json::json!(false));
        assert_eq!(status["auth_mode"], serde_json::json!("access_token"));
        assert_eq!(status["thermostats"], serde_json::json!(0));
    }
}
