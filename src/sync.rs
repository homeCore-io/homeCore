use anyhow::Result;
use serde_json::json;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use tracing::{debug, warn};

use crate::config::{HueDisplayConfig, IlluminanceDisplay, TemperatureUnit};
use crate::hue::api::HueApiClient;
use crate::hue::models::{BridgeSnapshot, HueAuxDevice, HueGroupedLight};
use crate::hue::registry::HueRegistry;
use crate::translator;
use plugin_sdk_rs::types::schema::{AttributeKind, AttributeSchema, DeviceSchema};
use plugin_sdk_rs::DevicePublisher;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventApplyOutcome {
    Applied,
    Ignored { reason: String },
    NeedsRefresh { reason: String },
}

/// Aggregates display and feature-flag config for sync operations, avoiding
/// the need to pass 7+ separate arguments through every call site.
pub struct SyncConfig {
    pub display: HueDisplayConfig,
    pub compact_motion_facets: bool,
    pub publish_grouped_lights: bool,
    pub publish_grouped_lights_for: Vec<String>,
    pub skip_grouped_lights_for: Vec<String>,
    pub publish_bridge_home: bool,
    pub publish_entertainment_configurations: bool,
}

pub async fn refresh_bridge_state(
    publisher: &DevicePublisher,
    registry: &mut HueRegistry,
    api: &HueApiClient,
    cfg: &SyncConfig,
) -> Result<()> {
    let target = api.target();
    let device_id = target.device_id();

    match api.fetch_bridge_summary().await {
        Ok(summary) => {
            let state = translator::bridge_state(target, true, summary.clone());
            publisher.publish_state(&device_id, &state).await?;
            publisher.publish_availability(&device_id, true).await?;
            registry.upsert_bridge(
                device_id.clone(),
                BridgeSnapshot {
                    online: true,
                    summary,
                },
            );
            debug!(device_id, "Hue bridge state refreshed");

            let lights = api.fetch_lights().await?;
            let light_owner_to_device = build_light_owner_map(&lights);
            let mut published_light_ids: HashSet<String> = HashSet::new();
            for light in lights {
                published_light_ids.insert(light.device_id.clone());
                if registry.ensure_light(&light) {
                    publisher
                        .register_device_full(
                            &light.device_id,
                            &light.name,
                            Some("light"),
                            // The Hue room, so moving a light between rooms in
                            // the Hue app moves it in homeCore too. Core keeps
                            // this as the plugin-delivered area; a user's
                            // `area_override` still wins.
                            light.area.as_deref(),
                            Some(translator::light_capabilities(&light)),
                        )
                        .await?;
                    publisher.subscribe_commands(&light.device_id).await?;
                    // Publish capability schema for the UI.
                    let light_schema = build_light_schema();
                    publisher
                        .register_device_schema(&light.device_id, &light_schema)
                        .await
                        .ok();
                    debug!(device_id = %light.device_id, name = %light.name, "Registered Hue light device");
                }

                publisher
                    .publish_availability(&light.device_id, true)
                    .await?;
                publisher
                    .publish_state(&light.device_id, &translator::light_state(&light))
                    .await?;
            }
            for stale_light_id in registry.prune_lights_not_in(&published_light_ids) {
                publisher
                    .unregister_device(publisher.plugin_id(), &stale_light_id)
                    .await?;
                debug!(device_id = %stale_light_id, "Unregistered stale Hue light");
            }

            if cfg.publish_grouped_lights || !cfg.publish_grouped_lights_for.is_empty() {
                let groups = api.fetch_grouped_lights().await?;
                let mut published_group_ids: HashSet<String> = HashSet::new();
                for group in groups {
                    if !should_publish_grouped_light(
                        &group,
                        cfg.publish_grouped_lights,
                        &cfg.publish_grouped_lights_for,
                        &cfg.skip_grouped_lights_for,
                    ) {
                        continue;
                    }
                    published_group_ids.insert(group.device_id.clone());

                    if registry.ensure_group(&group) {
                        publisher
                            .register_device_full(
                                &group.device_id,
                                &group.name,
                                Some("group"),
                                group.area.as_deref(),
                                Some(translator::grouped_light_capabilities()),
                            )
                            .await?;
                        publisher.subscribe_commands(&group.device_id).await?;
                        // Publish capability schema for the UI.
                        let group_schema = build_group_schema();
                        publisher
                            .register_device_schema(&group.device_id, &group_schema)
                            .await
                            .ok();
                        debug!(device_id = %group.device_id, name = %group.name, "Registered Hue grouped-light device");
                    }

                    publisher
                        .publish_availability(&group.device_id, true)
                        .await?;
                    publisher
                        .publish_state(&group.device_id, &translator::grouped_light_state(&group))
                        .await?;
                }
                for stale_group_id in registry.prune_groups_not_in(&published_group_ids) {
                    publisher
                        .unregister_device(publisher.plugin_id(), &stale_group_id)
                        .await?;
                }
            } else {
                for stale_group_id in registry.prune_groups_not_in(&HashSet::new()) {
                    publisher
                        .unregister_device(publisher.plugin_id(), &stale_group_id)
                        .await?;
                }
            }

            let scenes = api.fetch_scenes().await?;
            let mut published_scene_ids: HashSet<String> = HashSet::new();
            for scene in scenes {
                published_scene_ids.insert(scene.device_id.clone());
                if registry.ensure_scene(&scene) {
                    publisher
                        .register_device_full(
                            &scene.device_id,
                            &scene.name,
                            Some("scene"),
                            scene.area.as_deref(),
                            Some(translator::scene_capabilities()),
                        )
                        .await?;
                    publisher.subscribe_commands(&scene.device_id).await?;
                    debug!(device_id = %scene.device_id, name = %scene.name, "Registered Hue scene device");
                }

                publisher
                    .publish_availability(&scene.device_id, true)
                    .await?;
                publisher
                    .publish_state(&scene.device_id, &translator::scene_state(&scene))
                    .await?;
            }
            for stale_scene_id in registry.prune_scenes_not_in(&published_scene_ids) {
                publisher
                    .unregister_device(publisher.plugin_id(), &stale_scene_id)
                    .await?;
                debug!(device_id = %stale_scene_id, "Unregistered stale Hue scene");
            }

            let aux_devices = api.fetch_aux_devices().await?;
            let primary_aux_owner_to_device = build_primary_aux_owner_map(&aux_devices);
            let aux_registration_specs = build_aux_registration_specs(
                &aux_devices,
                &primary_aux_owner_to_device,
                &light_owner_to_device,
                cfg.compact_motion_facets,
                cfg.publish_bridge_home,
                cfg.publish_entertainment_configurations,
            );
            let mut registered_publish_ids: HashSet<String> = HashSet::new();
            let mut full_state_published_ids: HashSet<String> = HashSet::new();
            let mut compacted_partial_patches: HashMap<String, serde_json::Map<String, Value>> =
                HashMap::new();
            let mut active_aux_raw_ids: HashSet<String> = HashSet::new();
            let mut stale_publish_ids: HashSet<String> = HashSet::new();
            for aux in aux_devices {
                let publish_device_id = if cfg.compact_motion_facets {
                    compact_publish_device_id(
                        &aux,
                        &primary_aux_owner_to_device,
                        &light_owner_to_device,
                    )
                } else {
                    aux.device_id.clone()
                };

                if should_skip_aux_device(
                    &aux,
                    &publish_device_id,
                    cfg.publish_bridge_home,
                    cfg.publish_entertainment_configurations,
                ) {
                    continue;
                }
                active_aux_raw_ids.insert(aux.device_id.clone());

                let upsert = registry.upsert_aux(&aux, &publish_device_id);
                let binding_changed = upsert.previous_publish_device_id.is_some();
                if let Some(previous_publish_device_id) = upsert.previous_publish_device_id {
                    stale_publish_ids.insert(previous_publish_device_id);
                }

                if (upsert.newly_seen || binding_changed)
                    && registered_publish_ids.insert(publish_device_id.clone())
                    && !registry.is_primary_device_id(&publish_device_id)
                {
                    let registration = aux_registration_specs.get(&publish_device_id);
                    publisher
                        .register_device_full(
                            &publish_device_id,
                            registration
                                .map(|spec| spec.name.as_str())
                                .unwrap_or(aux.name.as_str()),
                            Some(
                                registration
                                    .map(|spec| spec.device_type.as_str())
                                    .unwrap_or(aux_device_type(&aux.resource_type)),
                            ),
                            None,
                            registration
                                .map(|spec| Value::Object(spec.capabilities.clone()))
                                .or_else(|| Some(translator::aux_capabilities(&aux))),
                        )
                        .await?;
                    debug!(device_id = %publish_device_id, name = %aux.name, kind = %aux.resource_type, "Registered Hue auxiliary device");
                }

                publisher
                    .publish_availability(&publish_device_id, aux_is_available(&aux))
                    .await?;

                // Skip state publish for zigbee_connectivity compacted onto lights —
                // connectivity_status is diagnostic noise that triggers a device_state_changed
                // event on every refresh cycle.  Availability is already tracked separately.
                if aux.resource_type == "zigbee_connectivity" && publish_device_id != aux.device_id
                {
                    continue;
                }

                let mut state = translator::aux_state(&aux);
                apply_display_preferences(&mut state, &aux.resource_type, &cfg.display);

                let compacted = cfg.compact_motion_facets && publish_device_id != aux.device_id;
                if compacted {
                    strip_aux_metadata(&mut state);
                    if let Some(obj) = state.as_object() {
                        compacted_partial_patches
                            .entry(publish_device_id.clone())
                            .or_default()
                            .extend(obj.clone());
                    }
                } else if full_state_published_ids.insert(publish_device_id.clone()) {
                    publisher.publish_state(&publish_device_id, &state).await?;
                } else {
                    strip_aux_metadata(&mut state);
                    publisher
                        .publish_state_partial(&publish_device_id, &state)
                        .await?;
                }
            }

            for (device_id, patch) in compacted_partial_patches {
                if !patch.is_empty() {
                    publisher
                        .publish_state_partial(&device_id, &Value::Object(patch))
                        .await?;
                }
            }

            for removed in registry.prune_aux_not_in(&active_aux_raw_ids) {
                stale_publish_ids.insert(removed.publish_device_id);
            }

            for stale_publish_id in stale_publish_ids {
                if !registry.is_primary_device_id(&stale_publish_id)
                    && !registry.is_aux_publish_device_id_referenced(&stale_publish_id)
                {
                    publisher
                        .unregister_device(publisher.plugin_id(), &stale_publish_id)
                        .await?;
                }
            }
        }
        Err(e) => {
            publisher.publish_availability(&device_id, false).await?;
            warn!(device_id, error = %e, "Failed to refresh Hue bridge state");
        }
    }

    Ok(())
}

