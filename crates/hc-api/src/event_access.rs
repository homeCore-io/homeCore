//! Who may see which event.
//!
//! The live stream (`GET /events/stream`) and the recent-events log
//! (`GET /events`) both hand out events from the bus. An event says what a
//! resource is or did, so it is shown to whoever may read that resource
//! through its own REST endpoint, and to nobody else: a device event to a
//! caller who may `GET /devices`, a rule or webhook event to one who may
//! `GET /automations`, and so on. Both paths ask [`view`] for every event, so
//! they cannot disagree.
//!
//! **Why this matters.** Both used to send everything to anyone signed in. A
//! key minted for one narrow job (`devices:read`, say, for a wall panel)
//! could read every rule as it was saved, and every webhook call, whose path
//! is the webhook's shared secret: holding it is enough to fire the rules
//! behind it.
//!
//! [`audience`] matches every [`Event`] variant without a wildcard, so a new
//! variant does not compile until somebody decides who may see it. A
//! `Custom` event is judged by its `event_type`; one not named in
//! [`custom_audience`] (a rule's own `FireEvent`, anything new) is for
//! admins only, so a new kind is never open by default.

use hc_auth::Claims;
use hc_types::event::Event;
use serde_json::{json, Value};

/// Who may see an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audience {
    /// Callers holding this scope.
    Scope(&'static str),
    /// Callers holding any of these scopes.
    AnyScope(&'static [&'static str]),
    /// `plugin_capabilities`: the whole manifest to `plugins:read`, which
    /// `GET /plugins/{id}/capabilities` requires; only the plugin's own
    /// screens (`ui`) to `devices:read`, which `GET /plugins/ui` requires.
    PluginManifest,
    /// `device_command_sent`: whole to whoever may command devices; to
    /// `devices:read` with the command's secrets masked. A rule that sets a
    /// lock's code sends it in a command, and seeing the house is not
    /// knowing its codes.
    DeviceCommand,
    /// Admins only.
    Admin,
}

/// How much of an event a caller may see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// All of it.
    Whole,
    /// A `plugin_capabilities` reduced to its plugin id and screens.
    PluginUi,
    /// A `device_command_sent` with its command's secrets masked.
    CommandMasked,
    /// None of it.
    Nothing,
}

/// The read scope each REST resource requires, named once.
const DEVICES: &str = "devices:read";
const AUTOMATIONS: &str = "automations:read";
const SCENES: &str = "scenes:read";
const AREAS: &str = "areas:read";
const PLUGINS: &str = "plugins:read";

/// Who may see `event`.
pub fn audience(event: &Event) -> Audience {
    match event {
        // GET /devices, /devices/{id}, /devices/{id}/schema. Timers and modes
        // are devices (GET /glue, GET /modes need devices:read), and so are
        // the commands sent to devices.
        Event::DeviceStateChanged { .. }
        | Event::DeviceAvailabilityChanged { .. }
        | Event::DeviceNameChanged { .. }
        | Event::DeviceAdded { .. }
        | Event::DeviceRemoved { .. }
        | Event::DeviceMetadataChanged { .. }
        | Event::DeviceSchemaChanged { .. }
        | Event::DeviceBatteryLow { .. }
        | Event::DeviceBatteryRecovered { .. }
        | Event::TimerStateChanged { .. }
        | Event::ModeChanged { .. } => Audience::Scope(DEVICES),

        Event::DeviceCommandSent { .. } => Audience::DeviceCommand,

        // GET /alerts needs devices:read; what plugins report feeds it.
        Event::AlertRaised { .. }
        | Event::AlertCleared { .. }
        | Event::AlertAcknowledged { .. }
        | Event::PluginAlerts { .. } => Audience::Scope(DEVICES),

        // GET /automations: rules as written, and what they did.
        Event::RuleFired { .. }
        | Event::RotationChanged { .. }
        | Event::RotationRemoved { .. }
        | Event::RotationAdvanced { .. }
        | Event::RuleChanged { .. }
        | Event::RuleRemoved { .. }
        | Event::RuleEvaluationFailed { .. }
        | Event::ActionFailed { .. } => Audience::Scope(AUTOMATIONS),

        // GET /scenes.
        Event::SceneActivated { .. } | Event::SceneChanged { .. } | Event::SceneRemoved { .. } => {
            Audience::Scope(SCENES)
        }

        // GET /areas, GET /areas/{id}/saved-lights.
        Event::RoomChanged { .. } | Event::RoomRemoved { .. } | Event::RoomLightsChanged { .. } => {
            Audience::Scope(AREAS)
        }

        // A plugin coming, going or changing status carries its id and a
        // status word. `GET /plugins/status` needs plugins:read, but
        // `GET /plugins/ui` (devices:read) already says which plugins there
        // are, and the app reloads plugin screens on these for everyone who
        // can see the house, household members included.
        Event::PluginRegistered { .. }
        | Event::PluginOffline { .. }
        | Event::PluginStatusChanged { .. } => Audience::AnyScope(&[PLUGINS, DEVICES]),

        // Version, uptime and the plugin's notices about itself:
        // GET /plugins/status.
        Event::PluginHeartbeat { .. } => Audience::Scope(PLUGINS),

        Event::PluginCapabilities { .. } => Audience::PluginManifest,

        // Nothing publishes these today and no GET serves them; whoever adds
        // one decides who it is for.
        Event::SystemAlert { .. } | Event::MqttMessage { .. } => Audience::Admin,

        Event::Custom { event_type, .. } => custom_audience(event_type),
    }
}

