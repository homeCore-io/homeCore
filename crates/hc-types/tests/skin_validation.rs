//! What core will and will not accept as a skin.
//!
//! The line matters: core checks **structure** — parseable colours, numbers in
//! range, a scale that ascends — and nothing about whether the skin is legible.
//! That measurement lives with the derivation in `skin_validator.dart`, because
//! that is where the rule is, and a second copy here would eventually disagree
//! with the first. A skin core accepts can still be one the client warns about,
//! and that is the intended division rather than a gap.

use hc_types::skin::{Brightness, Density, Glass, Motion, Skin, SkinSeeds};

fn seeds() -> SkinSeeds {
    SkinSeeds {
        brightness: Brightness::Dark,
        ground: "#0B0E13".into(),
        raised: "#141922".into(),
        sunken: "#0D1116".into(),
        overlay: "#1A202A".into(),
        ink: "#E9EDF2".into(),
        ink_muted: "#8B95A4".into(),
        accent: "#7CC4FF".into(),
        on_accent: "#06131F".into(),
        active: "#FFB661".into(),
        inactive: "#2A313B".into(),
        success: "#6FD1A6".into(),
        warn: "#FFC978".into(),
        danger: "#FF7B72".into(),
        offline: "#AA737A".into(),
        hairline: "#262D38".into(),
        focus: None,
        corners: [4.0, 8.0, 14.0, 22.0],
        space_unit: 8.0,
        type_scale: 1.0,
        glow_strength: 1.0,
        glow_radius: 34.0,
        density: Density::Comfortable,
        motion: Motion::Standard,
        glass: Glass::None,
    }
}

fn skin() -> Skin {
    Skin {
        id: "hallway".into(),
        name: "Hallway".into(),
        base: "midnight".into(),
        seeds: seeds(),
        overrides: Default::default(),
    }
}

#[test]
fn midnights_own_values_are_acceptable() {
    // The seeds above are Midnight's, so the shipped design has to pass the
    // structural bar it defines.
    assert_eq!(skin().validate(), Ok(()));
}

#[test]
fn a_colour_that_is_not_a_colour_is_named() {
    let mut s = skin();
    s.seeds.warn = "tomato".into();
    let err = s.validate().unwrap_err();
    assert!(err.contains("warn"), "{err}");
    assert!(err.contains("tomato"), "{err}");
}

#[test]
fn both_hex_lengths_are_accepted() {
    // #AARRGGBB is how a translucent hairline is written, and Ambient Glass
    // ships one.
    let mut s = skin();
    s.seeds.hairline = "#1FFFFFFF".into();
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn a_missing_hash_is_rejected_rather_than_guessed() {
    let mut s = skin();
    s.seeds.ground = "0B0E13".into();
    assert!(s.validate().unwrap_err().contains("#RRGGBB"));
}

#[test]
fn a_corner_scale_that_runs_backwards_is_a_typo() {
    let mut s = skin();
    s.seeds.corners = [22.0, 14.0, 8.0, 4.0];
    assert!(s.validate().unwrap_err().contains("ascend"));
}

#[test]
fn numbers_out_of_range_are_named_with_their_bounds() {
    for (label, mut s) in [
        ("space_unit", {
            let mut s = skin();
            s.seeds.space_unit = 0.0;
            s
        }),
        ("type_scale", {
            let mut s = skin();
            s.seeds.type_scale = 9.0;
            s
        }),
        ("glow_strength", {
            let mut s = skin();
            s.seeds.glow_strength = 1.5;
            s
        }),
    ] {
        let err = s.validate().unwrap_err();
        assert!(err.contains(label), "expected {label} in: {err}");
        assert!(err.contains("between"), "{err}");
        s.seeds.space_unit = 8.0;
    }
}

#[test]
fn a_skin_with_no_bloom_may_not_also_claim_a_radius() {
    // The two would disagree silently: the client honours strength and ignores
    // the radius, so the contradiction would never surface.
    let mut s = skin();
    s.seeds.glow_strength = 0.0;
    s.seeds.glow_radius = 30.0;
    let err = s.validate().unwrap_err();
    assert!(err.contains("no bloom"), "{err}");
}

#[test]
fn no_bloom_and_no_radius_is_control_rooms_shape_and_is_fine() {
    let mut s = skin();
    s.seeds.glow_strength = 0.0;
    s.seeds.glow_radius = 0.0;
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn an_override_that_is_not_a_colour_is_rejected_with_its_key() {
    let mut s = skin();
    s.overrides
        .insert("accent.warn".into(), "burnt sienna".into());
    let err = s.validate().unwrap_err();
    assert!(err.contains("accent.warn"), "{err}");
}

#[test]
fn identity_fields_cannot_be_blank() {
    for mutate in [
        (|s: &mut Skin| s.id = "  ".into()) as fn(&mut Skin),
        |s: &mut Skin| s.name = "".into(),
        |s: &mut Skin| s.base = "".into(),
    ] {
        let mut s = skin();
        mutate(&mut s);
        assert!(s.validate().is_err());
    }
}

#[test]
fn core_does_not_judge_whether_the_skin_is_legible() {
    // Deliberately illegible: ink almost exactly its own ground. The client's
    // validator blocks this; core stores it. Duplicating the contrast maths
    // here is what this test exists to prevent, because the two copies would
    // drift and the disagreement would be invisible.
    let mut s = skin();
    s.seeds.ground = "#0B0E13".into();
    s.seeds.ink = "#0C0F14".into();
    assert_eq!(
        s.validate(),
        Ok(()),
        "core is structural only — legibility belongs with the derivation"
    );
}

#[test]
fn the_wire_format_is_snake_case_and_round_trips() {
    let json = serde_json::to_string(&skin()).unwrap();
    assert!(json.contains("\"ink_muted\""), "{json}");
    assert!(json.contains("\"glow_strength\""), "{json}");
    assert!(json.contains("\"brightness\":\"dark\""), "{json}");
    let back: Skin = serde_json::from_str(&json).unwrap();
    assert_eq!(back, skin());
}

#[test]
fn glass_defaults_to_none_when_absent() {
    // An older client that predates the glass split must still round-trip.
    let json = serde_json::to_string(&skin()).unwrap();
    let stripped = json.replace(",\"glass\":\"none\"", "");
    let back: Skin = serde_json::from_str(&stripped).unwrap();
    assert_eq!(back.seeds.glass, Glass::None);
}
