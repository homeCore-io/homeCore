//! The names devices report and take: one name, one meaning, one unit.
//!
//! Every plugin publishes and accepts these names in these units, converting
//! from its device's own scale where the device meets the plugin. Apps,
//! automations, scripts and the bus see only this. An idea only one plugin has
//! (a WLED preset, a Hue effect) stays that plugin's own — under one spelling,
//! and never under a name here with another meaning.
//!
//! **Why a table.** The common names held for the basics and split everywhere
//! else: by October 2026 `brightness` meant 0–255, 0–254, 0–100 or 0–99
//! depending on the plugin, a thermostat's target had five names, and the
//! catalog declared `color_temp` in mireds while [`AttributeKind::ColorTemp`]
//! said kelvin. Nothing checked any of it. This is the one place it is written
//! down, and what the check and the automation rewrite read.
//!
//! # The rules
//!
//! 1. **Use a name here when the idea matches**, in its unit and range. A
//!    plugin's own idea gets its own name, never one of these with another
//!    meaning.
//! 2. **Convert at the edge.** The plugin turns its device's scale into this
//!    one when publishing, and back when commanding. Temperatures are °C, rain
//!    mm (named `…_mm`), pressure hPa, wind m/s — whatever the device or the person prefers:
//!    showing °F is the app's job, from the person's setting.
//! 3. **Units in names.** A number names its unit as a suffix (`_pct`, `_ms`,
//!    `_secs`, `_w`, `_kwh`) except the terms here without one, whose unit this
//!    table defines (`temperature`, `humidity`, `illuminance`, `volume`,
//!    `position`, `color_temp`, …). Fades are `_ms`; timers keep `_secs`.
//! 4. **Two command shapes.** Write a writable attribute — `{"on": true}` — or
//!    call a declared action — `{"action": "start", "duration_secs": 600}`.
//!    Nothing else (`{"command": …}` is retired).
//! 5. **`{"on": true}` restores the last level** where the device can; `0` %
//!    brightness means off; a light that is on never reports `0` %.

use crate::schema::AttributeKind;

/// How an old spelling's value becomes this term's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Convert {
    /// Same value, new name.
    Same,
    /// Multiply: seconds to milliseconds is `Times(1000.0)`.
    Times(f64),
    /// Mireds to kelvin, and back: `1_000_000 / v`.
    Reciprocal,
    /// The value's scale differed between plugins (`brightness` meant 0–255,
    /// 0–99 or 0–100), so it cannot be converted without knowing which.
    Ambiguous,
}

/// A spelling a term replaces.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Replaced {
    pub name: &'static str,
    /// The device types it meant this on; empty for any. `mode` is a
    /// thermostat's HVAC mode on a thermostat and something else on a mode.
    pub on_types: &'static [&'static str],
    pub convert: Convert,
}

/// One shared name.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Term {
    pub name: &'static str,
    pub kind: AttributeKind,
    /// As the schema's `unit` says it; `None` for a bool, an enum or a colour.
    pub unit: Option<&'static str>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// For an enum, the values it takes.
    pub options: &'static [&'static str],
    /// What it means, in one line.
    pub means: &'static str,
    pub replaces: &'static [Replaced],
}

const fn term(name: &'static str, kind: AttributeKind, means: &'static str) -> Term {
    Term {
        name,
        kind,
        unit: None,
        min: None,
        max: None,
        options: &[],
        means,
        replaces: &[],
    }
}

impl Term {
    const fn unit(mut self, unit: &'static str) -> Self {
        self.unit = Some(unit);
        self
    }
    const fn range(mut self, min: f64, max: f64) -> Self {
        self.min = Some(min);
        self.max = Some(max);
        self
    }
    const fn options(mut self, options: &'static [&'static str]) -> Self {
        self.options = options;
        self
    }
    const fn replaces(mut self, replaces: &'static [Replaced]) -> Self {
        self.replaces = replaces;
        self
    }
}

const fn was(name: &'static str, convert: Convert) -> Replaced {
    Replaced {
        name,
        on_types: &[],
        convert,
    }
}

const fn was_on(
    name: &'static str,
    on_types: &'static [&'static str],
    convert: Convert,
) -> Replaced {
    Replaced {
        name,
        on_types,
        convert,
    }
}

use AttributeKind::{Bool, ColorRgb, ColorTemp, ColorXy, Enum, Float, Integer, String as Text};
use Convert::{Ambiguous, Reciprocal, Same, Times};

