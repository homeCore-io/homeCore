//! Settings › Devices over MQTT, through the API: the profiles folder seen,
//! the catalog added, and a device's type set from the app and written to
//! the profile, all without a restart.

mod common;

use common::Harness;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tempfile::TempDir;

fn local_only() -> Vec<ipnet::IpNet> {
    vec!["127.0.0.1/32".parse().unwrap()]
}

const PLUGS: &str = "# My plugs.\n[ecosystem]\nname = \"plugs\"\nprefix = \"plug_\"\n\n\
    [[ecosystem.state_topics]]\npattern = \"plugs/{device}/power\"\nattribute = \"on\"\n";

#[tokio::test]
async fn an_admin_sees_the_profiles_adds_the_catalog_and_types_a_device() {
    let tmp = TempDir::new().unwrap();
    let folder = tmp.path().join("profiles");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("plugs.toml"), PLUGS).unwrap();
    std::fs::write(folder.join("broken.toml"), "[ecosystem\n").unwrap();
    let h = Harness::start_with(&tmp, local_only(), Default::default())
        .await
        .unwrap();
    let base = format!("{}/api/v1", h.tcp_base());
    let http = reqwest::Client::new();

    let v: Value = http
        .get(format!("{base}/mqtt"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let profiles = v["profiles"].as_array().unwrap();
    assert_eq!(profiles.len(), 2, "{v}");
    assert!(profiles
        .iter()
        .any(|p| p["name"] == "plugs" && p["error"].is_null()));
    assert!(profiles
        .iter()
        .any(|p| p["file"] == "broken.toml" && p["error"].is_string()));
    assert_eq!(v["catalog"]["present"], false);
    assert_eq!(v["shipped_off"].as_array().unwrap().len(), 4);

    // Without the catalog, a type can't be given.
    let r = http
        .patch(format!("{base}/mqtt/profiles/plugs/devices"))
        .json(&json!({ "changes": [{ "device_id": "plug_kettle", "device_type": "switch" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    let r: Value = http
        .post(format!("{base}/mqtt/catalog"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["added"], true);
    let types: Value = http
        .get(format!("{base}/mqtt/device-types"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let switch = types["types"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "switch")
        .expect("the catalog has switch");
    assert!(switch["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["name"] == "on" && a["writable"] == true));

    // A type the catalog doesn't have changes nothing; one it has is written.
    let r = http
        .patch(format!("{base}/mqtt/profiles/plugs/devices"))
        .json(&json!({ "changes": [{ "device_id": "plug_kettle", "device_type": "teapot" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = http
        .patch(format!("{base}/mqtt/profiles/plugs/devices"))
        .json(&json!({ "changes": [{ "device_id": "plug_kettle", "device_type": "switch", "name": "Kettle" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let text = std::fs::read_to_string(folder.join("plugs.toml")).unwrap();
    assert!(text.starts_with("# My plugs."));
    assert!(text.contains("[ecosystem.devices.plug_kettle]"), "{text}");
    assert!(text.contains("device_type = \"switch\""), "{text}");

    let p: Value = http
        .get(format!("{base}/mqtt/profiles/plugs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(p["profile"]["name"], "plugs");
    let r = http
        .get(format!("{base}/mqtt/profiles/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    h.stop().await;
}

#[tokio::test]
async fn only_an_admin_sees_them() {
    let tmp = TempDir::new().unwrap();
    let h = Harness::start(&tmp).await.unwrap();
    let r = reqwest::get(format!("{}/api/v1/mqtt", h.tcp_base()))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    h.stop().await;
}
