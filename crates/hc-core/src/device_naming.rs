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

#[cfg(test)]
mod tests {
    use super::{
        canonical_name_base, ensure_unique_canonical_name, normalize_canonical_name,
        normalize_name_segment,
    };
    use hc_types::device::DeviceState;

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
