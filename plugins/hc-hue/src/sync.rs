use anyhow::Result;
use serde_json::json;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use tracing::{debug, warn};

use crate::hue::api::{
    insert_light_level as insert_light_level_patch, insert_temperature as insert_temperature_patch,
    open_from_contact_report, HueApiClient,
};
use crate::hue::models::{BridgeSnapshot, HueAuxDevice, HueGroupedLight};
use crate::hue::registry::HueRegistry;
use crate::translator;
use plugin_sdk_rs::DevicePublisher;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventApplyOutcome {
    Applied,
    Ignored { reason: String },
    NeedsRefresh { reason: String },
}

/// Aggregates feature-flag config for sync operations, avoiding
/// the need to pass 7+ separate arguments through every call site.
pub struct SyncConfig {
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

    // Not paired yet: there is nothing to read but the bridge's public
    // config, which says it is there and waiting for its button.
    if !api.has_app_key() {
        let summary = api.fetch_bridge_summary().await?;
        let state = translator::bridge_state(target, true, summary.clone());
        publisher.publish_state(&device_id, &state).await?;
        publisher.publish_availability(&device_id, true).await?;
        registry.upsert_bridge(
            device_id,
            BridgeSnapshot {
                online: true,
                summary,
            },
        );
        return Ok(());
    }

    let resources = match api.fetch_all().await {
        Ok(r) => r,
        Err(e) => {
            publisher.publish_availability(&device_id, false).await?;
            publisher
                .publish_state_partial(
                    &device_id,
                    &json!({ "online": false, "integration_state": "unreachable" }),
                )
                .await?;
            return Err(e);
        }
    };

    {
        let summary = api.summary_from(&resources);
        let mut state = translator::bridge_state(target, true, summary.clone());
        let bridge_device = resources
            .of("bridge")
            .first()
            .and_then(|b| resources.owner_device(b));
        if let Some(obj) = state.as_object_mut() {
            obj.insert("light_count".into(), json!(resources.count("light")));
            obj.insert("scene_count".into(), json!(resources.count("scene")));
            obj.insert("room_count".into(), json!(resources.count("room")));
            obj.insert("zone_count".into(), json!(resources.count("zone")));
            obj.insert(
                "sensor_count".into(),
                json!(resources
                    .of("device")
                    .iter()
                    .filter(|d| {
                        resources.service_of(d, "light").is_none()
                            && resources.service_of(d, "bridge").is_none()
                    })
                    .count()),
            );
            if let Some(d) = bridge_device {
                let p = crate::hue::resources::product_of(d);
                if let Some(v) = p.model_id {
                    obj.insert("model_id".into(), json!(v));
                }
                if let Some(v) = p.software_version {
                    obj.insert("sw_version".into(), json!(v));
                }
                if let Some(v) = resources.update_state(d) {
                    obj.insert("update_state".into(), json!(v));
                }
            }
        }
        publisher.publish_state(&device_id, &state).await?;
        publisher.publish_availability(&device_id, true).await?;
        registry.upsert_bridge(
            device_id.clone(),
            BridgeSnapshot {
                online: true,
                summary,
            },
        );
        registry.set_bridge_update_state(
            &device_id,
            bridge_device.and_then(|d| resources.update_state(d)),
        );
        registry.set_rooms(&target.bridge_id, api.rooms(&resources));
        registry.set_button_labels(crate::hue::api::button_labels(&resources));
    }
    debug!(device_id, "Hue bridge state refreshed");