fn make_attr(
    kind: AttributeKind,
    writable: bool,
    display_name: &str,
    unit: Option<&str>,
    min: Option<f64>,
    max: Option<f64>,
    step: Option<f64>,
) -> AttributeSchema {
    AttributeSchema {
        kind,
        writable,
        display_name: Some(display_name.to_string()),
        unit: unit.map(str::to_string),
        min,
        max,
        step,
        options: None,
    }
}

fn build_light_schema() -> DeviceSchema {
    let mut attrs = HashMap::new();
    attrs.insert(
        "on".into(),
        make_attr(AttributeKind::Bool, true, "Power", None, None, None, None),
    );
    attrs.insert(
        "brightness_pct".into(),
        make_attr(
            AttributeKind::Integer,
            true,
            "Brightness",
            Some("%"),
            Some(1.0),
            Some(100.0),
            Some(1.0),
        ),
    );
    attrs.insert(
        "color_temp".into(),
        make_attr(
            AttributeKind::ColorTemp,
            true,
            "Colour Temperature",
            Some("K"),
            Some(2000.0),
            Some(6535.0),
            Some(100.0),
        ),
    );
    attrs.insert(
        "color_xy".into(),
        AttributeSchema {
            kind: AttributeKind::ColorXy,
            writable: true,
            display_name: Some("Colour".into()),
            unit: None,
            min: None,
            max: None,
            step: None,
            options: None,
        },
    );
    DeviceSchema { attributes: attrs }
}

fn build_group_schema() -> DeviceSchema {
    let mut attrs = HashMap::new();
    attrs.insert(
        "on".into(),
        make_attr(AttributeKind::Bool, true, "Power", None, None, None, None),
    );
    attrs.insert(
        "brightness_pct".into(),
        make_attr(
            AttributeKind::Integer,
            true,
            "Brightness",
            Some("%"),
            Some(1.0),
            Some(100.0),
            Some(1.0),
        ),
    );
    DeviceSchema { attributes: attrs }
}

/// Map a Hue auxiliary resource type to the canonical homeCore device
/// type. Aligning with the names in
/// `core/config/profiles/examples/device-types.toml` lets the UI pick up
/// the matching schema (icon, attribute hints, etc.) instead of falling
/// back to the generic "sensor" rendering.
///
/// Unknown / opaque resources stay as "sensor" so they're at least
/// listed; the alternative (skipping) hides Hue resources we don't yet
/// recognise without telling anyone.
fn aux_device_type(resource_type: &str) -> &str {
    match resource_type {
        "motion" | "grouped_motion" => "motion_sensor",
        "temperature" => "temperature_sensor",
        "light_level" | "grouped_light_level" => "illuminance_sensor",
        "contact" => "contact_sensor",
        "button" => "button",
        "relative_rotary" => "button",
        "tamper" => "binary_sensor",
        "entertainment_configuration" => "entertainment",
        _ => "sensor",
    }
}

/// Determines the availability to publish for an auxiliary device.
///
/// Motion, contact, temperature, and light_level resources expose an `enabled`
/// field that reflects whether the sensor is active and reachable over Zigbee.
/// When `enabled = false` the sensor is either disabled by the user or has lost
/// connectivity (dead battery, out of range). All other resource types do not
/// expose connectivity state and are published as available unconditionally.
fn aux_is_available(aux: &HueAuxDevice) -> bool {
    match aux.resource_type.as_str() {
        "motion"
        | "grouped_motion"
        | "temperature"
        | "light_level"
        | "grouped_light_level"
        | "contact" => aux
            .attributes
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        _ => true,
    }
}

