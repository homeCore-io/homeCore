//! Every device type an example profile names is one the example device type
//! catalog has. A type the catalog doesn't know gets its devices no schema,
//! and core says so only in its log.

use hc_topic_map::{
    canonical_device_type_name, loader::load_profiles_from_dir, DeviceTypeRegistry,
};

#[test]
fn every_type_an_example_profile_names_is_in_the_catalog() {
    let dir = "../../config/profiles/examples";
    let catalog = DeviceTypeRegistry::from_file(&format!("{dir}/device-types.toml"))
        .expect("the example catalog loads");
    let profiles = load_profiles_from_dir(dir).expect("the example profiles load");
    let mut named = 0;
    for profile in &profiles {
        let types = profile
            .state_topics
            .iter()
            .filter_map(|t| t.device_type.as_deref())
            .chain(
                profile
                    .devices
                    .values()
                    .filter_map(|d| d.device_type.as_deref()),
            );
        for t in types {
            named += 1;
            let t = canonical_device_type_name(t);
            assert!(
                catalog.get_device_schema(&t).is_some(),
                "{}: device_type {t:?} is not in device-types.toml",
                profile.name
            );
        }
    }
    assert!(named > 0, "the examples name some types");
}
