//! The move out of `main.rs` has to be a *move*, not a rewrite.
//!
//! `homecore.full.toml` is the sandbox house's own config with paths
//! generalised and any secret-shaped value redacted — 186 lines exercising
//! every section the binary knows how to parse, including the nested
//! `[logging.*]` tables and an array-of-tables `[[plugins]]`. If a section ever
//! stops deserialising, this fails here rather than at somebody's next boot.

use hc_config::AppConfig;
use std::path::Path;

fn load() -> AppConfig {
    let raw = include_str!("homecore.full.toml");
    toml::from_str::<AppConfig>(raw).expect("the real config still parses")
}

#[test]
fn every_section_of_a_real_config_deserialises() {
    let c = load();

    // A spread of sections, each read from the fixture rather than a default:
    // if a struct silently stopped matching its TOML the values would fall back
    // to defaults, and a test that only checked "it parsed" would pass.
    assert_eq!(c.server.host, "0.0.0.0");
    assert_eq!(c.server.port, 8080);
    assert_eq!(c.broker.port, 1883);
    assert_eq!(c.auth.token_expiry_hours, 24);
    assert_eq!(c.location.timezone.as_deref(), Some("America/New_York"));
    assert!(!c.plugins.is_empty(), "[[plugins]] is an array of tables");
    assert!(
        !c.logging.level.is_empty(),
        "[logging] lives in hc-logging and still has to deserialise from here"
    );
}

#[test]
fn relative_paths_resolve_against_the_base_dir() {
    // The `resolve_paths` walk moved with the structs; it is the part most
    // likely to have been quietly dropped, because nothing fails to compile
    // when a section stops resolving — the paths just come out relative and
    // the process writes its database somewhere surprising.
    let mut c = load();
    c.resolve_paths(Path::new("/srv/homecore"));

    assert!(
        Path::new(&c.storage.state_db_path).is_absolute(),
        "state_db_path resolved: {}",
        c.storage.state_db_path
    );
    assert!(Path::new(&c.storage.history_db_path).is_absolute());
    assert!(Path::new(&c.rules.dir).is_absolute());
    assert!(Path::new(&c.profiles.dir).is_absolute());
}

#[test]
fn an_empty_config_is_all_defaults() {
    // Every section is `#[serde(default)]`, which is what lets a freshly
    // installed house boot on a nearly empty file.
    let c = toml::from_str::<AppConfig>("").expect("empty config parses");
    assert_eq!(c.server.port, 8080);
    assert!(c.plugins.is_empty());
}

/// The schema is what a descriptor of these sections will be checked against,
/// so it has to actually reach the nested tables — not just the top level.
#[cfg(feature = "schema")]
#[test]
fn the_schema_covers_the_nested_sections() {
    let schema = serde_json::to_value(schemars::schema_for!(AppConfig)).unwrap();
    let defs = schema
        .get("definitions")
        .and_then(|d| d.as_object())
        .expect("schemars emits definitions for the section structs");

    for section in [
        "ServerSection",
        "BrokerSection",
        "AuthSection",
        "AdminUdsSection",
        "StorageSection",
        "BatterySection",
        "LoggingConfig",
        "InfluxConfig",
        "PluginEntry",
    ] {
        assert!(defs.contains_key(section), "schema is missing {section}");
    }

    // A leaf field, to prove the walk descends rather than stopping at a $ref.
    let port = defs["ServerSection"]["properties"]["port"].clone();
    assert!(!port.is_null(), "server.port should be described");
}

/// The coverage rule has to work against *this* schema, not just a plugin's.
///
/// This is the check Phase 4's descriptor will be held to, exercised now while
/// the descriptor is still empty: with nothing described, every leaf of
/// homecore.toml should be reported missing. If the walk failed to descend into
/// the nested `[logging.*]` tables the list would be suspiciously short, and a
/// descriptor written later would pass its coverage test while omitting half
/// the file.
#[cfg(feature = "schema")]
#[test]
fn the_coverage_rule_reads_this_schema() {
    use hc_types::config_descriptor::{missing_schema_coverage, Descriptor};

    let schema = serde_json::to_value(schemars::schema_for!(AppConfig)).unwrap();
    let nothing_described = Descriptor::new("homecore").build();

    let missing = missing_schema_coverage(&schema, &nothing_described, &[]);

    assert!(
        missing.iter().any(|k| k == "server.port"),
        "a top-level leaf should be reported: {missing:?}"
    );
    assert!(
        missing.iter().any(|k| k.starts_with("logging.")),
        "the walk must descend into the nested logging tables: {missing:?}"
    );
    assert!(
        missing.iter().any(|k| k.starts_with("auth.admin_uds.")),
        "and into a struct nested two deep: {missing:?}"
    );
}
