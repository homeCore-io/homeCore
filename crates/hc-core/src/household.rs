//! The household's own settings that change how core words things.
//!
//! **The temperature unit.** Devices, rules and history keep °C; this is only
//! what server-side text — a notification, an alert's message — shows a
//! temperature in. Set from `[location].temperature_unit` at startup and
//! changed through `PUT /system/temperature-unit`. A person's own unit in the
//! app is the app's business and still wins there.

use hc_types::temperature::TemperatureUnit;
use std::sync::RwLock;

// A `RwLock` rather than a `OnceLock` so the API can change it while running,
// the same way `hc_time` keeps the configured zone.
static TEMPERATURE_UNIT: RwLock<Option<TemperatureUnit>> = RwLock::new(None);

/// The household's temperature unit; °C until set.
pub fn temperature_unit() -> TemperatureUnit {
    chosen_temperature_unit().unwrap_or_default()
}

/// The unit the household chose, or `None` if nobody has. An app reads
/// this to know whether to fall back to the browser's language instead.
pub fn chosen_temperature_unit() -> Option<TemperatureUnit> {
    TEMPERATURE_UNIT.read().map(|g| *g).unwrap_or_default()
}

/// Set the household's unit, or `None` to go back to unset.
pub fn set_temperature_unit(unit: Option<TemperatureUnit>) {
    if let Ok(mut g) = TEMPERATURE_UNIT.write() {
        *g = unit;
    }
}

/// A °C value as the household reads it: "21.5 °C" or "70.7 °F".
pub fn format_temperature(celsius: f64) -> String {
    temperature_unit().format(celsius)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not changed here: the unit is process-wide, and the alert tests that
    // run beside this one read it.
    #[test]
    fn text_is_celsius_until_the_household_says_otherwise() {
        assert_eq!(temperature_unit(), TemperatureUnit::Celsius);
        assert_eq!(chosen_temperature_unit(), None);
        assert_eq!(format_temperature(20.0), "20 °C");
        assert_eq!(TemperatureUnit::Fahrenheit.format(20.0), "68 °F");
    }
}