pub async fn apply_eventstream_update(
    publisher: &DevicePublisher,
    registry: &HueRegistry,
    bridge_id: &str,
    payload: &Value,
    cfg: &SyncConfig,
) -> Result<EventApplyOutcome> {
    let mut applied_any = false;
    let mut ignored_reason: Option<String> = None;
    let mut refresh_reason: Option<String> = None;

    if let Some(events) = payload.as_array() {
        for event in events {
            match apply_event_item(publisher, registry, bridge_id, event, &cfg.display).await? {
                EventApplyOutcome::Applied => applied_any = true,
                EventApplyOutcome::Ignored { reason } => {
                    if ignored_reason.is_none() {
                        ignored_reason = Some(reason);
                    }
                }
                EventApplyOutcome::NeedsRefresh { reason } => {
                    if refresh_reason.is_none() {
                        refresh_reason = Some(reason);
                    }
                }
            }
        }
    } else if payload.is_object() {
        match apply_event_item(publisher, registry, bridge_id, payload, &cfg.display).await? {
            EventApplyOutcome::Applied => applied_any = true,
            EventApplyOutcome::Ignored { reason } => ignored_reason = Some(reason),
            EventApplyOutcome::NeedsRefresh { reason } => refresh_reason = Some(reason),
        }
    } else {
        return Ok(EventApplyOutcome::Ignored {
            reason: "unsupported_payload_shape".to_string(),
        });
    }

    if applied_any {
        Ok(EventApplyOutcome::Applied)
    } else if let Some(reason) = refresh_reason {
        Ok(EventApplyOutcome::NeedsRefresh { reason })
    } else {
        Ok(EventApplyOutcome::Ignored {
            reason: ignored_reason.unwrap_or_else(|| "empty_event_payload".to_string()),
        })
    }
}

