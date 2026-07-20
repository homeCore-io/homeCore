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
}

impl HueRegistry {
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
            },
        );
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
        keep_device_ids: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        let stale = self
            .groups_by_device_id
            .keys()
            .filter(|device_id| !keep_device_ids.contains(*device_id))
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
        keep_device_ids: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        let stale = self
            .lights_by_device_id
            .keys()
            .filter(|device_id| !keep_device_ids.contains(*device_id))
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
        keep_device_ids: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        let stale = self
            .scenes_by_device_id
            .keys()
            .filter(|device_id| !keep_device_ids.contains(*device_id))
            .cloned()
            .collect::<Vec<_>>();
        for device_id in &stale {
            self.scenes_by_device_id.remove(device_id);
        }
        stale
    }

    pub fn prune_aux_not_in(
        &mut self,
        keep_raw_device_ids: &std::collections::HashSet<String>,
    ) -> Vec<RegisteredAux> {
        let stale_keys = self
            .aux_by_device_id
            .keys()
            .filter(|device_id| !keep_raw_device_ids.contains(*device_id))
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
    use crate::hue::models::HueAuxDevice;
    use serde_json::json;
    use std::collections::HashSet;

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
        let removed = registry.prune_aux_not_in(&keep);

        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].publish_device_id, "publish-1");
        assert_eq!(registry.aux_count(), 1);
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