const THERMOSTAT: &[&str] = &["thermostat"];
const HVAC_MODES: &[&str] = &["off", "heat", "cool", "auto", "fan_only", "dry"];
const HVAC_ACTIONS: &[&str] = &["idle", "heating", "cooling", "fan", "drying", "off"];
const MEDIA: &[&str] = &["media_player"];
const PLAYBACK: &[&str] = &["playing", "paused", "stopped", "idle", "buffering"];

/// Every shared name, in the order a reader meets them.
pub const TERMS: &[Term] = &[
    // ── Power and light ─────────────────────────────────────────────────
    term(
        "on",
        Bool,
        "On or off. `true` with no level restores the last one.",
    ),
    term(
        "brightness_pct",
        Float,
        "Light level. 0 is off; a light that is on never reports 0.",
    )
    .unit("%")
    .range(0.0, 100.0)
    .replaces(&[was("brightness", Ambiguous), was("level", Ambiguous)]),
    term("color_temp", ColorTemp, "White warmth in kelvin.")
        .unit("K")
        .replaces(&[
            was("color_temp_mirek", Reciprocal),
            was("color_temp_k", Same),
        ]),
    term(
        "color_xy",
        ColorXy,
        "Colour as a CIE 1931 xy point, `{x, y}`.",
    ),
    term("color_rgb", ColorRgb, "Colour as sRGB, `{r, g, b}` 0–255."),
    term(
        "transition_ms",
        Integer,
        "Fade time for the change it is sent with.",
    )
    .unit("ms")
    .range(0.0, f64::MAX)
    .replaces(&[
        was("transition", Same),
        was("duration_ms", Same),
        was("fade_secs", Times(1000.0)),
    ]),
    // ── Covers, locks, fans ─────────────────────────────────────────────
    term("position", Float, "How open a cover is; 100 is fully open.")
        .unit("%")
        .range(0.0, 100.0),
    term("tilt_pct", Float, "Slat tilt; 100 is fully open.")
        .unit("%")
        .range(0.0, 100.0),
    term("locked", Bool, "Whether the lock is locked."),
    term("speed_pct", Float, "Fan speed; 0 is off.")
        .unit("%")
        .range(0.0, 100.0),
    // ── Sensors ─────────────────────────────────────────────────────────
    term("open", Bool, "A door, window or contact: `true` when open.")
        .replaces(&[was("contact_open", Same), was("door_open", Same)]),
    term("motion", Bool, "Motion seen now."),
    term("occupied", Bool, "Someone is in the area.").replaces(&[was("occupancy", Same)]),
    term("water_detected", Bool, "A leak sensor is wet."),
    term(
        "tampered",
        Bool,
        "The device's cover or mounting was disturbed.",
    )
    .replaces(&[was("tamper", Same)]),
    term("temperature", Float, "The temperature here, in °C.")
        .unit("°C")
        .replaces(&[was_on("current_temperature", THERMOSTAT, Same)]),
    term("humidity", Float, "Relative humidity.")
        .unit("%")
        .range(0.0, 100.0)
        .replaces(&[was("humidity_pct", Same)]),
    term("illuminance", Float, "Light level in lux.")
        .unit("lux")
        .range(0.0, f64::MAX)
        .replaces(&[was("illuminance_lux", Same)]),
    term("pressure", Float, "Air pressure.").unit("hPa"),
    term("wind_speed", Float, "Wind speed.").unit("m/s"),
    term("co2", Float, "Carbon dioxide.").unit("ppm"),
    // ── Climate ─────────────────────────────────────────────────────────
    term("setpoint", Float, "The one target temperature, in °C.").unit("°C"),
    term("heat_setpoint", Float, "Heat below this, in °C.")
        .unit("°C")
        .replaces(&[
            was_on("target_temp", THERMOSTAT, Same),
            was_on("target_temp_heat", THERMOSTAT, Same),
        ]),
    term("cool_setpoint", Float, "Cool above this, in °C.")
        .unit("°C")
        .replaces(&[was_on("target_temp_cool", THERMOSTAT, Same)]),
    term("hvac_mode", Enum, "What the thermostat is set to do.")
        .options(HVAC_MODES)
        .replaces(&[was_on("mode", THERMOSTAT, Same)]),
    term("hvac_action", Enum, "What it is doing now.")
        .options(HVAC_ACTIONS)
        .replaces(&[
            was_on("call_for", THERMOSTAT, Same),
            was_on("state", THERMOSTAT, Same),
        ]),
    term("fan_mode", Text, "The thermostat's fan setting."),
    // ── Energy and batteries ────────────────────────────────────────────
    term("power_w", Float, "Power drawn now.").unit("W"),
    term("energy_kwh", Float, "Energy used, counting up.").unit("kWh"),
    term("voltage", Float, "Supply voltage.").unit("V"),
    term("current_a", Float, "Current drawn now.").unit("A"),
    term("battery_pct", Float, "Battery left.")
        .unit("%")
        .range(0.0, 100.0)
        .replaces(&[was("battery", Same), was("battery_level", Same)]),
    term("battery_low", Bool, "The battery needs replacing soon."),
    // ── Media ───────────────────────────────────────────────────────────
    term("volume", Float, "Volume.").unit("%").range(0.0, 100.0),
    term("muted", Bool, "Sound is muted."),
    term("state", Enum, "A media player's playback.").options(PLAYBACK),
    term("source", Text, "The input or app playing."),
    term("media_title", Text, "What is playing.").replaces(&[was_on("title", MEDIA, Same)]),
    term("media_artist", Text, "Its artist.").replaces(&[was_on("artist", MEDIA, Same)]),
    term("media_album", Text, "Its album.").replaces(&[was_on("album", MEDIA, Same)]),
    term("media_position", Integer, "How far into it.")
        .unit("s")
        .replaces(&[was_on("position_secs", MEDIA, Same)]),
    term("media_duration", Integer, "How long it is.")
        .unit("s")
        .replaces(&[was_on("duration_secs", MEDIA, Same)]),
    // ── Scenes ──────────────────────────────────────────────────────────
    term("active", Bool, "A scene is what its devices show now."),
];

