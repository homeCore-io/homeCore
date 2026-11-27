//! The profiles, device type catalog and transforms homeCore ships, built in
//! so they are there to turn on whatever the install (a Docker image has no
//! `config/profiles/examples`). The files themselves stay the source:
//! `config/profiles/examples/`.

/// Each shipped profile, by its file name.
pub const PROFILES: &[(&str, &str)] = &[
    (
        "tasmota.toml",
        include_str!("../../../config/profiles/examples/tasmota.toml"),
    ),
    (
        "shelly-gen1.toml",
        include_str!("../../../config/profiles/examples/shelly-gen1.toml"),
    ),
    (
        "shelly-gen2.toml",
        include_str!("../../../config/profiles/examples/shelly-gen2.toml"),
    ),
    (
        "zigbee2mqtt.toml",
        include_str!("../../../config/profiles/examples/zigbee2mqtt.toml"),
    ),
];

/// The device type catalog.
pub const DEVICE_TYPES: &str = include_str!("../../../config/profiles/examples/device-types.toml");

/// The transforms the shipped profiles use, by file name.
pub const TRANSFORMS: &[(&str, &str)] = &[(
    "custom-transforms.rhai",
    include_str!("../../../config/profiles/examples/custom-transforms.rhai"),
)];

#[cfg(test)]
mod tests {
    #[test]
    fn every_shipped_profile_and_the_catalog_parse() {
        for (file, src) in super::PROFILES {
            crate::loader::load_profile_str(src).unwrap_or_else(|e| panic!("{file}: {e}"));
        }
        crate::DeviceTypeRegistry::from_str(super::DEVICE_TYPES).unwrap();
    }
}