    {
        let lights = api.lights(&resources);
        let light_owner_to_device = build_light_owner_map(&lights);
        let mut published_light_ids: HashSet<String> = HashSet::new();
        for light in &lights {
            published_light_ids.insert(light.device_id.clone());
            let renamed = registry.renamed(&light.device_id, &light.name, light.area.as_deref());
            if registry.ensure_light(light) || renamed {
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
                        Some(translator::light_capabilities(light)),
                    )
                    .await?;
                publisher.subscribe_commands(&light.device_id).await?;
                // What this light can do — its own range, colour only if
                // it has colour — plus everything it publishes that the
                // declaration does not mention.
                let light_schema = crate::aux_schema::with_published(
                    translator::light_schema(light),
                    &translator::light_state(light),
                );
                let with = plugin_sdk_rs::device_actions::with_actions(
                    &light_schema,
                    translator::light_actions(light),
                );
                publisher
                    .register_device_schema_json(&light.device_id, &with)
                    .await
                    .ok();
                debug!(device_id = %light.device_id, name = %light.name, "Registered Hue light device");
            }

            publisher
                .publish_state(&light.device_id, &translator::light_state(light))
                .await?;
            if registry.first_retirement(&light.device_id) {
                publisher
                    .publish_state_partial(
                        &light.device_id,
                        &translator::retired_state(translator::RetiredKind::Light),
                    )
                    .await?;
            }
            // A light the bridge cannot reach is not there, whatever its
            // last state says: switched off at the wall, or out of range.
            // After the state, which homeCore takes as a sign of life.
            publisher
                .publish_availability(&light.device_id, light.reachable.unwrap_or(true))
                .await?;
            registry
                .set_alerts_if_changed(publisher, &light.device_id, crate::alerts::for_light(light))
                .await;
        }
        for stale_light_id in registry.prune_lights_not_in(&target.bridge_id, &published_light_ids)
        {
            publisher
                .unregister_device(publisher.plugin_id(), &stale_light_id)
                .await?;
            debug!(device_id = %stale_light_id, "Unregistered stale Hue light");
        }

        if cfg.publish_grouped_lights || !cfg.publish_grouped_lights_for.is_empty() {
            let groups = api.grouped_lights(&resources);
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
                    publisher
                        .register_device_schema_json(
                            &group.device_id,
                            &translator::group_schema(&group),
                        )
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
            for stale_group_id in
                registry.prune_groups_not_in(&target.bridge_id, &published_group_ids)
            {
                publisher
                    .unregister_device(publisher.plugin_id(), &stale_group_id)
                    .await?;
            }
        } else {
            for stale_group_id in registry.prune_groups_not_in(&target.bridge_id, &HashSet::new()) {
                publisher
                    .unregister_device(publisher.plugin_id(), &stale_group_id)
                    .await?;
            }
        }

        let scenes = api.scenes(&resources);
        let mut published_scene_ids: HashSet<String> = HashSet::new();
        for scene in scenes {
            published_scene_ids.insert(scene.device_id.clone());
            let renamed = registry.renamed(&scene.device_id, &scene.name, scene.area.as_deref());
            if registry.ensure_scene(&scene) || renamed {
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
                publisher
                    .register_device_schema_json(
                        &scene.device_id,
                        &translator::scene_schema_for(&scene),
                    )
                    .await
                    .ok();
                debug!(device_id = %scene.device_id, name = %scene.name, "Registered Hue scene device");
            } else if registry.scene_status_support_changed(&scene) {
                publisher
                    .register_device_schema_json(
                        &scene.device_id,
                        &translator::scene_schema_for(&scene),
                    )
                    .await
                    .ok();
            }

            publisher
                .publish_availability(&scene.device_id, true)
                .await?;
            publisher
                .publish_state(&scene.device_id, &translator::scene_state(&scene))
                .await?;
        }
        for stale_scene_id in registry.prune_scenes_not_in(&target.bridge_id, &published_scene_ids)
        {
            publisher
                .unregister_device(publisher.plugin_id(), &stale_scene_id)
                .await?;
            debug!(device_id = %stale_scene_id, "Unregistered stale Hue scene");
        }