/// The shared term called `name`.
pub fn term_named(name: &str) -> Option<&'static Term> {
    TERMS.iter().find(|t| t.name == name)
}

/// The term an old spelling became on a device of `device_type`, and how its
/// value converts — `None` if `name` was never replaced there.
pub fn replacement(name: &str, device_type: Option<&str>) -> Option<(&'static Term, Convert)> {
    TERMS.iter().find_map(|t| {
        t.replaces
            .iter()
            .find(|r| {
                r.name == name
                    && (r.on_types.is_empty()
                        || device_type.is_some_and(|d| r.on_types.contains(&d)))
            })
            .map(|r| (t, r.convert))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn each_name_means_one_thing() {
        let mut seen = HashSet::new();
        for t in TERMS {
            assert!(seen.insert(t.name), "{} is listed twice", t.name);
        }
        // An old spelling may not be a current name, or the rewrite would
        // turn a correct attribute into another one.
        for t in TERMS {
            for r in t.replaces {
                if let Some(other) = term_named(r.name) {
                    assert!(
                        !r.on_types.is_empty(),
                        "{} replaces {}, which is itself a term ({})",
                        t.name,
                        r.name,
                        other.means
                    );
                }
            }
        }
    }

    #[test]
    fn a_number_names_its_unit_or_is_one_whose_unit_is_set_here() {
        const UNIT_SET_HERE: &[&str] = &[
            "temperature",
            "humidity",
            "illuminance",
            "pressure",
            "wind_speed",
            "co2",
            "voltage",
            "volume",
            "position",
            "color_temp",
            "setpoint",
            "heat_setpoint",
            "cool_setpoint",
            "media_position",
            "media_duration",
        ];
        const SUFFIXES: &[&str] = &["_pct", "_ms", "_secs", "_w", "_kwh", "_a", "_mm"];
        for t in TERMS.iter().filter(|t| matches!(t.kind, Float | Integer)) {
            assert!(
                SUFFIXES.iter().any(|s| t.name.ends_with(s)) || UNIT_SET_HERE.contains(&t.name),
                "{} is a number with no unit in its name",
                t.name
            );
            assert!(t.unit.is_some(), "{} is a number with no unit", t.name);
        }
    }

    #[test]
    fn temperatures_are_celsius() {
        for t in TERMS
            .iter()
            .filter(|t| t.unit.is_some_and(|u| u.starts_with('°')))
        {
            assert_eq!(t.unit, Some("°C"), "{}", t.name);
        }
    }

    #[test]
    fn old_spellings_find_their_term() {
        assert_eq!(
            replacement("humidity_pct", None).unwrap().0.name,
            "humidity"
        );
        assert_eq!(
            replacement("fade_secs", None).unwrap().1,
            Convert::Times(1000.0)
        );
        assert_eq!(
            replacement("brightness", None).unwrap().1,
            Convert::Ambiguous
        );
        // A thermostat's `mode` is its HVAC mode; a mode device's is not.
        assert_eq!(
            replacement("mode", Some("thermostat")).unwrap().0.name,
            "hvac_mode"
        );
        assert!(replacement("mode", Some("mode")).is_none());
        assert!(replacement("on", None).is_none());
    }
}