async fn apply_event_item(
    publisher: &DevicePublisher,
    registry: &HueRegistry,
    bridge_id: &str,
    event: &Value,
    display_cfg: &HueDisplayConfig,
) -> Result<EventApplyOutcome> {
    // `applied` means "the event type was recognized and dispatched to a known device".
    // It does NOT require that a non-empty state patch was produced — a keep-alive ping
    // or a connectivity notification with no new data still counts as handled.
    // Returning false triggers an expensive fallback full-refresh; only do that for
    // truly unrecognized event types.
    let mut applied = false;
    let mut saw_known_type = false;
    let mut saw_missing_identity = false;
    let mut unknown_resource_type: Option<String> = None;
    let Some(data_items) = event.get("data").and_then(|v| v.as_array()) else {
        return Ok(EventApplyOutcome::Ignored {
            reason: "missing_data".to_string(),
        });
    };

    for item in data_items {
        let Some(resource_type) = item.get("type").and_then(|v| v.as_str()) else {
            saw_missing_identity = true;
            continue;
        };
        let Some(rid) = item.get("id").and_then(|v| v.as_str()) else {
            saw_missing_identity = true;
            continue;
        };

        match resource_type {
            "light" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_light_device_id(bridge_id, rid) {
                    applied = true;
                    let mut patch = serde_json::Map::new();
                    if let Some(on) = item
                        .get("on")
                        .and_then(|o| o.get("on"))
                        .and_then(|v| v.as_bool())
                    {
                        patch.insert("on".to_string(), json!(on));
                    }
                    if let Some(brightness) = item
                        .get("dimming")
                        .and_then(|d| d.get("brightness"))
                        .and_then(|v| v.as_f64())
                    {
                        patch.insert("brightness_pct".to_string(), json!(brightness));
                    }
                    if let Some(mirek) = item
                        .get("color_temperature")
                        .and_then(|ct| ct.get("mirek"))
                        .and_then(|v| v.as_u64())
                    {
                        patch.insert("color_temp_mirek".to_string(), json!(mirek));
                    }
                    if let Some((x, y)) =
                        item.get("color").and_then(|c| c.get("xy")).and_then(|xy| {
                            let x = xy.get("x")?.as_f64()?;
                            let y = xy.get("y")?.as_f64()?;
                            Some((x, y))
                        })
                    {
                        patch.insert("color_xy".to_string(), json!({ "x": x, "y": y }));
                    }
                    if let Some(effect) = item
                        .get("effects")
                        .and_then(|e| e.get("effect"))
                        .and_then(|v| v.as_str())
                    {
                        patch.insert("effect".to_string(), json!(effect));
                    }
                    if let Some(speed) = item
                        .get("dynamics")
                        .and_then(|d| d.get("speed"))
                        .and_then(|v| v.as_f64())
                    {
                        patch.insert("dynamic_speed".to_string(), json!(speed));
                    }
                    if let Some(status) = item
                        .get("dynamics")
                        .and_then(|d| d.get("status"))
                        .and_then(|v| v.as_str())
                    {
                        patch.insert("dynamic_status".to_string(), json!(status));
                    }
                    if let Some(points) = item
                        .get("gradient")
                        .and_then(|g| g.get("points"))
                        .and_then(|v| v.as_array())
                    {
                        let gradient_points = points
                            .iter()
                            .filter_map(|point| {
                                let xy = point.get("color")?.get("xy")?;
                                let x = xy.get("x")?.as_f64()?;
                                let y = xy.get("y")?.as_f64()?;
                                Some(json!({ "x": x, "y": y }))
                            })
                            .collect::<Vec<_>>();
                        if !gradient_points.is_empty() {
                            patch.insert(
                                "gradient_points".to_string(),
                                Value::Array(gradient_points),
                            );
                        }
                    }

                    if !patch.is_empty() {
                        publisher
                            .publish_state_partial(&device_id, &Value::Object(patch))
                            .await?;
                        applied = true;
                    }
                }
            }
            "grouped_light" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_group_device_id(bridge_id, rid) {
                    applied = true;
                    let mut patch = serde_json::Map::new();
                    if let Some(on) = item
                        .get("on")
                        .and_then(|o| o.get("on"))
                        .and_then(|v| v.as_bool())
                    {
                        patch.insert("on".to_string(), json!(on));
                    }
                    if let Some(brightness) = item
                        .get("dimming")
                        .and_then(|d| d.get("brightness"))
                        .and_then(|v| v.as_f64())
                    {
                        patch.insert("brightness_pct".to_string(), json!(brightness));
                    }

                    if !patch.is_empty() {
                        publisher
                            .publish_state_partial(&device_id, &Value::Object(patch))
                            .await?;
                        applied = true;
                    }
                }
            }
            "scene" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_scene_device_id(bridge_id, rid) {
                    applied = true;
                    if let Some(active) = item
                        .get("status")
                        .and_then(|s| s.get("active"))
                        .and_then(|v| v.as_bool())
                    {
                        publisher
                            .publish_state_partial(&device_id, &json!({ "active": active }))
                            .await?;
                        applied = true;
                    }
                }
            }
            "motion" | "grouped_motion" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_aux_device_id(bridge_id, resource_type, rid)
                {
                    applied = true;
                    let mut patch = serde_json::Map::new();
                    insert_motion_patch(&mut patch, item);
                    if resource_type == "grouped_motion" {
                        insert_temperature_patch(&mut patch, item);
                        insert_light_level_patch(&mut patch, item);
                        apply_display_preferences_to_patch(
                            &mut patch,
                            "grouped_motion",
                            display_cfg,
                        );
                    }
                    if !patch.is_empty() {
                        publisher
                            .publish_state_partial(&device_id, &Value::Object(patch))
                            .await?;
                        applied = true;
                    }
                }
            }
            "temperature" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_aux_device_id(bridge_id, resource_type, rid)
                {
                    applied = true;
                    let mut patch = serde_json::Map::new();
                    if let Some(v) = extract_temperature_c(item) {
                        patch.insert("temperature_c".to_string(), json!(v));
                        patch.insert("temperature_f".to_string(), json!((v * 9.0 / 5.0) + 32.0));
                        patch.insert("temperature_unit".to_string(), json!("C"));
                    }
                    if let Some(v) = extract_temperature_valid(item) {
                        patch.insert("temperature_valid".to_string(), json!(v));
                    }
                    if let Some(v) = item.get("enabled").and_then(|v| v.as_bool()) {
                        patch.insert("enabled".to_string(), json!(v));
                    }
                    apply_display_preferences_to_patch(&mut patch, resource_type, display_cfg);
                    if !patch.is_empty() {
                        publisher
                            .publish_state_partial(&device_id, &Value::Object(patch))
                            .await?;
                        applied = true;
                    }
                }
            }
            "light_level" | "grouped_light_level" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_aux_device_id(bridge_id, resource_type, rid)
                {
                    applied = true;
                    let mut patch = serde_json::Map::new();
                    insert_light_level_patch(&mut patch, item);
                    apply_display_preferences_to_patch(&mut patch, resource_type, display_cfg);
                    if !patch.is_empty() {
                        publisher
                            .publish_state_partial(&device_id, &Value::Object(patch))
                            .await?;
                        applied = true;
                    }
                }
            }
            "contact" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_aux_device_id(bridge_id, resource_type, rid)
                {
                    applied = true;
                    let mut patch = serde_json::Map::new();
                    if let Some(v) = item
                        .get("contact_report")
                        .and_then(|c| c.get("state"))
                        .and_then(|v| v.as_str())
                    {
                        patch.insert("contact_state".to_string(), json!(v));
                    }
                    if let Some(v) = item.get("tampered").and_then(|v| v.as_bool()) {
                        patch.insert("tampered".to_string(), json!(v));
                    }
                    if let Some(v) = item.get("enabled").and_then(|v| v.as_bool()) {
                        patch.insert("enabled".to_string(), json!(v));
                    }
                    if !patch.is_empty() {
                        publisher
                            .publish_state_partial(&device_id, &Value::Object(patch))
                            .await?;
                        applied = true;
                    }
                }
            }
            "device_power" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_aux_device_id(bridge_id, resource_type, rid)
                {
                    applied = true;
                    let mut patch = serde_json::Map::new();
                    if let Some(v) =
                        item.get("battery_level")
                            .and_then(|v| v.as_f64())
                            .or_else(|| {
                                item.get("power_state")
                                    .and_then(|p| p.get("battery_level"))
                                    .and_then(|v| v.as_f64())
                            })
                    {
                        patch.insert("battery_pct".to_string(), json!(v));
                    }
                    if let Some(v) =
                        item.get("battery_state")
                            .and_then(|v| v.as_str())
                            .or_else(|| {
                                item.get("power_state")
                                    .and_then(|p| p.get("battery_state"))
                                    .and_then(|v| v.as_str())
                            })
                    {
                        patch.insert("battery_state".to_string(), json!(v));
                    }
                    if !patch.is_empty() {
                        publisher
                            .publish_state_partial(&device_id, &Value::Object(patch))
                            .await?;
                        applied = true;
                    }
                }
            }
            "zigbee_connectivity" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_aux_device_id(bridge_id, resource_type, rid)
                {
                    applied = true;
                    if let Some(status) = item.get("status").and_then(|v| v.as_str()) {
                        // Only publish on actual connectivity changes (not keep-alives).
                        // "connected" is the normal state — only publish disconnects or
                        // transitions back to connected after a disconnect.
                        if status != "connected" {
                            let patch = json!({"connectivity_status": status});
                            publisher.publish_state_partial(&device_id, &patch).await?;
                        }
                    }
                }
            }
            "button" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_aux_device_id(bridge_id, resource_type, rid)
                {
                    applied = true;
                    let mut patch = serde_json::Map::new();
                    let mut button_event_value: Option<String> = None;

                    if let Some(v) = item
                        .get("button_report")
                        .and_then(|r| r.get("event"))
                        .and_then(|v| v.as_str())
                    {
                        patch.insert("button_event".to_string(), json!(v));
                        button_event_value = Some(v.to_string());
                    }
                    if let Some(v) = item
                        .get("button_report")
                        .and_then(|r| r.get("updated"))
                        .and_then(|v| v.as_str())
                    {
                        patch.insert("button_updated".to_string(), json!(v));
                    }
                    if let Some(v) = item.get("repeat_interval").and_then(|v| v.as_u64()) {
                        patch.insert("button_repeat_interval_ms".to_string(), json!(v));
                    }

                    if !patch.is_empty() {
                        publisher
                            .publish_state_partial(&device_id, &Value::Object(patch))
                            .await?;
                        applied = true;
                    }

                    if let Some(event) = button_event_value {
                        let payload = translator::button_event(
                            publisher.plugin_id(),
                            &device_id,
                            bridge_id,
                            rid,
                            &event,
                        );
                        publisher.publish_event("device_button", &payload).await?;
                    }
                }
            }
            "relative_rotary" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_aux_device_id(bridge_id, resource_type, rid)
                {
                    applied = true;
                    let mut patch = serde_json::Map::new();
                    let action = item
                        .get("rotary_report")
                        .and_then(|r| r.get("action"))
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                    let direction = item
                        .get("rotary_report")
                        .and_then(|r| r.get("rotation"))
                        .and_then(|r| r.get("direction"))
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                    let steps = item
                        .get("rotary_report")
                        .and_then(|r| r.get("rotation"))
                        .and_then(|r| r.get("steps"))
                        .and_then(|v| v.as_i64());

                    if let Some(v) = action.as_deref() {
                        patch.insert("rotary_action".to_string(), json!(v));
                    }
                    if let Some(v) = direction.as_deref() {
                        patch.insert("rotary_direction".to_string(), json!(v));
                    }
                    if let Some(v) = steps {
                        patch.insert("rotary_steps".to_string(), json!(v));
                    }
                    if let Some(v) = item
                        .get("rotary_report")
                        .and_then(|r| r.get("updated"))
                        .and_then(|v| v.as_str())
                    {
                        patch.insert("rotary_updated".to_string(), json!(v));
                    }

                    if !patch.is_empty() {
                        publisher
                            .publish_state_partial(&device_id, &Value::Object(patch))
                            .await?;
                        applied = true;
                    }

                    if action.is_some() || direction.is_some() || steps.is_some() {
                        let payload = translator::rotary_event(
                            publisher.plugin_id(),
                            &device_id,
                            bridge_id,
                            rid,
                            action.as_deref(),
                            direction.as_deref(),
                            steps,
                        );
                        publisher.publish_event("device_rotary", &payload).await?;
                    }
                }
            }
            "entertainment_configuration" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_aux_device_id(bridge_id, resource_type, rid)
                {
                    applied = true;
                    let mut patch = serde_json::Map::new();
                    let mut active_value: Option<bool> = None;
                    let mut status_value: Option<String> = None;
                    let mut type_value: Option<String> = None;
                    if let Some(v) = item
                        .get("status")
                        .and_then(|s| s.get("active"))
                        .and_then(|v| v.as_bool())
                    {
                        active_value = Some(v);
                        patch.insert("entertainment_active".to_string(), json!(v));
                    }
                    if let Some(v) = item
                        .get("status")
                        .and_then(|s| s.get("status"))
                        .and_then(|v| v.as_str())
                    {
                        status_value = Some(v.to_string());
                        patch.insert("entertainment_status".to_string(), json!(v));
                    }
                    if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                        patch.insert("entertainment_name".to_string(), json!(name));
                    }
                    if let Some(owner) = item
                        .get("owner")
                        .and_then(|v| v.get("rid"))
                        .and_then(|v| v.as_str())
                    {
                        patch.insert("entertainment_owner".to_string(), json!(owner));
                    }
                    if let Some(channels) = item.get("channels").and_then(|v| v.as_array()) {
                        patch.insert(
                            "entertainment_channel_count".to_string(),
                            json!(channels.len() as u32),
                        );
                    }
                    if let Some(segments) = item.get("segments").and_then(|v| v.as_array()) {
                        patch.insert(
                            "entertainment_segment_count".to_string(),
                            json!(segments.len() as u32),
                        );
                    }
                    if let Some(proxy) = item.get("stream_proxy").and_then(|v| v.get("node")) {
                        patch.insert("entertainment_proxy_type".to_string(), json!(proxy));
                    }
                    if let Some(v) = item.get("configuration_type").and_then(|v| v.as_str()) {
                        type_value = Some(v.to_string());
                        patch.insert("entertainment_type".to_string(), json!(v));
                    }
                    if !patch.is_empty() {
                        publisher
                            .publish_state_partial(&device_id, &Value::Object(patch))
                            .await?;
                        if active_value.is_some() || status_value.is_some() || type_value.is_some()
                        {
                            let payload = translator::entertainment_status_changed_event(
                                publisher.plugin_id(),
                                &device_id,
                                rid,
                                active_value,
                                status_value.as_deref(),
                                type_value.as_deref(),
                                "eventstream_patch",
                            );
                            publisher
                                .publish_event("entertainment_status_changed", &payload)
                                .await?;
                        }
                        applied = true;
                    }
                }
            }
            "bridge_home" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_aux_device_id(bridge_id, resource_type, rid)
                {
                    applied = true;
                    let mut patch = serde_json::Map::new();
                    if let Some(v) = item
                        .get("children")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                    {
                        patch.insert("child_count".to_string(), json!(v));
                    }
                    if let Some(v) = item.get("id_v1").and_then(|v| v.as_str()) {
                        patch.insert("id_v1".to_string(), json!(v));
                    }
                    if !patch.is_empty() {
                        publisher
                            .publish_state_partial(&device_id, &Value::Object(patch))
                            .await?;
                        applied = true;
                    }
                }
            }
            _ => {
                if unknown_resource_type.is_none() {
                    unknown_resource_type = Some(format!("unknown_resource_type:{resource_type}"));
                }
            }
        }
    }

    if applied {
        Ok(EventApplyOutcome::Applied)
    } else if saw_known_type {
        Ok(EventApplyOutcome::Ignored {
            reason: "recognized_no_patch_or_device".to_string(),
        })
    } else if saw_missing_identity {
        Ok(EventApplyOutcome::Ignored {
            reason: "missing_event_identity".to_string(),
        })
    } else if let Some(reason) = unknown_resource_type {
        Ok(EventApplyOutcome::NeedsRefresh { reason })
    } else {
        Ok(EventApplyOutcome::Ignored {
            reason: "empty_event_data".to_string(),
        })
    }
}