        let aux_devices = api.aux_devices(&resources);
        let primary_aux_owner_to_device = build_primary_aux_owner_map(&aux_devices);
        let aux_registration_specs = build_aux_registration_specs(
            &aux_devices,
            &primary_aux_owner_to_device,
            &light_owner_to_device,
            cfg.compact_motion_facets,
            cfg.publish_bridge_home,
            cfg.publish_entertainment_configurations,
        );
        let mut aux_published_attrs: HashMap<String, serde_json::Map<String, Value>> =
            HashMap::new();
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
                        aux.area.as_deref(),
                        registration
                            .map(|spec| Value::Object(spec.capabilities.clone()))
                            .or_else(|| Some(translator::aux_capabilities(&aux))),
                    )
                    .await?;
                debug!(device_id = %publish_device_id, name = %aux.name, kind = %aux.resource_type, "Registered Hue auxiliary device");
            }

            // A lamp's connectivity is its own availability, said with the
            // lamp above; publishing "available" for the facet here undid it.
            // Its state is diagnostic noise that would change the lamp on
            // every resync, so nothing of it is published here.
            if aux.resource_type == "zigbee_connectivity" && publish_device_id != aux.device_id {
                continue;
            }

            publisher
                .publish_availability(&publish_device_id, aux_is_available(&aux))
                .await?;

            let mut state = translator::aux_state(&aux);

            // What this device ends up reporting, however many Hue
            // resources compacted onto it — the schema is derived from it
            // once the loop has seen them all.
            if let Some(obj) = state.as_object() {
                let entry = aux_published_attrs
                    .entry(publish_device_id.clone())
                    .or_default();
                for (k, v) in obj {
                    entry.insert(k.clone(), v.clone());
                }
            }

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

        // Clear what sensors no longer publish from homeCore's stored state,
        // once a run; compacted facets publish only partials, which never
        // remove a key.
        for device_id in aux_published_attrs.keys() {
            if registry.is_primary_device_id(device_id) || !registry.first_retirement(device_id) {
                continue;
            }
            publisher
                .publish_state_partial(
                    device_id,
                    &translator::retired_state(translator::RetiredKind::Sensor),
                )
                .await?;
        }

        // Sensors never described themselves. A Hue motion sensor
        // publishes motion, temperature, illuminance and battery, and a
        // client had to infer every one of them and demote the battery by
        // name. Lights, groups and scenes keep their own declared schemas —
        // this is only for the devices that had none.
        for (device_id, attrs) in &aux_published_attrs {
            if registry.is_primary_device_id(device_id) || attrs.is_empty() {
                continue;
            }
            let mut names: Vec<String> = attrs.keys().cloned().collect();
            names.sort();
            if !registry.aux_schema_changed(device_id, &names) {
                continue;
            }
            let schema = crate::aux_schema::schema_for_state(attrs);
            if let Err(e) = publisher.register_device_schema(device_id, &schema).await {
                warn!(device_id = %device_id, error = %e, "Failed to publish aux schema");
            }
        }

        for (device_id, attrs) in &aux_published_attrs {
            if registry.is_primary_device_id(device_id) {
                continue;
            }
            registry
                .set_alerts_if_changed(publisher, device_id, crate::alerts::for_accessory(attrs))
                .await;
        }

        for removed in registry.prune_aux_not_in(&target.bridge_id, &active_aux_raw_ids) {
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

    Ok(())
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
) -> Result<EventApplyOutcome> {
    let mut applied_any = false;
    let mut ignored_reason: Option<String> = None;
    let mut refresh_reason: Option<String> = None;

    if let Some(events) = payload.as_array() {
        for event in events {
            match apply_event_item(publisher, registry, bridge_id, event).await? {
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
        match apply_event_item(publisher, registry, bridge_id, payload).await? {
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
    // A battery or a lamp's reach changed: its alerts come from the whole
    // device, which a refresh reads in one request.
    let mut alerts_stale = false;
    let Some(data_items) = event.get("data").and_then(|v| v.as_array()) else {
        return Ok(EventApplyOutcome::Ignored {
            reason: "missing_data".to_string(),
        });
    };
    // Something added or removed in the Hue app: a new lamp, a deleted scene.
    // A patch cannot register a device, so read the bridge — one request now.
    if let Some(kind @ ("add" | "delete")) = event.get("type").and_then(|v| v.as_str()) {
        return Ok(EventApplyOutcome::NeedsRefresh {
            reason: format!("resource_{kind}"),
        });
    }

    for item in data_items {
        let Some(resource_type) = item.get("type").and_then(|v| v.as_str()) else {
            saw_missing_identity = true;
            continue;
        };
        let Some(rid) = item.get("id").and_then(|v| v.as_str()) else {
            saw_missing_identity = true;
            continue;
        };

        // A rename, or anything else about what a resource is called or is:
        // its registration says that, and a refresh is one request.
        if item.get("metadata").is_some() {
            alerts_stale = true;
        }

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
                        patch.insert(
                            "brightness_pct".to_string(),
                            json!(translator::published_brightness(brightness)),
                        );
                    }
                    // What is showing now: a temperature set says so with a
                    // valid mirek; a colour set invalidates it.
                    let mirek_valid = item
                        .get("color_temperature")
                        .and_then(|ct| ct.get("mirek_valid"))
                        .and_then(|v| v.as_bool());
                    // `color_temp` in kelvin; mirek stops at this edge. A
                    // mirek the bridge marks invalid (it sends one while a
                    // colour shows) or one outside any light's range is
                    // ignored rather than published as a temperature.
                    if let Some(kelvin) = item
                        .get("color_temperature")
                        .filter(|_| mirek_valid != Some(false))
                        .and_then(|ct| ct.get("mirek"))
                        .and_then(|v| v.as_u64())
                        .and_then(translator::kelvin_from_event_mirek)
                    {
                        patch.insert("color_temp".to_string(), json!(kelvin));
                    }
                    match mirek_valid {
                        Some(true) => {
                            patch.insert("color_mode".to_string(), json!("color_temp"));
                        }
                        Some(false) if item.get("color").is_some() => {
                            patch.insert("color_mode".to_string(), json!("color"));
                        }
                        _ => {}
                    }
                    if item
                        .get("gradient")
                        .and_then(|g| g.get("points"))
                        .and_then(|p| p.as_array())
                        .is_some_and(|p| p.len() > 1)
                    {
                        patch.insert("color_mode".to_string(), json!("gradient"));
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
                        .get("effects_v2")
                        .and_then(|e| e.get("status"))
                        .and_then(|s| s.get("effect"))
                        .or_else(|| item.get("effects").and_then(|e| e.get("status")))
                        .or_else(|| item.get("effects").and_then(|e| e.get("effect")))
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
                        patch.insert(
                            "brightness_pct".to_string(),
                            json!(translator::published_brightness(brightness)),
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
            "scene" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_scene_device_id(bridge_id, rid) {
                    applied = true;
                    // `status.active` is a string enum
                    // ("inactive" | "static" | "dynamic_palette"), not a bool —
                    // a recall flips the previous scene to "inactive" and the
                    // new one to "static", and the bridge emits both events, so
                    // this stays in sync even when the change came from the Hue
                    // app rather than homeCore.
                    if let Some(status) = item
                        .get("status")
                        .and_then(|s| s.get("active"))
                        .and_then(|v| v.as_str())
                    {
                        publisher
                            .publish_state_partial(
                                &device_id,
                                &json!({ "active": status != "inactive", "status": status }),
                            )
                            .await?;
                        applied = true;
                    }
                }
            }
            "smart_scene" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_scene_device_id(bridge_id, rid) {
                    applied = true;
                    if let Some(state) = item.get("state").and_then(|v| v.as_str()) {
                        publisher
                            .publish_state_partial(
                                &device_id,
                                &json!({ "active": state == "active", "status": state }),
                            )
                            .await?;
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
                    insert_temperature_patch(&mut patch, item);
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
                        patch.insert("open".to_string(), json!(open_from_contact_report(v)));
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
                // Its alert follows from the whole device; read it again.
                alerts_stale = true;
            }
            "device_software_update" => {
                saw_known_type = true;
                alerts_stale = true;
            }
            "zigbee_connectivity" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_aux_device_id(bridge_id, resource_type, rid)
                {
                    applied = true;
                    if let Some(status) = item.get("status").and_then(|v| v.as_str()) {
                        // A lamp the bridge cannot reach is not there.
                        publisher
                            .publish_availability(&device_id, status == "connected")
                            .await?;
                        if !registry.is_primary_device_id(&device_id) {
                            publisher
                                .publish_state_partial(
                                    &device_id,
                                    &json!({"connectivity_status": status}),
                                )
                                .await?;
                        }
                    }
                }
                alerts_stale = true;
            }
            "button" => {
                saw_known_type = true;
                if let Some(device_id) = registry.find_aux_device_id(bridge_id, resource_type, rid)
                {
                    applied = true;
                    let mut patch = serde_json::Map::new();
                    let mut button_event_value: Option<String> = None;

                    let report = item
                        .get("button")
                        .and_then(|b| b.get("button_report"))
                        .or_else(|| item.get("button_report"));
                    if let Some(v) = report.and_then(|r| r.get("event")).and_then(|v| v.as_str()) {
                        patch.insert("button_event".to_string(), json!(v));
                        button_event_value = Some(v.to_string());
                        // Which button, and how: "Brighter held".
                        if let (Some(label), Some(word)) =
                            (registry.button_label(rid), crate::hue::api::press_word(v))
                        {
                            patch.insert("last_button".to_string(), json!(label));
                            patch
                                .insert("last_press".to_string(), json!(format!("{label} {word}")));
                        }
                    }
                    if let Some(v) = report
                        .and_then(|r| r.get("updated"))
                        .and_then(|v| v.as_str())
                    {
                        patch.insert("button_updated".to_string(), json!(v));
                        patch.insert("last_pressed_at".to_string(), json!(v));
                    }
                    if let Some(v) = item
                        .get("button")
                        .and_then(|b| b.get("repeat_interval"))
                        .or_else(|| item.get("repeat_interval"))
                        .and_then(|v| v.as_u64())
                    {
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
                    let report = item
                        .get("relative_rotary")
                        .and_then(|r| r.get("rotary_report").or_else(|| r.get("last_event")))
                        .or_else(|| item.get("rotary_report"));
                    let action = report
                        .and_then(|r| r.get("action"))
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                    let direction = report
                        .and_then(|r| r.get("rotation"))
                        .and_then(|r| r.get("direction"))
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                    let steps = report
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
                    if let Some(v) = report
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
                    // CLIP v2 says `status: "active" | "inactive"`; older
                    // firmware said `status: {active: bool}`.
                    if let Some(v) = item
                        .get("status")
                        .and_then(|s| s.as_str().map(|s| s == "active"))
                        .or_else(|| {
                            item.get("status")
                                .and_then(|s| s.get("active"))
                                .and_then(|v| v.as_bool())
                        })
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

    if alerts_stale {
        Ok(EventApplyOutcome::NeedsRefresh {
            reason: "alerts".to_string(),
        })
    } else if applied {
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

#[cfg(test)]
mod tests {

    /// Every boolean names both of its states.
    ///
    /// A boolean attribute is two events, not one: a client given only one
    /// name offers one row, and the other direction needs a Not gate wrapped
    /// round the trigger. Half-declaring is worse than not declaring, because
    /// the client's own fallback lexicon is skipped for an attribute that then
    /// has no second name.
    #[test]
    fn every_boolean_names_both_of_its_states() {
        let light = crate::hue::models::HueLight::sample();
        let group: DeviceSchema = serde_json::from_value(crate::translator::group_schema(
            &crate::hue::models::HueGroupedLight::sample(),
        ))
        .unwrap();
        let schemas = [
            ("light", crate::translator::light_schema(&light)),
            ("group", group),
        ];
        for (label, schema) in schemas {
            for (name, attr) in &schema.attributes {
                if !matches!(attr.kind, AttributeKind::Bool) {
                    continue;
                }
                let s = attr
                    .states
                    .as_ref()
                    .unwrap_or_else(|| panic!("{label}.{name} is a bool with no state names"));
                assert!(!s.when_true.label.is_empty(), "{label}.{name}");
                assert!(!s.when_false.label.is_empty(), "{label}.{name}");
                assert_ne!(
                    s.when_true.label, s.when_false.label,
                    "{label}.{name} names both states the same thing"
                );
            }
        }
    }
    use super::*;
    use crate::hue::models::HueAuxDevice;
    use crate::hue::registry::HueRegistry;
    use plugin_sdk_rs::types::schema::{AttributeKind, DeviceSchema};
    use plugin_sdk_rs::DevicePublisher;
    use serde_json::json;

    fn dummy_publisher() -> DevicePublisher {
        DevicePublisher::test_instance("plugin.hue")
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

    /// Sensor readings publish under the shared names in the shared units —
    /// `temperature` °C, `illuminance` lux — with no unit siblings and no
    /// second spelling.
    #[test]
    fn sensor_patches_use_the_shared_names() {
        let mut attrs = serde_json::Map::new();
        insert_temperature_patch(
            &mut attrs,
            &json!({ "temperature": { "temperature": 2234.0, "temperature_valid": true } }),
        );
        insert_light_level_patch(
            &mut attrs,
            &json!({ "light": { "light_level": 20001.0, "light_level_valid": false } }),
        );
        assert_eq!(
            attrs.get("temperature").and_then(Value::as_f64),
            Some(22.34)
        );
        assert_eq!(
            attrs.get("temperature_valid").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            attrs.get("illuminance").and_then(Value::as_f64),
            Some(100.0)
        );
        assert_eq!(
            attrs.get("illuminance_valid").and_then(Value::as_bool),
            Some(false)
        );
        for gone in [
            "temperature_c",
            "temperature_f",
            "temperature_unit",
            "illuminance_raw",
            "illuminance_lux",
            "illuminance_unit",
        ] {
            assert!(attrs.get(gone).is_none(), "{gone} is not published");
        }
    }

    #[test]
    fn compacts_temp_lux_battery_to_motion_device() {
        let motion = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            area: None,
            owner_rid: "owner-1".to_string(),
            resource_type: "motion".to_string(),
            resource_id: "rid-motion".to_string(),
            device_id: "motion-dev".to_string(),
            name: "Sensor".to_string(),
            attributes: json!({}),
        };
        let temp = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            area: None,
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
            area: None,
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
            area: None,
            owner_rid: "owner-1".to_string(),
            resource_type: "grouped_motion".to_string(),
            resource_id: "rid-grouped-motion".to_string(),
            device_id: "grouped-motion-dev".to_string(),
            name: "Sensor".to_string(),
            attributes: json!({}),
        };
        let grouped_light = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            area: None,
            owner_rid: "owner-1".to_string(),
            resource_type: "grouped_light_level".to_string(),
            resource_id: "rid-grouped-light".to_string(),
            device_id: "grouped-light-dev".to_string(),
            name: "Sensor".to_string(),
            attributes: json!({}),
        };
        let motion = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            area: None,
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
            area: None,
            owner_rid: "owner-1".to_string(),
            resource_type: "grouped_motion".to_string(),
            resource_id: "rid-grouped-motion".to_string(),
            device_id: "grouped-motion-dev".to_string(),
            name: "Grouped Sensor".to_string(),
            attributes: json!({}),
        };
        let bridge_home = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            area: None,
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
            area: None,
            owner_rid: "owner-remote".to_string(),
            resource_type: "button".to_string(),
            resource_id: "rid-button".to_string(),
            device_id: "button-dev".to_string(),
            name: "Dial Switch".to_string(),
            attributes: json!({}),
        };
        let rotary = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            area: None,
            owner_rid: "owner-remote".to_string(),
            resource_type: "relative_rotary".to_string(),
            resource_id: "rid-rotary".to_string(),
            device_id: "rotary-dev".to_string(),
            name: "Dial Switch".to_string(),
            attributes: json!({}),
        };
        let battery = HueAuxDevice {
            bridge_id: "bridge-1".to_string(),
            area: None,
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
            area: None,
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
            owner_rid: None,
            light_ids: vec![],
            supports_color: false,
            supports_color_temp: false,
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

        let outcome = apply_eventstream_update(&publisher, &registry, "bridge-1", &payload)
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

        let outcome = apply_eventstream_update(&publisher, &registry, "bridge-1", &payload)
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

        let outcome = apply_eventstream_update(&publisher, &registry, "bridge-1", &payload)
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
