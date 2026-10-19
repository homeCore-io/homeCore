use std::collections::{HashMap, HashSet};

use super::models::{BridgeSnapshot, HueAuxDevice, HueGroupedLight, HueLight, HueScene};

#[derive(Debug, Clone)]
pub struct RegisteredLight {
    pub bridge_id: String,
    pub light_rid: String,
    pub supports_gradient: bool,
    pub supports_identify: bool,
    pub effect_values: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RegisteredGroup {
    pub bridge_id: String,
    pub group_rid: String,
}

#[derive(Debug, Clone)]
pub struct RegisteredScene {
    pub bridge_id: String,
    pub scene_rid: String,
    /// Whether the bridge reported `status.active` for this scene, which is
    /// what the published schema claims. Kept so a scene whose answer changes
    /// — a new scene, a bridge that gains the field — republishes its schema
    /// rather than declaring a state row that never fills in.
    pub reports_status: bool,
}

#[derive(Debug, Clone)]
pub struct RegisteredAux {
    pub bridge_id: String,
    pub resource_type: String,
    pub resource_rid: String,
    pub publish_device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxUpsertResult {
    pub newly_seen: bool,
    pub previous_publish_device_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct HueRegistry {
    by_device_id: HashMap<String, BridgeSnapshot>,
    lights_by_device_id: HashMap<String, RegisteredLight>,
    groups_by_device_id: HashMap<String, RegisteredGroup>,
    scenes_by_device_id: HashMap<String, RegisteredScene>,
    aux_by_device_id: HashMap<String, RegisteredAux>,
    /// publish device id → the attribute names its published schema covers.
    aux_schema_attrs: HashMap<String, Vec<String>>,
    /// bridge id → its rooms and zones, for the plugin page.
    rooms: HashMap<String, Vec<serde_json::Value>>,
    /// button rid → what the button is called ("Brighter").
    button_labels: HashMap<String, String>,
    /// device id → the alerts last said for it, as JSON, so an unchanged list
    /// is not said again every resync.
    alerts_said: HashMap<String, String>,
    /// bridge device id → its own `device_software_update` state.
    bridge_updates: HashMap<String, String>,
}

impl HueRegistry {
    /// A bridge's `update_state`, from its last summary.
    pub fn bridge_update_state(&self, device_id: &str) -> Option<&str> {
        self.bridge_updates.get(device_id).map(String::as_str)
    }

    pub fn set_bridge_update_state(&mut self, device_id: &str, state: Option<&str>) {
        match state {
            Some(s) => self
                .bridge_updates
                .insert(device_id.to_string(), s.to_string()),
            None => self.bridge_updates.remove(device_id),
        };
    }

    pub fn set_rooms(&mut self, bridge_id: &str, rooms: Vec<serde_json::Value>) {
        self.rooms.insert(bridge_id.to_string(), rooms);
    }

    /// Every bridge's rooms and zones.
    pub fn rooms(&self) -> Vec<serde_json::Value> {
        let mut out: Vec<_> = self.rooms.values().flatten().cloned().collect();
        // A room is a device only when it is published as one.
        for r in &mut out {
            let published = r
                .get("device_id")
                .and_then(|d| d.as_str())
                .is_some_and(|d| self.groups_by_device_id.contains_key(d));
            if !published {
                r["device_id"] = serde_json::Value::Null;
            }
        }
        out.sort_by(|a, b| {
            let k = |v: &serde_json::Value, f: &str| {
                v.get(f).and_then(|x| x.as_str()).unwrap_or("").to_string()
            };
            (k(a, "kind"), k(a, "name")).cmp(&(k(b, "kind"), k(b, "name")))
        });
        out
    }

    pub fn set_button_labels(&mut self, labels: HashMap<String, String>) {
        self.button_labels.extend(labels);
    }

    pub fn button_label(&self, rid: &str) -> Option<&str> {
        self.button_labels.get(rid).map(String::as_str)
    }

    /// Say a device's alerts when they differ from what was last said.
    pub async fn set_alerts_if_changed(
        &mut self,
        publisher: &plugin_sdk_rs::DevicePublisher,
        device_id: &str,
        alerts: Vec<plugin_sdk_rs::types::PluginAlert>,
    ) {
        let said = serde_json::to_string(&alerts).unwrap_or_default();
        if self.alerts_said.get(device_id) == Some(&said) {
            return;
        }
        // An empty list is said once too: alerts are retained, and one left
        // from before a restart (a battery changed meanwhile) must clear.
        match publisher.set_alerts(device_id, &alerts).await {
            Ok(()) => {
                self.alerts_said.insert(device_id.to_string(), said);
            }
            Err(e) => tracing::warn!(device_id, error = %e, "Failed to publish Hue alerts"),
        }
    }

    pub fn upsert_bridge(&mut self, device_id: String, snapshot: BridgeSnapshot) {
        self.by_device_id.insert(device_id, snapshot);
    }

    pub fn bridge_count(&self) -> usize {
        self.by_device_id.len()
    }

    pub fn online_count(&self) -> usize {
        self.by_device_id
            .values()
            .filter(|snapshot| snapshot.online)
            .count()
    }

    pub fn total_summary_bytes(&self) -> usize {
        self.by_device_id
            .values()
            .map(|snapshot| snapshot.summary.to_string().len())
            .sum()
    }

    pub fn ensure_light(&mut self, light: &HueLight) -> bool {
        let existed = self.lights_by_device_id.contains_key(&light.device_id);
        self.lights_by_device_id.insert(
            light.device_id.clone(),
            RegisteredLight {
                bridge_id: light.bridge_id.clone(),
                light_rid: light.resource_id.clone(),
                supports_gradient: light.supports_gradient,
                supports_identify: light.supports_identify,
                effect_values: light.effect_values.clone(),
            },
        );
        !existed
    }

    pub fn get_light_binding(&self, device_id: &str) -> Option<&RegisteredLight> {
        self.lights_by_device_id.get(device_id)
    }

    pub fn light_count(&self) -> usize {
        self.lights_by_device_id.len()
    }

    pub fn find_light_device_id(&self, bridge_id: &str, light_rid: &str) -> Option<String> {
        self.lights_by_device_id
            .iter()
            .find_map(|(device_id, binding)| {
                if binding.bridge_id == bridge_id && binding.light_rid == light_rid {
                    Some(device_id.clone())
                } else {
                    None
                }
            })
    }

    pub fn ensure_group(&mut self, group: &HueGroupedLight) -> bool {
        let existed = self.groups_by_device_id.contains_key(&group.device_id);
        self.groups_by_device_id.insert(
            group.device_id.clone(),
            RegisteredGroup {
                bridge_id: group.bridge_id.clone(),
                group_rid: group.resource_id.clone(),
            },
        );
        !existed
    }

    pub fn get_group_binding(&self, device_id: &str) -> Option<&RegisteredGroup> {
        self.groups_by_device_id.get(device_id)
    }

    pub fn group_count(&self) -> usize {
        self.groups_by_device_id.len()
    }

    pub fn find_group_device_id(&self, bridge_id: &str, group_rid: &str) -> Option<String> {
        self.groups_by_device_id
            .iter()
            .find_map(|(device_id, binding)| {
                if binding.bridge_id == bridge_id && binding.group_rid == group_rid {
                    Some(device_id.clone())
                } else {
                    None
                }
            })
    }

    pub fn ensure_scene(&mut self, scene: &HueScene) -> bool {
        if self.scenes_by_device_id.contains_key(&scene.device_id) {
            return false;
        }
        self.scenes_by_device_id.insert(
            scene.device_id.clone(),
            RegisteredScene {
                bridge_id: scene.bridge_id.clone(),
                scene_rid: scene.resource_id.clone(),
                reports_status: scene.active.is_some(),
            },
        );
        true
    }

    /// Whether what this scene can report has changed since its schema was
    /// published, recording the new answer.
    ///
    /// False for a scene just added by [`Self::ensure_scene`] — that one's
    /// schema goes out with its registration.
    pub fn scene_status_support_changed(&mut self, scene: &HueScene) -> bool {
        let Some(known) = self.scenes_by_device_id.get_mut(&scene.device_id) else {
            return false;
        };
        let reports = scene.active.is_some();
        if known.reports_status == reports {
            return false;
        }
        known.reports_status = reports;
        true
    }

    pub fn get_scene_binding(&self, device_id: &str) -> Option<&RegisteredScene> {
        self.scenes_by_device_id.get(device_id)
    }

    pub fn scene_count(&self) -> usize {
        self.scenes_by_device_id.len()
    }

    pub fn find_scene_device_id(&self, bridge_id: &str, scene_rid: &str) -> Option<String> {
        self.scenes_by_device_id
            .iter()
            .find_map(|(device_id, binding)| {
                if binding.bridge_id == bridge_id && binding.scene_rid == scene_rid {
                    Some(device_id.clone())
                } else {
                    None
                }
            })
    }

    pub fn upsert_aux(&mut self, aux: &HueAuxDevice, publish_device_id: &str) -> AuxUpsertResult {
        let previous_publish_device_id =
            self.aux_by_device_id
                .get(&aux.device_id)
                .and_then(|existing| {
                    if existing.publish_device_id != publish_device_id {
                        Some(existing.publish_device_id.clone())
                    } else {
                        None
                    }
                });
        let newly_seen = !self.aux_by_device_id.contains_key(&aux.device_id);
        self.aux_by_device_id.insert(
            aux.device_id.clone(),
            RegisteredAux {
                bridge_id: aux.bridge_id.clone(),
                resource_type: aux.resource_type.clone(),
                resource_rid: aux.resource_id.clone(),
                publish_device_id: publish_device_id.to_string(),
            },
        );
        AuxUpsertResult {
            newly_seen,
            previous_publish_device_id,
        }
    }

    pub fn aux_count(&self) -> usize {
        self.aux_by_device_id.len()
    }

    pub fn get_aux_binding(&self, device_id: &str) -> Option<&RegisteredAux> {
        self.aux_by_device_id.get(device_id)
    }

    pub fn find_aux_device_id(
        &self,
        bridge_id: &str,
        resource_type: &str,
        resource_rid: &str,
    ) -> Option<String> {
        self.aux_by_device_id.values().find_map(|binding| {
            if binding.bridge_id == bridge_id
                && binding.resource_type == resource_type
                && binding.resource_rid == resource_rid
            {
                Some(binding.publish_device_id.clone())
            } else {
                None
            }
        })
    }

    /// Whether an auxiliary device's published attribute set has changed since
    /// its schema was last described, recording the new set.
    ///
    /// A sensor's attributes depend on which Hue resources compacted onto it,
    /// so the schema is derived from what it published rather than declared up
    /// front — and republished only when that set changes, not on every poll.
    pub fn aux_schema_changed(&mut self, device_id: &str, names: &[String]) -> bool {
        if self.aux_schema_attrs.get(device_id).map(Vec::as_slice) == Some(names) {
            return false;
        }
        self.aux_schema_attrs
            .insert(device_id.to_string(), names.to_vec());
        true
    }

    /// Forget what has already been published, without forgetting the
    /// bindings that make commands work.
    ///
    /// **Publication is gated on first sighting** — `ensure_light` and friends
    /// return false for a device already in the registry, so a refresh
    /// re-reads the bridge and republishes nothing. That is right for the
    /// periodic loop and wrong for the operator pressing *Refresh devices*,
    /// which is the button somebody reaches for when a device looks wrong: a
    /// schema lost from core (an unregister deletes it, and so does a restore
    /// from an older backup) could only be restored by restarting the plugin.
    ///
    /// Clearing the *seen* sets makes the next pass re-register and republish
    /// everything exactly once. The `rid`/`publish_device_id` bindings live in
    /// the same maps, so they are rebuilt by that pass before anything reads
    /// them, and pruning still works: it runs on what the pass just saw.
    pub fn forget_publications(&mut self) {
        self.lights_by_device_id.clear();
        self.groups_by_device_id.clear();
        self.scenes_by_device_id.clear();
        self.aux_by_device_id.clear();
        self.aux_schema_attrs.clear();
    }

    pub fn is_primary_device_id(&self, device_id: &str) -> bool {
        self.lights_by_device_id.contains_key(device_id)
            || self.groups_by_device_id.contains_key(device_id)
            || self.scenes_by_device_id.contains_key(device_id)
            || self.by_device_id.contains_key(device_id)
    }

    pub fn is_aux_publish_device_id_referenced(&self, device_id: &str) -> bool {
        self.aux_by_device_id
            .values()
            .any(|binding| binding.publish_device_id == device_id)
    }

    pub fn prune_groups_not_in(
        &mut self,
        bridge_id: &str,
        keep_device_ids: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        // Only this bridge's: another bridge's devices are not in its read.
        let stale = self
            .groups_by_device_id
            .iter()
            .filter(|(device_id, b)| {
                b.bridge_id.eq_ignore_ascii_case(bridge_id) && !keep_device_ids.contains(*device_id)
            })
            .map(|(device_id, _)| device_id)
            .cloned()
            .collect::<Vec<_>>();
        for device_id in &stale {
            self.groups_by_device_id.remove(device_id);
        }
        stale
    }

    /// Drop any light registration whose device_id isn't in `keep_device_ids`,
    /// returning the removed ids so the caller can issue an
    /// `unregister_device` for each.
    pub fn prune_lights_not_in(
        &mut self,
        bridge_id: &str,
        keep_device_ids: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        // Only this bridge's: another bridge's devices are not in its read.
        let stale = self
            .lights_by_device_id
            .iter()
            .filter(|(device_id, b)| {
                b.bridge_id.eq_ignore_ascii_case(bridge_id) && !keep_device_ids.contains(*device_id)
            })
            .map(|(device_id, _)| device_id)
            .cloned()
            .collect::<Vec<_>>();
        for device_id in &stale {
            self.lights_by_device_id.remove(device_id);
        }
        stale
    }

    /// Drop any scene registration whose device_id isn't in `keep_device_ids`,
    /// returning the removed ids so the caller can issue an
    /// `unregister_device` for each.
    pub fn prune_scenes_not_in(
        &mut self,
        bridge_id: &str,
        keep_device_ids: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        // Only this bridge's: another bridge's devices are not in its read.
        let stale = self
            .scenes_by_device_id
            .iter()
            .filter(|(device_id, b)| {
                b.bridge_id.eq_ignore_ascii_case(bridge_id) && !keep_device_ids.contains(*device_id)
            })
            .map(|(device_id, _)| device_id)
            .cloned()
            .collect::<Vec<_>>();
        for device_id in &stale {
            self.scenes_by_device_id.remove(device_id);
        }
        stale
    }

    pub fn prune_aux_not_in(
        &mut self,
        bridge_id: &str,
        keep_raw_device_ids: &std::collections::HashSet<String>,
    ) -> Vec<RegisteredAux> {
        let stale_keys = self
            .aux_by_device_id
            .iter()
            .filter(|(device_id, b)| {
                b.bridge_id.eq_ignore_ascii_case(bridge_id)
                    && !keep_raw_device_ids.contains(*device_id)
            })
            .map(|(device_id, _)| device_id)
            .cloned()
            .collect::<Vec<_>>();

        let mut removed = Vec::new();
        for device_id in stale_keys {
            if let Some(binding) = self.aux_by_device_id.remove(&device_id) {
                removed.push(binding);
            }
        }
        removed
    }

    /// Snapshot every device_id this plugin has registered with homeCore
    /// as of the last successful sync — bridges, lights, groups, scenes,
    /// aux devices, and any aux compact-publish targets. Fed into
    /// `DevicePublisher::reconcile_devices` as the authoritative live
    /// set; the SDK handles diff + unregister + persistence.
    pub fn all_device_ids(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        out.extend(self.by_device_id.keys().cloned());
        out.extend(self.lights_by_device_id.keys().cloned());
        out.extend(self.groups_by_device_id.keys().cloned());
        out.extend(self.scenes_by_device_id.keys().cloned());
        out.extend(self.aux_by_device_id.keys().cloned());
        for binding in self.aux_by_device_id.values() {
            out.insert(binding.publish_device_id.clone());
        }
        out
    }

    /// Forget every registration belonging to one bridge — the bridge device
    /// itself (`bridge_device_id`, i.e. `target.device_id()`) plus all of its
    /// lights, groups, scenes and aux devices (and their compact-publish
    /// targets). Returns the removed device_ids, deduped, so the caller can
    /// reconcile them out of homeCore. Bridge id is matched case-insensitively
    /// (Hue reports uppercase; config stores lowercase). Used by
    /// `unpair_bridge` to delete a bridge's devices when it's removed.
    pub fn remove_all_for_bridge(
        &mut self,
        bridge_id: &str,
        bridge_device_id: &str,
    ) -> Vec<String> {
        let mut removed: HashSet<String> = HashSet::new();
        let matches = |b: &str| b.eq_ignore_ascii_case(bridge_id);

        if self.by_device_id.remove(bridge_device_id).is_some() {
            removed.insert(bridge_device_id.to_string());
        }

        let light_ids: Vec<String> = self
            .lights_by_device_id
            .iter()
            .filter(|(_, b)| matches(&b.bridge_id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in light_ids {
            self.lights_by_device_id.remove(&id);
            removed.insert(id);
        }

        let group_ids: Vec<String> = self
            .groups_by_device_id
            .iter()
            .filter(|(_, b)| matches(&b.bridge_id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in group_ids {
            self.groups_by_device_id.remove(&id);
            removed.insert(id);
        }

        let scene_ids: Vec<String> = self
            .scenes_by_device_id
            .iter()
            .filter(|(_, b)| matches(&b.bridge_id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in scene_ids {
            self.scenes_by_device_id.remove(&id);
            removed.insert(id);
        }

        let aux_ids: Vec<String> = self
            .aux_by_device_id
            .iter()
            .filter(|(_, b)| matches(&b.bridge_id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in aux_ids {
            if let Some(binding) = self.aux_by_device_id.remove(&id) {
                removed.insert(id);
                removed.insert(binding.publish_device_id);
            }
        }

        removed.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hue::models::{HueAuxDevice, HueScene};
    use serde_json::json;
    use std::collections::HashSet;

    fn scene(device_id: &str, active: Option<bool>) -> HueScene {
        HueScene {
            bridge_id: "bridge-1".to_string(),
            resource_id: format!("rid-{device_id}"),
            device_id: device_id.to_string(),
            name: "Evening".to_string(),
            area: None,
            group_kind: None,
            active,
            group_rid: None,
            group_name: None,
            palette: vec![],
            status: None,
            dynamic_capable: false,
        }
    }

    /// **The button somebody presses when a device looks wrong.** Publication
    /// is gated on first sighting, so a refresh republished nothing and a
    /// schema core had lost could only be restored by restarting the plugin.
    #[test]
    fn forgetting_publications_makes_the_next_pass_say_everything_again() {
        let mut registry = HueRegistry::default();
        let s = scene("hue_scene_1", Some(true));
        assert!(registry.ensure_scene(&s));
        assert!(!registry.ensure_scene(&s), "already published");

        registry.forget_publications();

        assert!(
            registry.ensure_scene(&s),
            "after forgetting, the next pass republishes"
        );
    }

    /// A freshly registered scene publishes its schema with its registration,
    /// so it must not also count as a change.
    #[test]
    fn a_new_scene_is_not_a_change() {
        let mut registry = HueRegistry::default();
        let s = scene("hue_scene_1", Some(false));
        assert!(registry.ensure_scene(&s));
        assert!(!registry.scene_status_support_changed(&s));
    }

    /// A bridge that starts reporting `status.active` — or a scene replaced by
    /// one that does — has to republish, or the schema keeps denying a state
    /// the device now publishes.
    #[test]
    fn gaining_or_losing_status_republishes_once() {
        let mut registry = HueRegistry::default();
        registry.ensure_scene(&scene("hue_scene_1", None));

        assert!(registry.scene_status_support_changed(&scene("hue_scene_1", Some(true))));
        assert!(!registry.scene_status_support_changed(&scene("hue_scene_1", Some(false))));
        assert!(registry.scene_status_support_changed(&scene("hue_scene_1", None)));
    }

    fn aux(device_id: &str, publish_device_id: &str) -> (HueAuxDevice, String) {
        (
            HueAuxDevice {
                bridge_id: "bridge-1".to_string(),
                area: None,
                owner_rid: "owner-1".to_string(),
                resource_type: "grouped_motion".to_string(),
                resource_id: format!("rid-{device_id}"),
                device_id: device_id.to_string(),
                name: "Aux".to_string(),
                attributes: json!({}),
            },
            publish_device_id.to_string(),
        )
    }

    #[test]
    fn upsert_aux_tracks_publish_device_changes() {
        let mut registry = HueRegistry::default();
        let (aux, first_publish_id) = aux("aux-1", "standalone-dev");

        let first = registry.upsert_aux(&aux, &first_publish_id);
        assert!(first.newly_seen);
        assert_eq!(first.previous_publish_device_id, None);

        let second = registry.upsert_aux(&aux, "merged-dev");
        assert!(!second.newly_seen);
        assert_eq!(
            second.previous_publish_device_id,
            Some("standalone-dev".to_string())
        );
        assert_eq!(
            registry.find_aux_device_id("bridge-1", "grouped_motion", "rid-aux-1"),
            Some("merged-dev".to_string())
        );
    }

    #[test]
    fn prune_aux_not_in_returns_removed_bindings() {
        let mut registry = HueRegistry::default();
        let (aux1, publish1) = aux("aux-1", "publish-1");
        let (aux2, publish2) = aux("aux-2", "publish-2");
        registry.upsert_aux(&aux1, &publish1);
        registry.upsert_aux(&aux2, &publish2);

        let keep = HashSet::from([String::from("aux-2")]);
        let removed = registry.prune_aux_not_in("bridge-1", &keep);

        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].publish_device_id, "publish-1");
        assert_eq!(registry.aux_count(), 1);
    }

    /// With two bridges, reading one must not drop the other's devices: it
    /// used to, so every device of a second bridge was unregistered and
    /// registered again on every resync.
    #[test]
    fn pruning_one_bridge_leaves_the_other_alone() {
        let mut registry = HueRegistry::default();
        let mut a = crate::hue::models::HueLight::sample();
        a.bridge_id = "AAAA".into();
        a.device_id = "hue_aaaa_light_1".into();
        let mut b = crate::hue::models::HueLight::sample();
        b.bridge_id = "bbbb".into();
        b.device_id = "hue_bbbb_light_1".into();
        registry.ensure_light(&a);
        registry.ensure_light(&b);

        let removed = registry.prune_lights_not_in("bbbb", &HashSet::new());
        assert_eq!(removed, vec!["hue_bbbb_light_1".to_string()]);
        assert!(registry.get_light_binding("hue_aaaa_light_1").is_some());
    }

    #[test]
    fn remove_all_for_bridge_removes_only_that_bridge_case_insensitively() {
        let mut registry = HueRegistry::default();

        // Two bridges' aux devices; bridge-1 stored uppercase (as Hue reports).
        let (mut aux_a, _) = aux("aux-a", "pub-a");
        aux_a.bridge_id = "AABBCC".to_string();
        let (mut aux_b, _) = aux("aux-b", "pub-b");
        aux_b.bridge_id = "ddeeff".to_string();
        registry.upsert_aux(&aux_a, "pub-a");
        registry.upsert_aux(&aux_b, "pub-b");
        // The bridge's own device row for bridge-1.
        registry.upsert_bridge(
            "hue_aabbcc_bridge".to_string(),
            BridgeSnapshot {
                online: true,
                summary: json!({}),
            },
        );

        // Request lowercase — must still match the uppercase-stored bridge_id.
        let removed: HashSet<String> = registry
            .remove_all_for_bridge("aabbcc", "hue_aabbcc_bridge")
            .into_iter()
            .collect();

        assert!(removed.contains("hue_aabbcc_bridge")); // bridge device
        assert!(removed.contains("aux-a")); // its aux device
        assert!(removed.contains("pub-a")); // its aux compact-publish target
        assert_eq!(removed.len(), 3);

        // The other bridge is untouched.
        assert_eq!(registry.bridge_count(), 0);
        assert_eq!(registry.aux_count(), 1);
        assert_eq!(
            registry.find_aux_device_id("ddeeff", "grouped_motion", "rid-aux-b"),
            Some("pub-b".to_string())
        );
    }
}