/// Who may see a `Custom` event of this `event_type`.
pub fn custom_audience(event_type: &str) -> Audience {
    match event_type {
        // A webhook call carries the webhook's path, its shared secret. The
        // rule that listens for it is at GET /automations.
        "webhook"
        // The engine's own requests to run a rule now.
        | "scheduler_tick"
        | "deferred_rule_fire"
        | hc_core::engine::MANUAL_RUN_EVENT
        // GET /automations/variables.
        | "hub_variable_changed"
        // What `SystemStarted` triggers listen for.
        | "system_started" => Audience::Scope(AUTOMATIONS),

        // The device-side twins of mode_changed and device_removed.
        "mode_changed" | "device_deleted" => Audience::Scope(DEVICES),

        // A rule's own `FireEvent`, a `PingHost` answer, or a kind nobody has
        // classified yet: its payload can be anything a rule puts in it.
        _ => Audience::Admin,
    }
}

/// How much of an event for `audience` this caller may see.
pub fn view(claims: &Claims, audience: Audience) -> View {
    if claims.is_admin() {
        return View::Whole;
    }
    match audience {
        Audience::Scope(scope) if claims.has_scope(scope) => View::Whole,
        Audience::AnyScope(scopes) if scopes.iter().any(|s| claims.has_scope(s)) => View::Whole,
        Audience::PluginManifest if claims.has_scope(PLUGINS) => View::Whole,
        Audience::PluginManifest if claims.has_scope(DEVICES) => View::PluginUi,
        Audience::DeviceCommand
            if claims.has_scope("devices:write") || claims.has_scope("devices:command") =>
        {
            View::Whole
        }
        Audience::DeviceCommand if claims.has_scope(DEVICES) => View::CommandMasked,
        _ => View::Nothing,
    }
}

/// A `plugin_capabilities` event cut down to what `GET /plugins/ui` shows:
/// which plugin, and its screens. The rest (its actions, its config schema
/// and descriptor) is for `plugins:read`.
pub fn plugin_ui_only(event: &Value) -> Value {
    let mut out = json!({
        "type": event.get("type").cloned().unwrap_or(Value::Null),
        "timestamp": event.get("timestamp").cloned().unwrap_or(Value::Null),
        "plugin_id": event.get("plugin_id").cloned().unwrap_or(Value::Null),
    });
    if let Some(ui) = event.get("ui") {
        out["ui"] = ui.clone();
    }
    out
}