fn light_level_to_lux(raw: f64) -> Option<f64> {
    if raw <= 0.0 {
        return None;
    }
    let lux = 10f64.powf((raw - 1.0) / 10000.0);
    if lux.is_finite() {
        Some(lux)
    } else {
        None
    }
}

fn extract_temperature_c(item: &Value) -> Option<f64> {
    let raw = item
        .get("temperature")
        .and_then(|v| v.as_f64())
        .or_else(|| {
            item.get("temperature")
                .and_then(|obj| obj.get("temperature"))
                .and_then(|v| v.as_f64())
        })?;

    let normalized = if raw.abs() > 120.0 { raw / 100.0 } else { raw };
    Some(normalized)
}

fn extract_temperature_valid(item: &Value) -> Option<bool> {
    item.get("temperature_valid")
        .and_then(|v| v.as_bool())
        .or_else(|| {
            item.get("temperature")
                .and_then(|obj| obj.get("temperature_valid"))
                .and_then(|v| v.as_bool())
        })
}

fn extract_light_level_raw(item: &Value) -> Option<f64> {
    item.get("light_level")
        .and_then(|v| v.as_f64())
        .or_else(|| {
            item.get("light")
                .and_then(|obj| obj.get("light_level"))
                .and_then(|v| v.as_f64())
        })
}

fn extract_light_level_valid(item: &Value) -> Option<bool> {
    item.get("light_level_valid")
        .and_then(|v| v.as_bool())
        .or_else(|| {
            item.get("light")
                .and_then(|obj| obj.get("light_level_valid"))
                .and_then(|v| v.as_bool())
        })
}

fn insert_motion_patch(attrs: &mut serde_json::Map<String, Value>, item: &Value) {
    if let Some(v) = item
        .get("motion")
        .and_then(|m| m.get("motion"))
        .and_then(|v| v.as_bool())
    {
        attrs.insert("motion".to_string(), json!(v));
    }
    if let Some(v) = item.get("motion_valid").and_then(|v| v.as_bool()) {
        attrs.insert("motion_valid".to_string(), json!(v));
    }
    if let Some(v) = item.get("enabled").and_then(|v| v.as_bool()) {
        attrs.insert("enabled".to_string(), json!(v));
    }
    if let Some(v) = item
        .get("sensitivity")
        .and_then(|s| s.get("sensitivity"))
        .and_then(|v| v.as_u64())
    {
        attrs.insert("motion_sensitivity".to_string(), json!(v));
    }
}

fn insert_temperature_patch(attrs: &mut serde_json::Map<String, Value>, item: &Value) {
    if let Some(v) = extract_temperature_c(item) {
        attrs.insert("temperature_c".to_string(), json!(v));
        attrs.insert("temperature_f".to_string(), json!((v * 9.0 / 5.0) + 32.0));
    }
    if let Some(v) = extract_temperature_valid(item) {
        attrs.insert("temperature_valid".to_string(), json!(v));
    }
    if let Some(v) = item.get("enabled").and_then(|v| v.as_bool()) {
        attrs.insert("enabled".to_string(), json!(v));
    }
    if attrs.contains_key("temperature_c") || attrs.contains_key("temperature_f") {
        attrs.insert("temperature_unit".to_string(), json!("C"));
    }
}

fn insert_light_level_patch(attrs: &mut serde_json::Map<String, Value>, item: &Value) {
    if let Some(v) = extract_light_level_raw(item) {
        attrs.insert("illuminance_raw".to_string(), json!(v));
        if let Some(lux) = light_level_to_lux(v) {
            attrs.insert("illuminance_lux".to_string(), json!(lux));
        }
        attrs.insert("illuminance_unit".to_string(), json!("lux"));
    }
    if let Some(v) = extract_light_level_valid(item) {
        attrs.insert("illuminance_valid".to_string(), json!(v));
    }
    if let Some(v) = item.get("enabled").and_then(|v| v.as_bool()) {
        attrs.insert("enabled".to_string(), json!(v));
    }
}

#[derive(Debug, Clone)]
struct AuxRegistrationSpec {
    name: String,
    device_type: String,
    capabilities: serde_json::Map<String, Value>,
    name_priority: u8,
}

fn build_primary_aux_owner_map(aux_devices: &[HueAuxDevice]) -> HashMap<String, String> {
    let mut selected: HashMap<String, (u8, String)> = HashMap::new();

    for aux in aux_devices {
        let Some(priority) = primary_aux_priority(&aux.resource_type) else {
            continue;
        };

        selected
            .entry(aux.owner_rid.clone())
            .and_modify(|current| {
                if priority < current.0 {
                    *current = (priority, aux.device_id.clone());
                }
            })
            .or_insert_with(|| (priority, aux.device_id.clone()));
    }

    selected
        .into_iter()
        .map(|(owner_rid, (_, device_id))| (owner_rid, device_id))
        .collect()
}

