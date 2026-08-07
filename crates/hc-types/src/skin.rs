//! Skins as data.
//!
//! Step 3 of `clients/hc-web/theme-editor-plan.md`. A skin is currently ~80
//! lines of Dart `const` compiled into hc-web, so changing one is a rebuild and
//! a redeploy — which is why nobody can try a skin on the wall panel and adjust
//! it. This is where a skin lives instead.
//!
//! **Core stores seeds, not resolved tokens.** hc-web derives 74 tokens from
//! ~26 seeds (`lib/design/skin_seeds.dart`), and storing the *output* would
//! freeze every skin at the moment it was written: improving a derivation rule
//! later would then improve nothing already saved. Storing the input means a
//! better rule reaches every skin in the house.
//!
//! **Core does not judge a skin.** It checks structure — known keys, parseable
//! colours, numbers in range — and nothing else. Whether `active` is legible on
//! a card is a contrast measurement that lives with the derivation, in
//! `skin_validator.dart`, because that is where the rule is and duplicating it
//! here would give the two a chance to disagree. A skin core accepts can still
//! be one the client warns about.
//!
//! **The built-in four are not here.** They stay compiled into the client and
//! are the floor: a house should never be one bad row away from an unstyled
//! app. [`Skin::base`] names which of them a data skin was forked from, so a
//! skin that fails to load has somewhere to fall back to.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A user-defined skin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skin {
    pub id: String,
    pub name: String,

    /// The built-in this was forked from — `midnight`, `ambient_glass`,
    /// `control_room` or `soft_home`. Not validated against a list here: the
    /// client owns which built-ins exist, and a core that policed the set would
    /// have to be redeployed to add one.
    pub base: String,

    pub seeds: SkinSeeds,

    /// Individual token overrides, `"accent.warn" -> "#C8761F"`.
    ///
    /// Deliberately a free-form map rather than a struct. The seed set is a
    /// considered shape and is typed; overrides are the escape hatch for the
    /// other ~48 derived values, and a client that learns a new one must not
    /// have it silently dropped by an older core. `hc_types` structs have no
    /// `deny_unknown_fields`, so an unknown *typed* field vanishes without
    /// error — a map cannot lose anything.
    #[serde(default)]
    pub overrides: BTreeMap<String, String>,
}

/// The decisions a skin is made of. Mirrors `SkinSeeds` in
/// `clients/hc-web/lib/design/skin_seeds.dart`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkinSeeds {
    pub brightness: Brightness,

    // The palette. Chosen, not computed — see the client's derivation for why
    // no formula reproduces these.
    pub ground: String,
    pub raised: String,
    pub sunken: String,
    pub overlay: String,
    pub ink: String,
    pub ink_muted: String,
    pub accent: String,
    pub on_accent: String,
    pub active: String,
    pub inactive: String,
    pub success: String,
    pub warn: String,
    pub danger: String,
    pub offline: String,
    pub hairline: String,

    /// The focus ring. Absent takes `accent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,

    /// xs, sm, md, lg. Four values rather than a ratio: measured against the
    /// four shipped skins, no single ratio fits.
    pub corners: [f64; 4],

    pub space_unit: f64,
    pub type_scale: f64,
    pub glow_strength: f64,
    pub glow_radius: f64,

    pub density: Density,
    pub motion: Motion,

    #[serde(default)]
    pub glass: Glass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Brightness {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    Compact,
    Comfortable,
    Wall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Motion {
    Crisp,
    Standard,
    Calm,
}

/// Glass is two decisions, not one: a tint without a blur is what a light skin
/// wants, because a light ground scatters without needing to soften.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Glass {
    #[default]
    None,
    Tinted,
    Frosted,
}

impl Skin {
    /// Structural validation, and only structural.
    ///
    /// Returns the first problem in a form a person can act on. It says nothing
    /// about whether the skin is *good* — that is the client's measurement, and
    /// a second copy of it here would eventually disagree with the first.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("skin id cannot be empty".into());
        }
        if self.name.trim().is_empty() {
            return Err("skin name cannot be empty".into());
        }
        if self.base.trim().is_empty() {
            return Err("skin must name the built-in it is based on".into());
        }
        self.seeds.validate()?;
        for (key, value) in &self.overrides {
            if key.trim().is_empty() {
                return Err("override key cannot be empty".into());
            }
            // Overrides are token paths, and every overridable token is a
            // colour today. A number would need a different representation and
            // is worth rejecting until it exists rather than storing something
            // no client can read.
            parse_hex(value).map_err(|e| format!("override '{key}': {e}"))?;
        }
        Ok(())
    }
}

impl SkinSeeds {
    pub fn validate(&self) -> Result<(), String> {
        for (field, value) in [
            ("ground", &self.ground),
            ("raised", &self.raised),
            ("sunken", &self.sunken),
            ("overlay", &self.overlay),
            ("ink", &self.ink),
            ("ink_muted", &self.ink_muted),
            ("accent", &self.accent),
            ("on_accent", &self.on_accent),
            ("active", &self.active),
            ("inactive", &self.inactive),
            ("success", &self.success),
            ("warn", &self.warn),
            ("danger", &self.danger),
            ("offline", &self.offline),
            ("hairline", &self.hairline),
        ] {
            parse_hex(value).map_err(|e| format!("{field}: {e}"))?;
        }
        if let Some(focus) = &self.focus {
            parse_hex(focus).map_err(|e| format!("focus: {e}"))?;
        }

        for (i, corner) in self.corners.iter().enumerate() {
            if !corner.is_finite() || *corner < 0.0 || *corner > 200.0 {
                return Err(format!(
                    "corners[{i}] must be between 0 and 200, got {corner}"
                ));
            }
        }
        // Ascending, because xs/sm/md/lg is a scale and a scale that goes
        // backwards is a typo rather than a style.
        for pair in self.corners.windows(2) {
            if pair[1] < pair[0] {
                return Err(format!(
                    "corners must ascend: {:?} is out of order",
                    self.corners
                ));
            }
        }

        range("space_unit", self.space_unit, 1.0, 32.0)?;
        range("type_scale", self.type_scale, 0.5, 2.0)?;
        range("glow_strength", self.glow_strength, 0.0, 1.0)?;
        range("glow_radius", self.glow_radius, 0.0, 200.0)?;

        // A skin that says "no bloom" and then names a radius is describing two
        // different intentions; the client would honour the strength and ignore
        // the radius, so the disagreement would be invisible.
        if self.glow_strength == 0.0 && self.glow_radius != 0.0 {
            return Err(format!(
                "glow_strength is 0 but glow_radius is {} — a skin with no \
                 bloom has no radius",
                self.glow_radius
            ));
        }
        Ok(())
    }
}

fn range(field: &str, value: f64, lo: f64, hi: f64) -> Result<(), String> {
    if !value.is_finite() || value < lo || value > hi {
        return Err(format!(
            "{field} must be between {lo} and {hi}, got {value}"
        ));
    }
    Ok(())
}

/// `#RRGGBB` or `#AARRGGBB`, the two forms a client can render.
fn parse_hex(value: &str) -> Result<u32, String> {
    let hex = value
        .strip_prefix('#')
        .ok_or_else(|| format!("'{value}' is not a colour — expected #RRGGBB"))?;
    if hex.len() != 6 && hex.len() != 8 {
        return Err(format!(
            "'{value}' is not a colour — expected #RRGGBB or #AARRGGBB"
        ));
    }
    u32::from_str_radix(hex, 16).map_err(|_| format!("'{value}' is not hexadecimal"))
}
