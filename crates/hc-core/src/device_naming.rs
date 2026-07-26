use anyhow::{anyhow, Result};
use hc_state::StateStore;
use hc_types::device::DeviceState;

pub fn normalize_name_segment(value: &str) -> String {
    let raw: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();

    raw.split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

pub fn normalize_canonical_name(value: &str) -> Option<String> {
    let segments: Vec<String> = value
        .split('.')
        .map(normalize_name_segment)
        .filter(|part| !part.is_empty())
        .collect();

    if segments.is_empty() {
        None
    } else {
        Some(segments.join("."))
    }
}

pub fn canonical_name_base(device: &DeviceState) -> String {
    let name = normalize_name_segment(&device.name);
    let area = device
        .area
        .as_deref()
        .map(normalize_name_segment)
        .filter(|part| !part.is_empty());

    match (area, name.is_empty()) {
        (Some(area), false) => format!("{area}.{name}"),
        (Some(area), true) => area,
        (None, false) => name,
        (None, true) => normalize_name_segment(&device.device_id),
    }
}

pub fn ensure_unique_canonical_name(device: &DeviceState, devices: &[DeviceState]) -> String {
    let base = canonical_name_base(device);
    let device_suffix = normalize_name_segment(&device.device_id);

    let is_available = |candidate: &str| {
        devices.iter().all(|other| {
            other.device_id == device.device_id
                || other.canonical_name.as_deref() != Some(candidate)
        })
    };

    if is_available(&base) {
        return base;
    }

    let suffixed = format!("{base}_{device_suffix}");
    if is_available(&suffixed) {
        return suffixed;
    }

    let mut counter = 2usize;
    loop {
        let candidate = format!("{suffixed}_{counter}");
        if is_available(&candidate) {
            return candidate;
        }
        counter += 1;
    }
}

pub fn validate_or_generate_canonical_name(
    device: &DeviceState,
    devices: &[DeviceState],
    requested: Option<&str>,
) -> Result<String> {
    let candidate = match requested {
        Some(raw) => normalize_canonical_name(raw).ok_or_else(|| {
            anyhow!("canonical_name must contain at least one alphanumeric character")
        })?,
        None => ensure_unique_canonical_name(device, devices),
    };

    if devices.iter().any(|other| {
        other.device_id != device.device_id
            && other.canonical_name.as_deref() == Some(candidate.as_str())
    }) {
        return Err(anyhow!("canonical_name '{}' is already in use", candidate));
    }

    Ok(candidate)
}

pub async fn backfill_missing_canonical_names(store: &StateStore) -> Result<usize> {
    let mut devices = store.list_devices().await?;
    let mut updated = 0usize;

    for idx in 0..devices.len() {
        if devices[idx].canonical_name.is_some() {
            continue;
        }

        let canonical_name = ensure_unique_canonical_name(&devices[idx], &devices);
        devices[idx].canonical_name = Some(canonical_name);
        store.upsert_device(&devices[idx]).await?;
        updated += 1;
    }

    Ok(updated)
}

/// Rewrite any `device.area` that isn't already a normalized slug.
///
/// `device.area` is supposed to hold the normalized form (`living_room`), with
/// the UI deriving a pretty label from it. Plugin registrations used to store
/// the upstream label verbatim, so devices from Z-Wave and friends carried
/// `"Living Room"` while devices assigned through the API carried
/// `"living_room"`. Anything grouping by the raw string then saw two rooms and
/// put the devices in neither.
///
/// Registrations are normalized at ingest now, so a device heals itself as soon
/// as its plugin next registers it — but only if that plugin is running. Devices
/// belonging to a stopped or removed plugin would stay split forever, so sweep
/// them once at startup. Idempotent: an already-normalized area is left alone.
pub async fn migrate_area_names(store: &StateStore) -> Result<usize> {
    let devices = store.list_devices().await?;
    let mut updated = 0usize;

    for mut device in devices {
        let Some(area) = device.area.as_deref() else {
            continue;
        };
        let normalized = normalize_name_segment(area);
        if normalized == area {
            continue;
        }

        device.area = if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        };
        store.upsert_device(&device).await?;
        updated += 1;
    }

    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_name_base, ensure_unique_canonical_name, migrate_area_names,
        normalize_canonical_name, normalize_name_segment,
    };
    use hc_types::device::DeviceState;

    #[test]
    fn area_labels_normalize_to_the_stored_form_and_stay_put() {
        // Plugins report whatever the upstream system calls the room. These are
        // the exact shapes Z-Wave JS sends.
        assert_eq!(normalize_name_segment("Living Room"), "living_room");
        assert_eq!(normalize_name_segment("Equipment Room"), "equipment_room");
        assert_eq!(normalize_name_segment("Master Bedroom"), "master_bedroom");

        // Idempotence is what lets the migration run on every boot and skip rows
        // that are already correct.
        for slug in ["living_room", "equipment_room", "master_bedroom"] {
            assert_eq!(normalize_name_segment(slug), slug);
        }
    }

    #[tokio::test]
    async fn migration_reunites_a_room_split_across_raw_and_normalized_names() {
        let tmp = std::env::temp_dir().join(format!("hc_area_mig_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("temp dir");
        let store = hc_state::StateStore::open(
            tmp.join("state.redb").to_str().unwrap(),
            tmp.join("history.db").to_str().unwrap(),
        )
        .await
        .expect("store opens");

        // A plugin-registered device (raw upstream label) and an API-assigned
        // device (normalized slug) that are supposed to be in the same room.
        let mut from_plugin = DeviceState::new("zwave_9", "Lock", "plugin.zwave");
        from_plugin.area = Some("Living Room".into());
        store.upsert_device(&from_plugin).await.unwrap();

        let mut from_api = DeviceState::new("lutron_17", "Overhead", "plugin.lutron");
        from_api.area = Some("living_room".into());
        store.upsert_device(&from_api).await.unwrap();

        // Blank-ish areas should clear, not become "_".
        let mut junk = DeviceState::new("zwave_47", "Unplaced", "plugin.zwave");
        junk.area = Some("   ".into());
        store.upsert_device(&junk).await.unwrap();

        let updated = migrate_area_names(&store).await.expect("migration runs");
        assert_eq!(updated, 2, "the raw-label device and the blank one");

        let areas = |id: &str| {
            let store = store.clone();
            let id = id.to_string();
            async move { store.get_device(&id).await.unwrap().unwrap().area }
        };

        // Both devices now agree on one room, so nothing downstream can split
        // them into a duplicate.
        assert_eq!(areas("zwave_9").await.as_deref(), Some("living_room"));
        assert_eq!(areas("lutron_17").await.as_deref(), Some("living_room"));
        assert_eq!(areas("zwave_47").await, None);

        // Idempotent: a second pass has nothing left to do.
        assert_eq!(migrate_area_names(&store).await.unwrap(), 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn normalizes_segments() {
        assert_eq!(normalize_name_segment("Floor Lamp"), "floor_lamp");
        assert_eq!(normalize_name_segment("CO2 Sensor"), "co2_sensor");
        assert_eq!(normalize_name_segment("  "), "");
    }

    #[test]
    fn normalizes_canonical_name() {
        assert_eq!(
            normalize_canonical_name("Living Room.Floor Lamp"),
            Some("living_room.floor_lamp".into())
        );
        assert_eq!(normalize_canonical_name("..."), None);
    }

    #[test]
    fn builds_area_based_base() {
        let mut device = DeviceState::new("hue_a", "Floor Lamp", "plugin.test");
        device.area = Some("Living Room".into());
        assert_eq!(canonical_name_base(&device), "living_room.floor_lamp");
    }

    #[test]
    fn uniquifies_collisions() {
        let mut first = DeviceState::new("lamp_a", "Floor Lamp", "plugin.test");
        first.area = Some("Living Room".into());
        first.canonical_name = Some("living_room.floor_lamp".into());

        let mut second = DeviceState::new("lamp_b", "Floor Lamp", "plugin.test");
        second.area = Some("Living Room".into());

        assert_eq!(
            ensure_unique_canonical_name(&second, &[first]),
            "living_room.floor_lamp_lamp_b"
        );
    }
}