fn build_light_owner_map(lights: &[crate::hue::models::HueLight]) -> HashMap<String, String> {
    lights
        .iter()
        .map(|light| (light.owner_rid.clone(), light.device_id.clone()))
        .collect()
}

fn primary_aux_priority(resource_type: &str) -> Option<u8> {
    match resource_type {
        "motion" => Some(0),
        "contact" => Some(1),
        "button" => Some(2),
        "relative_rotary" => Some(3),
        "temperature" => Some(4),
        "light_level" => Some(5),
        _ => None,
    }
}

fn should_compact_to_primary_aux(resource_type: &str) -> bool {
    matches!(
        resource_type,
        "motion"
            | "contact"
            | "button"
            | "relative_rotary"
            | "temperature"
            | "light_level"
            | "grouped_motion"
            | "grouped_light_level"
            | "device_power"
            | "zigbee_connectivity"
    )
}

fn compact_publish_device_id(
    aux: &HueAuxDevice,
    primary_aux_owner_to_device: &HashMap<String, String>,
    light_owner_to_device: &HashMap<String, String>,
) -> String {
    if should_compact_to_primary_aux(&aux.resource_type) {
        if let Some(primary_aux_device_id) = primary_aux_owner_to_device.get(&aux.owner_rid) {
            return primary_aux_device_id.clone();
        }
    }

    if matches!(
        aux.resource_type.as_str(),
        "device_power" | "zigbee_connectivity"
    ) {
        if let Some(light_device_id) = light_owner_to_device.get(&aux.owner_rid) {
            return light_device_id.clone();
        }
    }
    aux.device_id.clone()
}

fn merge_capabilities(target: &mut serde_json::Map<String, Value>, capabilities: Value) {
    if let Value::Object(obj) = capabilities {
        target.extend(obj);
    }
}

fn build_aux_registration_specs(
    aux_devices: &[HueAuxDevice],
    primary_aux_owner_to_device: &HashMap<String, String>,
    light_owner_to_device: &HashMap<String, String>,
    compact_motion_facets: bool,
    publish_bridge_home: bool,
    publish_entertainment_configurations: bool,
) -> HashMap<String, AuxRegistrationSpec> {
    let mut specs: HashMap<String, AuxRegistrationSpec> = HashMap::new();

    for aux in aux_devices {
        let publish_device_id = if compact_motion_facets {
            compact_publish_device_id(aux, primary_aux_owner_to_device, light_owner_to_device)
        } else {
            aux.device_id.clone()
        };

        if should_skip_aux_device(
            aux,
            &publish_device_id,
            publish_bridge_home,
            publish_entertainment_configurations,
        ) {
            continue;
        }

        let entry = specs
            .entry(publish_device_id)
            .or_insert_with(|| AuxRegistrationSpec {
                name: aux.name.clone(),
                device_type: aux_device_type(&aux.resource_type).to_string(),
                capabilities: serde_json::Map::new(),
                name_priority: aux_name_priority(&aux.resource_type),
            });
        merge_capabilities(&mut entry.capabilities, translator::aux_capabilities(aux));
        let candidate_name_priority = aux_name_priority(&aux.resource_type);
        if candidate_name_priority < entry.name_priority {
            entry.name = aux.name.clone();
            entry.name_priority = candidate_name_priority;
        }
        if entry.device_type == "sensor" {
            entry.device_type = aux_device_type(&aux.resource_type).to_string();
        }
    }

    specs
}

fn aux_name_priority(resource_type: &str) -> u8 {
    match resource_type {
        "motion" | "contact" | "button" | "relative_rotary" | "temperature" | "light_level" => 0,
        "grouped_motion" | "grouped_light_level" => 1,
        "device_power" | "zigbee_connectivity" => 2,
        "entertainment" => 3,
        _ => 4,
    }
}

