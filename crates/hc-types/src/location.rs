//! The house's position, as homeCore tells its plugins.
//!
//! Core publishes the `[location]` coordinates from `homecore.toml` as a
//! retained message on [`TOPIC`], beside the time zone on
//! `homecore/system/tz`, so a plugin that needs to know where the house is
//! (weather for "home") reads the same place sunrise and sunset come from,
//! instead of asking the operator to type it twice.

use serde::{Deserialize, Serialize};

/// The retained topic the house's coordinates are published on.
pub const TOPIC: &str = "homecore/system/location";

/// Where the house is, in decimal degrees.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HouseLocation {
    pub latitude: f64,
    pub longitude: f64,
}

impl HouseLocation {
    /// The message published on [`TOPIC`].
    pub fn to_payload(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// A payload read back, if it is a position on the earth.
    pub fn from_payload(payload: &[u8]) -> Option<Self> {
        let l: Self = serde_json::from_slice(payload).ok()?;
        let on_earth = l.latitude.is_finite()
            && l.longitude.is_finite()
            && (-90.0..=90.0).contains(&l.latitude)
            && (-180.0..=180.0).contains(&l.longitude);
        on_earth.then_some(l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_round_trips() {
        let here = HouseLocation {
            latitude: 35.96,
            longitude: -83.92,
        };
        assert_eq!(HouseLocation::from_payload(&here.to_payload()), Some(here));
    }

    #[test]
    fn nonsense_is_not_a_position() {
        assert_eq!(HouseLocation::from_payload(b"not json"), None);
        assert_eq!(
            HouseLocation::from_payload(br#"{"latitude": 95, "longitude": 0}"#),
            None
        );
        assert_eq!(
            HouseLocation::from_payload(br#"{"latitude": 1, "longitude": -181}"#),
            None
        );
    }
}