/// A `device_command_sent` with every secret in its `command` masked: the
/// keys config redaction masks, and any code (`code`, `user_code`).
pub fn command_masked(event: &Value) -> Value {
    fn mask(v: &mut Value, secret: bool) {
        match v {
            Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    let k = k.to_ascii_lowercase();
                    mask(v, crate::handlers::is_secret_key(&k) || k.contains("code"));
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| mask(v, secret)),
            _ if secret => *v = Value::String(crate::handlers::REDACTED_SECRET.into()),
            _ => {}
        }
    }
    let mut out = event.clone();
    if let Some(command) = out.get_mut("command") {
        mask(command, false);
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::Utc;
    use hc_auth::user::{display_scopes, Role};
    use hc_auth::Actor;
    use hc_types::device::{Area, DeviceChange, DeviceState};

    /// A user signed in with `role`.
    pub(crate) fn user(role: Role) -> Claims {
        Claims {
            sub: "someone".into(),
            uid: uuid::Uuid::new_v4().to_string(),
            exp: u64::MAX,
            role,
            scopes: role.scopes(),
            actor: None,
            tv: 0,
            sid: None,
        }
    }

    /// An API key holding exactly `scopes`, shaped as `verify_api_key`
    /// shapes one (decorative `Admin` role).
    pub(crate) fn key(scopes: &[&str]) -> Claims {
        Claims {
            sub: "api_key:test".into(),
            uid: uuid::Uuid::new_v4().to_string(),
            exp: u64::MAX,
            role: Role::Admin,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            actor: Some(Actor::ApiKey {
                id: uuid::Uuid::new_v4(),
                owner_uid: uuid::Uuid::new_v4(),
                label: "test".into(),
            }),
            tv: 0,
            sid: None,
        }
    }

    fn now() -> chrono::DateTime<Utc> {
        Utc::now()
    }

    /// Seeing the house shows a command sent to a lock, not the code in it.
    #[test]
    fn a_command_is_whole_to_who_may_send_one_and_masked_to_readers() {
        let event = Event::DeviceCommandSent {
            timestamp: now(),
            device_id: "lock.front".into(),
            device_name: None,
            command: json!({ "action": "set_code", "slot": 2, "user_code": "4321", "pin": 99 }),
            source: "rule".into(),
            source_id: None,
            correlation_id: None,
        };
        let a = audience(&event);
        assert_eq!(view(&key(&["devices:command"]), a), View::Whole);
        assert_eq!(view(&user(Role::Admin), a), View::Whole);
        assert_eq!(view(&key(&["devices:read"]), a), View::CommandMasked);
        assert_eq!(view(&key(&["scenes:read"]), a), View::Nothing);

        let shown = command_masked(&serde_json::to_value(&event).unwrap());
        assert_eq!(shown["command"]["action"], json!("set_code"));
        assert_eq!(shown["command"]["slot"], json!(2));
        assert_eq!(
            shown["command"]["user_code"],
            json!(crate::handlers::REDACTED_SECRET)
        );
        assert_eq!(
            shown["command"]["pin"],
            json!(crate::handlers::REDACTED_SECRET)
        );
        assert_eq!(shown["device_id"], json!("lock.front"));
    }

    fn custom(event_type: &str) -> Event {
        Event::Custom {
            timestamp: now(),
            event_type: event_type.into(),
            payload: json!({ "path": "s3cret-path" }),
        }
    }

    pub(crate) fn device_event() -> Event {
        Event::DeviceStateChanged {
            timestamp: now(),
            device_id: "light.kitchen".into(),
            device_name: None,
            previous: Default::default(),
            current: Default::default(),
            changed: vec![],
            change: DeviceChange::unknown(),
        }
    }

    pub(crate) fn webhook_event() -> Event {
        custom("webhook")
    }

    pub(crate) fn rule_removed() -> Event {
        Event::RuleRemoved {
            timestamp: now(),
            rule_id: uuid::Uuid::new_v4(),
        }
    }

    fn capabilities() -> Event {
        serde_json::from_value(json!({
            "type": "plugin_capabilities",
            "timestamp": now(),
            "plugin_id": "plugin.hue",
            "capabilities": {
                "spec": "1",
                "plugin_id": "plugin.hue",
                "actions": [],
                "api_scopes": ["devices:read"],
            },
            "config_schema": { "type": "object" },
        }))
        .expect("a plugin_capabilities event")
    }

    /// One event of each kind a REST resource serves, with the scope that
    /// resource's GET requires. Written from the router's side, not from
    /// [`audience`], so the two can be checked against each other.
    fn by_resource() -> Vec<(&'static str, &'static str, Event)> {
        let device = DeviceState::new("light.kitchen", "Kitchen", "plugin.hue");
        let room: Area =
            serde_json::from_value(json!({ "id": uuid::Uuid::new_v4(), "name": "Kitchen" }))
                .expect("an area");
        vec![
            ("GET /devices", DEVICES, device_event()),
            (
                "GET /devices",
                DEVICES,
                Event::DeviceAvailabilityChanged {
                    timestamp: now(),
                    device_id: "light.kitchen".into(),
                    device_name: None,
                    available: false,
                },
            ),
            (
                "GET /devices",
                DEVICES,
                Event::DeviceAdded {
                    timestamp: now(),
                    device: Box::new(device.clone()),
                },
            ),
            (
                "GET /devices",
                DEVICES,
                Event::DeviceMetadataChanged {
                    timestamp: now(),
                    device: Box::new(device),
                },
            ),
            (
                "GET /devices",
                DEVICES,
                Event::DeviceRemoved {
                    timestamp: now(),
                    device_id: "light.kitchen".into(),
                },
            ),
            (
                "GET /devices/{id}/schema",
                DEVICES,
                Event::DeviceSchemaChanged {
                    timestamp: now(),
                    device_id: "light.kitchen".into(),
                    attributes: vec![],
                    actions: vec![],
                },
            ),
            (
                "GET /modes",
                DEVICES,
                Event::ModeChanged {
                    timestamp: now(),
                    mode_id: "mode_night".into(),
                    mode_name: "Night".into(),
                    on: true,
                },
            ),
            ("GET /modes", DEVICES, custom("mode_changed")),
            (
                "GET /glue",
                DEVICES,
                Event::TimerStateChanged {
                    timestamp: now(),
                    timer_id: "timer_tea".into(),
                    timer_name: None,
                    state: "running".into(),
                    remaining_secs: Some(60),
                },
            ),
            (
                "GET /alerts",
                DEVICES,
                Event::AlertCleared {
                    timestamp: now(),
                    alert_id: "a1".into(),
                    device_id: "light.kitchen".into(),
                    alert: None,
                },
            ),
            ("GET /automations", AUTOMATIONS, rule_removed()),
            (
                "GET /automations",
                AUTOMATIONS,
                Event::RuleFired {
                    timestamp: now(),
                    rule_id: "r1".into(),
                    rule_name: "r".into(),
                    trigger_type: "WebhookReceived".into(),
                    action_count: 1,
                    elapsed_ms: None,
                    correlation_id: None,
                },
            ),
            ("GET /automations", AUTOMATIONS, webhook_event()),
            (
                "GET /automations/variables",
                AUTOMATIONS,
                custom("hub_variable_changed"),
            ),
            (
                "GET /scenes",
                SCENES,
                Event::SceneActivated {
                    timestamp: now(),
                    scene_id: "s1".into(),
                    scene_name: "Evening".into(),
                },
            ),
            (
                "GET /scenes",
                SCENES,
                Event::SceneRemoved {
                    timestamp: now(),
                    scene_id: uuid::Uuid::new_v4(),
                },
            ),
            (
                "GET /areas",
                AREAS,
                Event::RoomChanged {
                    timestamp: now(),
                    room,
                },
            ),
            (
                "GET /areas",
                AREAS,
                Event::RoomRemoved {
                    timestamp: now(),
                    room_id: uuid::Uuid::new_v4(),
                    merged_into: None,
                },
            ),
            (
                "GET /plugins/status",
                PLUGINS,
                Event::PluginStatusChanged {
                    timestamp: now(),
                    plugin_id: "plugin.hue".into(),
                    status: "active".into(),
                    previous_status: "starting".into(),
                },
            ),
            ("GET /plugins/{id}/capabilities", PLUGINS, capabilities()),
            (
                "GET /plugins/status",
                PLUGINS,
                Event::PluginHeartbeat {
                    timestamp: now(),
                    plugin_id: "plugin.hue".into(),
                    version: None,
                    sdk_version: None,
                    uptime_secs: None,
                    device_count: None,
                    notices: vec![],
                },
            ),
            // The app reloads plugin screens on these, and GET /plugins/ui
            // needs only devices:read.
            (
                "GET /plugins/ui",
                DEVICES,
                Event::PluginRegistered {
                    timestamp: now(),
                    plugin_id: "plugin.hue".into(),
                },
            ),
            (
                "GET /plugins/ui",
                DEVICES,
                Event::PluginOffline {
                    timestamp: now(),
                    plugin_id: "plugin.hue".into(),
                },
            ),
            ("GET /plugins/ui", DEVICES, capabilities()),
        ]
    }

    #[test]
    fn every_role_sees_the_events_of_everything_it_can_read() {
        let mut callers: Vec<(String, Claims)> = Role::all()
            .into_iter()
            .map(|r| (r.wire().to_string(), user(r)))
            .collect();
        let display: Vec<String> = display_scopes();
        let display: Vec<&str> = display.iter().map(String::as_str).collect();
        callers.push(("display key".into(), key(&display)));

        for (who, claims) in &callers {
            for (route, scope, event) in by_resource() {
                if claims.has_scope(scope) {
                    assert_ne!(
                        view(claims, audience(&event)),
                        View::Nothing,
                        "{who} can {route} but would not see {}",
                        crate::event_log::event_type_name(&event)
                    );
                }
            }
        }
    }

    #[test]
    fn the_household_member_keeps_what_the_app_listens_for() {
        // The events the app (hc-web-svelte) acts on, for the role with the
        // least the app supports a person signing in with.
        let member = user(Role::HouseholdMember);
        for (_, _, event) in by_resource() {
            let name = crate::event_log::event_type_name(&event);
            let expected = match name {
                "plugin_heartbeat" => View::Nothing,
                "plugin_capabilities" => View::PluginUi,
                _ => View::Whole,
            };
            assert_eq!(view(&member, audience(&event)), expected, "{name}");
        }
    }

    #[test]
    fn a_device_reading_key_sees_devices_and_nothing_about_rules_or_webhooks() {
        let k = key(&["devices:read"]);
        assert_eq!(view(&k, audience(&device_event())), View::Whole);
        assert_eq!(view(&k, audience(&webhook_event())), View::Nothing);
        assert_eq!(view(&k, audience(&rule_removed())), View::Nothing);
        assert_eq!(view(&k, audience(&custom("scheduler_tick"))), View::Nothing);
        assert_eq!(
            view(&k, audience(&custom("manual_rule_fire"))),
            View::Nothing
        );
    }

    #[test]
    fn a_key_without_devices_read_sees_no_device_events() {
        let k = key(&["automations:read"]);
        assert_eq!(view(&k, audience(&device_event())), View::Nothing);
        assert_eq!(view(&k, audience(&webhook_event())), View::Whole);
    }

    #[test]
    fn an_admin_sees_everything() {
        let admin = user(Role::Admin);
        let admin_key = key(&Role::Admin
            .scopes()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>());
        for claims in [
            &admin,
            &admin_key,
            &crate::auth_middleware::whitelist_claims(),
        ] {
            for (_, _, event) in by_resource() {
                assert_eq!(view(claims, audience(&event)), View::Whole);
            }
            for e in [custom("my_own_event"), custom("webhook")] {
                assert_eq!(view(claims, audience(&e)), View::Whole);
            }
        }
    }

    #[test]
    fn an_unknown_custom_event_is_for_admins() {
        let e = custom("doorbell_pressed");
        assert_eq!(audience(&e), Audience::Admin);
        for role in Role::all().into_iter().filter(|r| *r != Role::Admin) {
            assert_eq!(
                view(&user(role), audience(&e)),
                View::Nothing,
                "{}",
                role.wire()
            );
        }
    }

    #[test]
    fn a_plugin_manifest_is_cut_to_its_screens_for_device_readers() {
        let wire = serde_json::to_value(capabilities()).unwrap();
        let k = key(&["devices:read"]);
        assert_eq!(view(&k, Audience::PluginManifest), View::PluginUi);
        let seen = plugin_ui_only(&wire);
        assert_eq!(seen["type"], "plugin_capabilities");
        assert_eq!(seen["plugin_id"], "plugin.hue");
        assert!(seen.get("config_schema").is_none());
        assert!(seen.get("capabilities").is_none());

        let plugins = key(&["plugins:read"]);
        assert_eq!(view(&plugins, Audience::PluginManifest), View::Whole);
        let scenes = key(&["scenes:read"]);
        assert_eq!(view(&scenes, Audience::PluginManifest), View::Nothing);
    }

    #[test]
    fn no_event_a_non_author_sees_carries_a_webhook_path() {
        // The webhook path is the secret. Only callers who may read rules may
        // see an event carrying one: the webhook call itself and the rule.
        for scopes in [
            &["devices:read"][..],
            &["devices:read", "scenes:read", "areas:read", "plugins:read"][..],
        ] {
            let k = key(scopes);
            for e in [
                webhook_event(),
                rule_removed(),
                custom("deferred_rule_fire"),
            ] {
                assert_eq!(view(&k, audience(&e)), View::Nothing);
            }
        }
    }
}