fn normalize_group_selector(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn grouped_light_matches_selector(group: &HueGroupedLight, selector: &str) -> bool {
    let selector = selector.trim().to_ascii_lowercase();
    if selector.is_empty() {
        return false;
    }

    let name_slug = normalize_group_selector(&group.name);
    let area_slug = group
        .area
        .as_ref()
        .map(|area| normalize_group_selector(area));
    let resource_id = group.resource_id.to_ascii_lowercase();

    if selector == resource_id || selector == name_slug {
        return true;
    }

    if let Some(area_slug) = area_slug.as_ref() {
        if selector == *area_slug {
            return true;
        }
    }

    if let Some(kind) = group.group_kind.as_deref() {
        let kind_selector = format!("{kind}:{name_slug}");
        if selector == kind_selector {
            return true;
        }
        if let Some(area_slug) = area_slug.as_ref() {
            let kind_area_selector = format!("{kind}:{area_slug}");
            if selector == kind_area_selector {
                return true;
            }
        }
    }

    false
}

fn should_publish_grouped_light(
    group: &HueGroupedLight,
    publish_all_grouped_lights: bool,
    publish_grouped_lights_for: &[String],
    skip_grouped_lights_for: &[String],
) -> bool {
    let explicitly_included = publish_grouped_lights_for
        .iter()
        .any(|selector| grouped_light_matches_selector(group, selector));
    let explicitly_excluded = skip_grouped_lights_for
        .iter()
        .any(|selector| grouped_light_matches_selector(group, selector));

    (publish_all_grouped_lights || explicitly_included) && !explicitly_excluded
}

fn should_skip_aux_device(
    aux: &HueAuxDevice,
    publish_device_id: &str,
    publish_bridge_home: bool,
    publish_entertainment_configurations: bool,
) -> bool {
    if aux.resource_type == "bridge_home" && !publish_bridge_home {
        return true;
    }

    if aux.resource_type == "entertainment_configuration" && !publish_entertainment_configurations {
        return true;
    }

    if matches!(
        aux.resource_type.as_str(),
        "device_power" | "zigbee_connectivity"
    ) && publish_device_id == aux.device_id
    {
        return true;
    }

    matches!(
        aux.resource_type.as_str(),
        "grouped_motion" | "grouped_light_level"
    ) && publish_device_id == aux.device_id
}

fn strip_aux_metadata(state: &mut Value) {
    if let Some(obj) = state.as_object_mut() {
        obj.remove("kind");
        obj.remove("bridge_id");
        obj.remove("resource_type");
        obj.remove("resource_id");
    }
}

fn apply_display_preferences(
    state: &mut Value,
    resource_type: &str,
    display_cfg: &HueDisplayConfig,
) {
    let Some(obj) = state.as_object_mut() else {
        return;
    };
    apply_display_preferences_to_patch(obj, resource_type, display_cfg);
}

fn apply_display_preferences_to_patch(
    attrs: &mut serde_json::Map<String, Value>,
    resource_type: &str,
    display_cfg: &HueDisplayConfig,
) {
    match resource_type {
        "grouped_motion" => {
            apply_display_preferences_to_patch(attrs, "temperature", display_cfg);
            apply_display_preferences_to_patch(attrs, "light_level", display_cfg);
        }
        "grouped_light_level" => {
            apply_display_preferences_to_patch(attrs, "light_level", display_cfg);
        }
        "temperature" => {
            let temperature_c = attrs.get("temperature_c").and_then(Value::as_f64);
            if let Some(c) = temperature_c {
                let f = attrs
                    .get("temperature_f")
                    .and_then(Value::as_f64)
                    .unwrap_or((c * 9.0 / 5.0) + 32.0);
                attrs.insert("temperature_f".to_string(), json!(f));
                match display_cfg.temperature_unit {
                    TemperatureUnit::C => {
                        attrs.insert("temperature".to_string(), json!(c));
                        attrs.insert("temperature_unit".to_string(), json!("C"));
                    }
                    TemperatureUnit::F => {
                        attrs.insert("temperature".to_string(), json!(f));
                        attrs.insert("temperature_unit".to_string(), json!("F"));
                    }
                }
            }
        }
        "light_level" => {
            let raw = attrs.get("illuminance_raw").and_then(Value::as_f64);
            let lux = attrs
                .get("illuminance_lux")
                .and_then(Value::as_f64)
                .or_else(|| raw.and_then(light_level_to_lux));

            if let Some(v) = lux {
                attrs.insert("illuminance_lux".to_string(), json!(v));
            }

            match display_cfg.illuminance_display {
                IlluminanceDisplay::Lux => {
                    if let Some(v) = lux {
                        attrs.insert("illuminance".to_string(), json!(v));
                        attrs.insert("illuminance_unit".to_string(), json!("lux"));
                    }
                }
                IlluminanceDisplay::Raw => {
                    if let Some(v) = raw {
                        attrs.insert("illuminance".to_string(), json!(v));
                        attrs.insert("illuminance_unit".to_string(), json!("raw"));
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HueDisplayConfig, IlluminanceDisplay, TemperatureUnit};
    use crate::hue::models::HueAuxDevice;
    use crate::hue::registry::HueRegistry;
    use plugin_sdk_rs::DevicePublisher;
    use serde_json::json;

    fn dummy_publisher() -> DevicePublisher {
        DevicePublisher::test_instance("plugin.hue")
    }

    fn default_sync_cfg() -> SyncConfig {
        SyncConfig {
            display: HueDisplayConfig::default(),
            compact_motion_facets: false,
            publish_grouped_lights: false,
            publish_grouped_lights_for: Vec::new(),
            skip_grouped_lights_for: Vec::new(),
            publish_bridge_home: false,
            publish_entertainment_configurations: false,
        }
    }

    #[test]
    fn maps_aux_resource_types_to_canonical_homecore_types() {
        // entertainment is its own thing.
        assert_eq!(
            aux_device_type("entertainment_configuration"),
            "entertainment"
        );
        // Sensors map to the homeCore canonical types (see
        // core/config/profiles/examples/device-types.toml) so the UI can
        // pick up the right schema.
        assert_eq!(aux_device_type("motion"), "motion_sensor");
        assert_eq!(aux_device_type("temperature"), "temperature_sensor");
        assert_eq!(aux_device_type("light_level"), "illuminance_sensor");
        assert_eq!(aux_device_type("contact"), "contact_sensor");
        assert_eq!(aux_device_type("button"), "button");
        // Anything we don't recognise keeps falling back to "sensor"
        // rather than getting hidden.
        assert_eq!(aux_device_type("unknown_future_type"), "sensor");
    }

    #[test]
    fn parses_nested_temperature_and_light_level_fields() {
        let temp_item = json!({
            "temperature": { "temperature": 2234.0, "temperature_valid": true }
        });
        assert_eq!(extract_temperature_c(&temp_item), Some(22.34));
        assert_eq!(extract_temperature_valid(&temp_item), Some(true));

        let light_item = json!({
            "light": { "light_level": 18000.0, "light_level_valid": false }
        });
        assert_eq!(extract_light_level_raw(&light_item), Some(18000.0));
        assert_eq!(extract_light_level_valid(&light_item), Some(false));
    }

    #[test]
    fn applies_display_preferences_for_temperature_and_illuminance() {
        let cfg_f_raw = HueDisplayConfig {
            temperature_unit: TemperatureUnit::F,
            illuminance_display: IlluminanceDisplay::Raw,
        };
        let mut attrs = serde_json::Map::new();
        attrs.insert("temperature_c".to_string(), json!(21.0));
        attrs.insert("illuminance_raw".to_string(), json!(15000.0));
        apply_display_preferences_to_patch(&mut attrs, "temperature", &cfg_f_raw);
        apply_display_preferences_to_patch(&mut attrs, "light_level", &cfg_f_raw);

        assert_eq!(attrs.get("temperature").and_then(Value::as_f64), Some(69.8));
        assert_eq!(
            attrs.get("temperature_unit").and_then(Value::as_str),
            Some("F")
        );
        assert_eq!(
            attrs.get("illuminance").and_then(Value::as_f64),
            Some(15000.0)
        );
        assert_eq!(
            attrs.get("illuminance_unit").and_then(Value::as_str),
            Some("raw")
        );

        let cfg_c_lux = HueDisplayConfig {
            temperature_unit: TemperatureUnit::C,
            illuminance_display: IlluminanceDisplay::Lux,
        };
        apply_display_preferences_to_patch(&mut attrs, "temperature", &cfg_c_lux);
        apply_display_preferences_to_patch(&mut attrs, "light_level", &cfg_c_lux);

        assert_eq!(attrs.get("temperature").and_then(Value::as_f64), Some(21.0));
        assert_eq!(
            attrs.get("temperature_unit").and_then(Value::as_str),
            Some("C")
        );
        assert_eq!(
            attrs.get("illuminance_unit").and_then(Value::as_str),
            Some("lux")
        );
        assert!(attrs.get("illuminance").and_then(Value::as_f64).is_some());
    }

    #[test]
    fn compacts_temp_lux_battery_to_motion_device() {
        let motion = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            owner_rid: "owner-1".to_string(),
            resource_type: "motion".to_string(),
            resource_id: "rid-motion".to_string(),
            device_id: "motion-dev".to_string(),
            name: "Sensor".to_string(),
            attributes: json!({}),
        };
        let temp = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            owner_rid: "owner-1".to_string(),
            resource_type: "temperature".to_string(),
            resource_id: "rid-temp".to_string(),
            device_id: "temp-dev".to_string(),
            name: "Sensor".to_string(),
            attributes: json!({}),
        };

        let map = build_primary_aux_owner_map(&[motion.clone(), temp.clone()]);
        let light_map: HashMap<String, String> = HashMap::new();
        assert_eq!(
            compact_publish_device_id(&temp, &map, &light_map),
            "motion-dev"
        );
        assert_eq!(
            compact_publish_device_id(&motion, &map, &light_map),
            "motion-dev"
        );
    }

    #[test]
    fn compacts_zigbee_connectivity_to_owner_light_when_no_motion_device() {
        let zigbee = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            owner_rid: "owner-2".to_string(),
            resource_type: "zigbee_connectivity".to_string(),
            resource_id: "rid-zigbee".to_string(),
            device_id: "zigbee-dev".to_string(),
            name: "Light Owner".to_string(),
            attributes: json!({}),
        };

        let motion_map: HashMap<String, String> = HashMap::new();
        let mut light_map: HashMap<String, String> = HashMap::new();
        light_map.insert("owner-2".to_string(), "light-dev".to_string());

        assert_eq!(
            compact_publish_device_id(&zigbee, &motion_map, &light_map),
            "light-dev"
        );
    }

    #[test]
    fn compacts_grouped_sensor_resources_to_motion_device() {
        let grouped_motion = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            owner_rid: "owner-1".to_string(),
            resource_type: "grouped_motion".to_string(),
            resource_id: "rid-grouped-motion".to_string(),
            device_id: "grouped-motion-dev".to_string(),
            name: "Sensor".to_string(),
            attributes: json!({}),
        };
        let grouped_light = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            owner_rid: "owner-1".to_string(),
            resource_type: "grouped_light_level".to_string(),
            resource_id: "rid-grouped-light".to_string(),
            device_id: "grouped-light-dev".to_string(),
            name: "Sensor".to_string(),
            attributes: json!({}),
        };
        let motion = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            owner_rid: "owner-1".to_string(),
            resource_type: "motion".to_string(),
            resource_id: "rid-motion".to_string(),
            device_id: "motion-dev".to_string(),
            name: "Sensor".to_string(),
            attributes: json!({}),
        };

        let motion_map = build_primary_aux_owner_map(&[motion]);
        let light_map: HashMap<String, String> = HashMap::new();

        assert_eq!(
            compact_publish_device_id(&grouped_motion, &motion_map, &light_map),
            "motion-dev"
        );
        assert_eq!(
            compact_publish_device_id(&grouped_light, &motion_map, &light_map),
            "motion-dev"
        );
    }

    #[test]
    fn skips_standalone_grouped_sensor_devices_and_bridge_home_by_default() {
        let grouped_motion = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            owner_rid: "owner-1".to_string(),
            resource_type: "grouped_motion".to_string(),
            resource_id: "rid-grouped-motion".to_string(),
            device_id: "grouped-motion-dev".to_string(),
            name: "Grouped Sensor".to_string(),
            attributes: json!({}),
        };
        let bridge_home = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            owner_rid: "bridge-owner".to_string(),
            resource_type: "bridge_home".to_string(),
            resource_id: "rid-bridge-home".to_string(),
            device_id: "bridge-home-dev".to_string(),
            name: "Bridge Home".to_string(),
            attributes: json!({}),
        };

        assert!(should_skip_aux_device(
            &grouped_motion,
            &grouped_motion.device_id,
            false,
            false
        ));
        assert!(should_skip_aux_device(
            &bridge_home,
            &bridge_home.device_id,
            false,
            false
        ));
        assert!(!should_skip_aux_device(
            &bridge_home,
            &bridge_home.device_id,
            true,
            true
        ));
    }

    #[test]
    fn compacts_remote_facets_to_button_owner_device() {
        let button = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            owner_rid: "owner-remote".to_string(),
            resource_type: "button".to_string(),
            resource_id: "rid-button".to_string(),
            device_id: "button-dev".to_string(),
            name: "Dial Switch".to_string(),
            attributes: json!({}),
        };
        let rotary = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            owner_rid: "owner-remote".to_string(),
            resource_type: "relative_rotary".to_string(),
            resource_id: "rid-rotary".to_string(),
            device_id: "rotary-dev".to_string(),
            name: "Dial Switch".to_string(),
            attributes: json!({}),
        };
        let battery = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            owner_rid: "owner-remote".to_string(),
            resource_type: "device_power".to_string(),
            resource_id: "rid-battery".to_string(),
            device_id: "battery-dev".to_string(),
            name: "Dial Switch".to_string(),
            attributes: json!({}),
        };

        let primary_map = build_primary_aux_owner_map(&[rotary.clone(), button.clone()]);
        let light_map: HashMap<String, String> = HashMap::new();

        assert_eq!(
            compact_publish_device_id(&button, &primary_map, &light_map),
            "button-dev"
        );
        assert_eq!(
            compact_publish_device_id(&rotary, &primary_map, &light_map),
            "button-dev"
        );
        assert_eq!(
            compact_publish_device_id(&battery, &primary_map, &light_map),
            "button-dev"
        );
    }

    #[test]
    fn skips_standalone_support_facets_without_owner_device() {
        let battery = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            owner_rid: "owner-support".to_string(),
            resource_type: "device_power".to_string(),
            resource_id: "rid-battery".to_string(),
            device_id: "battery-dev".to_string(),
            name: "Battery".to_string(),
            attributes: json!({}),
        };

        assert!(should_skip_aux_device(
            &battery,
            &battery.device_id,
            false,
            false
        ));
    }

    #[test]
    fn publishes_grouped_lights_by_selector() {
        let kitchen = HueGroupedLight {
            bridge_id: "bridge-1".to_string(),
            resource_id: "group-kitchen".to_string(),
            device_id: "group-dev".to_string(),
            name: "Kitchen".to_string(),
            area: Some("kitchen".to_string()),
            group_kind: Some("room".to_string()),
            on: Some(true),
            brightness_pct: Some(50.0),
        };

        assert!(should_publish_grouped_light(
            &kitchen,
            false,
            &[String::from("Kitchen")],
            &[]
        ));
        assert!(should_publish_grouped_light(
            &kitchen,
            false,
            &[String::from("room:kitchen")],
            &[]
        ));
        assert!(!should_publish_grouped_light(
            &kitchen,
            true,
            &[],
            &[String::from("room:kitchen")]
        ));
    }

    #[tokio::test]
    async fn ignores_known_event_type_without_registered_device() {
        let publisher = dummy_publisher();
        let registry = HueRegistry::default();
        let payload = json!({
            "data": [
                {
                    "type": "light",
                    "id": "unknown-light-rid",
                    "on": { "on": true }
                }
            ]
        });

        let outcome = apply_eventstream_update(
            &publisher,
            &registry,
            "bridge-1",
            &payload,
            &default_sync_cfg(),
        )
        .await
        .expect("eventstream update");

        assert_eq!(
            outcome,
            EventApplyOutcome::Ignored {
                reason: "recognized_no_patch_or_device".to_string()
            }
        );
    }

    #[tokio::test]
    async fn grouped_sensor_events_do_not_request_refresh() {
        let publisher = dummy_publisher();
        let registry = HueRegistry::default();
        let payload = json!({
            "data": [
                {
                    "type": "grouped_motion",
                    "id": "unknown-grouped-motion-rid",
                    "motion": { "motion": true },
                    "temperature": { "temperature": 2210.0, "temperature_valid": true },
                    "light": { "light_level": 17000.0, "light_level_valid": true }
                },
                {
                    "type": "grouped_light_level",
                    "id": "unknown-grouped-light-rid",
                    "light": { "light_level": 17000.0, "light_level_valid": true }
                }
            ]
        });

        let outcome = apply_eventstream_update(
            &publisher,
            &registry,
            "bridge-1",
            &payload,
            &default_sync_cfg(),
        )
        .await
        .expect("eventstream update");

        assert_eq!(
            outcome,
            EventApplyOutcome::Ignored {
                reason: "recognized_no_patch_or_device".to_string()
            }
        );
    }

    #[tokio::test]
    async fn requests_refresh_for_unknown_event_type() {
        let publisher = dummy_publisher();
        let registry = HueRegistry::default();
        let payload = json!({
            "data": [
                {
                    "type": "mystery_resource",
                    "id": "mystery-1"
                }
            ]
        });

        let outcome = apply_eventstream_update(
            &publisher,
            &registry,
            "bridge-1",
            &payload,
            &default_sync_cfg(),
        )
        .await
        .expect("eventstream update");

        assert_eq!(
            outcome,
            EventApplyOutcome::NeedsRefresh {
                reason: "unknown_resource_type:mystery_resource".to_string()
            }
        );
    }
}
