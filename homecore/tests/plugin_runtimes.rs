//! Plugin runtimes over real HTTP: add one, connect with its key, reset its
//! login, remove it.
//!
//! The unit tests cover the key, the broker address and the login check; these
//! cover what only a server can get wrong: status codes, what a response
//! carries, and that a reset or a removal really ends the old key.

mod common;

use common::Harness;
use hc_api_types::plugin_runtimes::{ConnectionKey, HelloRequest, IssuedKey, RuntimeCapabilities};
use reqwest::StatusCode;
use tempfile::TempDir;

/// Admin calls come from 127.0.0.1, which the harness whitelists.
fn local_only() -> Vec<ipnet::IpNet> {
    vec!["127.0.0.1/32".parse().unwrap()]
}

fn hello_body(kind: &str) -> HelloRequest {
    HelloRequest {
        capabilities: RuntimeCapabilities {
            kind: kind.into(),
            abi: "manylinux_2_28".into(),
            arch: "x86_64".into(),
        },
        host_version: "0.2.0".into(),
        hostname: "pyhost".into(),
        network_mode: "bridge".into(),
        page_port: Some(8790),
        page_url: None,
    }
}

async fn hello(base: &str, key: &ConnectionKey, secret: &str, kind: &str) -> StatusCode {
    reqwest::Client::new()
        .post(format!(
            "{base}/api/v1/plugin-runtimes/{}/hello",
            key.runtime_id
        ))
        .bearer_auth(secret)
        .json(&hello_body(kind))
        .send()
        .await
        .unwrap()
        .status()
}

async fn add(base: &str, body: serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/api/v1/plugin-runtimes"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_runtime_is_added_connects_and_is_reset_and_removed() {
    let tmp = TempDir::new().unwrap();
    let h = Harness::start_with(&tmp, local_only(), Default::default())
        .await
        .unwrap();
    let base = h.tcp_base();
    let http = reqwest::Client::new();

    // Adding it makes the login and hands the key back, once.
    let r = add(
        &base,
        serde_json::json!({"name": "Python runtime 1", "kind": "python", "homecore_url": "http://homecore.local:8080"}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let issued: IssuedKey = r.json().await.unwrap();
    let key = ConnectionKey::decode(&issued.key).unwrap();
    assert_eq!(key, issued.details, "the key and Show details must agree");
    assert_eq!(key.url, "http://homecore.local:8080");
    // The harness broker listens on 127.0.0.1, which only works from here, so
    // the key names the host the runtime reaches homeCore at.
    assert_eq!(key.broker_host, "homecore.local");
    assert_eq!(key.kind, "python");
    assert_eq!(key.username, issued.runtime.plugin_id);

    // The list never carries the secret.
    let list = http
        .get(format!("{base}/api/v1/plugin-runtimes"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(list.contains("Python runtime 1"), "{list}");
    assert!(
        !list.contains(&key.secret),
        "the secret leaked into the list"
    );

    // Its key works; a wrong one, or the wrong language, does not.
    assert_eq!(
        hello(&base, &key, &key.secret, "python").await,
        StatusCode::OK
    );
    assert_eq!(
        hello(&base, &key, "hc_sk_wrong", "python").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        hello(&base, &key, &key.secret, "node").await,
        StatusCode::CONFLICT
    );
    // An image that still brings its own Python is told to update, since it
    // would ignore the interpreter each plugin is placed with.
    let mut old_image = hello_body("python");
    old_image.capabilities.abi = "cp312-manylinux_2_28".into();
    let resp = reqwest::Client::new()
        .post(format!(
            "{base}/api/v1/plugin-runtimes/{}/hello",
            key.runtime_id
        ))
        .bearer_auth(&key.secret)
        .json(&old_image)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let said = resp.text().await.unwrap();
    assert!(
        said.contains("Pull the latest hc-runtime-python image"),
        "{said}"
    );
    // Saying hello is what records where it runs.
    let list: serde_json::Value = http
        .get(format!("{base}/api/v1/plugin-runtimes"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["runtimes"][0]["arch"], "x86_64");
    assert_eq!(list["runtimes"][0]["source_ip"], "127.0.0.1");
    assert_eq!(list["runtimes"][0]["page_port"], 8790);

    let placements = |secret: String| {
        let http = http.clone();
        let url = format!(
            "{base}/api/v1/plugin-runtimes/{}/placements",
            key.runtime_id
        );
        async move {
            http.get(url)
                .bearer_auth(secret)
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(placements(key.secret.clone()).await, StatusCode::OK);

    // Reset login: a new key, and the old one stops working at once.
    let r = http
        .post(format!(
            "{base}/api/v1/plugin-runtimes/{}/reset",
            key.runtime_id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let reset: IssuedKey = r.json().await.unwrap();
    assert_ne!(reset.details.secret, key.secret);
    assert_eq!(reset.details.runtime_id, key.runtime_id);
    assert_eq!(
        hello(&base, &key, &key.secret, "python").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(placements(key.secret.clone()).await, StatusCode::NOT_FOUND);
    assert_eq!(
        hello(&base, &reset.details, &reset.details.secret, "python").await,
        StatusCode::OK
    );

    // Remove: gone from the list, and its key with it.
    let r = http
        .delete(format!("{base}/api/v1/plugin-runtimes/{}", key.runtime_id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        hello(&base, &reset.details, &reset.details.secret, "python").await,
        StatusCode::UNAUTHORIZED
    );
    let list: serde_json::Value = http
        .get(format!("{base}/api/v1/plugin-runtimes"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["runtimes"].as_array().unwrap().len(), 0);

    h.stop().await;
}

#[tokio::test]
async fn adding_says_what_is_wrong_with_the_form() {
    let tmp = TempDir::new().unwrap();
    let h = Harness::start_with(&tmp, local_only(), Default::default())
        .await
        .unwrap();
    let base = h.tcp_base();

    for (body, says) in [
        (
            serde_json::json!({"name": " ", "kind": "python", "homecore_url": "http://h:8080"}),
            "name",
        ),
        (
            serde_json::json!({"name": "R", "kind": "python", "homecore_url": "h:8080"}),
            "http",
        ),
        (
            serde_json::json!({"name": "R", "kind": "Node JS", "homecore_url": "http://h:8080"}),
            "lowercase",
        ),
    ] {
        let r = add(&base, body).await;
        assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let text = r.text().await.unwrap();
        assert!(text.contains(says), "{text}");
    }

    // Only an admin adds runtimes.
    let tmp2 = TempDir::new().unwrap();
    let h2 = Harness::start(&tmp2).await.unwrap();
    let r = add(
        &h2.tcp_base(),
        serde_json::json!({"name": "R", "kind": "python", "homecore_url": "http://h:8080"}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    h.stop().await;
    h2.stop().await;
}

#[tokio::test]
async fn the_broker_check_says_a_loopback_broker_is_unreachable() {
    let tmp = TempDir::new().unwrap();
    let h = Harness::start_with(&tmp, local_only(), Default::default())
        .await
        .unwrap();
    let v: serde_json::Value =
        reqwest::get(format!("{}/api/v1/plugin-runtimes/broker", h.tcp_base()))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(v["reachable_from_network"], false, "{v}");
    assert_eq!(v["port"], 1883);
    h.stop().await;
}

#[tokio::test]
async fn a_runtime_can_be_renamed_and_given_a_limit() {
    let tmp = TempDir::new().unwrap();
    let h = Harness::start_with(&tmp, local_only(), Default::default())
        .await
        .unwrap();
    let base = h.tcp_base();
    let http = reqwest::Client::new();
    let issued: IssuedKey = add(
        &base,
        serde_json::json!({"name": "Pi", "kind": "python", "homecore_url": "http://h:8080", "max_plugins": 3}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(issued.runtime.max_plugins, Some(3));

    let url = format!(
        "{base}/api/v1/plugin-runtimes/{}",
        issued.runtime.runtime_id
    );
    let set = |body: serde_json::Value| {
        let http = http.clone();
        let url = url.clone();
        async move {
            http.patch(url)
                .json(&body)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    // Renaming leaves the limit alone.
    let v = set(serde_json::json!({"name": "Garage Pi"})).await;
    assert_eq!(v["name"], "Garage Pi");
    assert_eq!(v["max_plugins"], 3);
    // null clears it.
    let v = set(serde_json::json!({"max_plugins": null})).await;
    assert!(v.get("max_plugins").is_none(), "{v}");

    h.stop().await;
}

/// homeCore names no language: a runtime of any kind is added, and its first
/// check-in records the platform its image provides. After that, an image on
/// another platform is refused, as for any runtime.
#[tokio::test]
async fn a_runtime_of_any_kind_learns_its_platform_from_its_first_hello() {
    let tmp = TempDir::new().unwrap();
    let h = Harness::start_with(&tmp, local_only(), Default::default())
        .await
        .unwrap();
    let base = h.tcp_base();

    let r = add(
        &base,
        serde_json::json!({"name": "Node runtime 1", "kind": "node", "homecore_url": "http://homecore.local:8080"}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let issued: IssuedKey = r.json().await.unwrap();
    let key = ConnectionKey::decode(&issued.key).unwrap();
    assert_eq!(key.kind, "node");
    assert_eq!(key.abi, "", "the platform is not known until it connects");
    assert!(key.username.starts_with("plugin.node-"), "{}", key.username);

    let say = |abi: &'static str| {
        let mut body = hello_body("node");
        body.capabilities.abi = abi.into();
        let (base, id, secret) = (base.clone(), key.runtime_id.clone(), key.secret.clone());
        async move {
            reqwest::Client::new()
                .post(format!("{base}/api/v1/plugin-runtimes/{id}/hello"))
                .bearer_auth(secret)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(say("glibc_2_28").await, StatusCode::OK);
    assert_eq!(say("glibc_2_28").await, StatusCode::OK);
    assert_eq!(say("musl_1_2").await, StatusCode::CONFLICT);

    let kinds: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/api/v1/plugin-runtimes/kinds"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(kinds["kinds"], serde_json::json!(["node"]));

    h.stop().await;
}

/// A plugin placed on a runtime: its config is core's own file, which the
/// runtime is served. GET used to ask the plugin and hand back its broker
/// password in clear (the SDK answers with the file as text, which the JSON
/// redaction had no keys to find in), and PUT went to the plugin's own copy,
/// which the runtime's next check-in replaced, restarting nothing.
#[tokio::test]
async fn a_placed_plugins_config_is_cores_file_redacted_and_written_in_place() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("config/plugins/plugin.matter.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let original = "# set by core\n[homecore]\nplugin_id = \"plugin.matter\"\npassword = \"s3cret-broker-pass\"\n\n[matter]\nlog_level = \"info\"\n";
    std::fs::write(&file, original).unwrap();
    let h = Harness::start_seeded(&tmp, local_only(), Default::default(), |store| {
        store
            .plugin_runtimes()
            .place(&hc_state::plugin_runtime_store::PlacementRecord {
                runtime_id: "rt-00000001".into(),
                plugin_id: "plugin.matter".into(),
                version: "0.2.0".into(),
                artifact_url: "http://example.invalid/a.tar.zst".into(),
                sha256: "0".repeat(64),
                config: original.into(),
                placed_at: chrono::Utc::now(),
                interpreter: None,
            })
            .unwrap();
    })
    .await
    .unwrap();
    let url = format!("{}/api/v1/plugins/plugin.matter/config", h.tcp_base());
    let http = reqwest::Client::new();

    let got: serde_json::Value = http.get(&url).send().await.unwrap().json().await.unwrap();
    assert_eq!(got["format"], "toml", "{got}");
    let raw = got["raw"].as_str().unwrap();
    assert!(
        !raw.contains("s3cret-broker-pass"),
        "the broker password leaked: {raw}"
    );
    assert!(
        raw.contains("__redacted__") && raw.contains("# set by core"),
        "{raw}"
    );

    // Change a setting the way the editor does; the password it never saw stays.
    let res = http
        .put(&url)
        .json(&serde_json::json!({ "patch": { "matter": { "log_level": "debug" } } }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let written = std::fs::read_to_string(&file).unwrap();
    assert!(written.contains("log_level = \"debug\""), "{written}");
    assert!(
        written.contains("password = \"s3cret-broker-pass\""),
        "{written}"
    );
    assert!(
        written.contains("# set by core"),
        "comments are kept: {written}"
    );

    // The editor's round trip: the redacted text PUT back as raw keeps it too.
    let res = http
        .put(&url)
        .json(&serde_json::json!({ "raw": raw.replace("log_level = \"info\"", "log_level = \"warn\"") }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let written = std::fs::read_to_string(&file).unwrap();
    assert!(
        written.contains("password = \"s3cret-broker-pass\"")
            && written.contains("log_level = \"warn\""),
        "{written}"
    );

    // Not placed and never registered: still not found.
    let res = http
        .get(format!(
            "{}/api/v1/plugins/plugin.nothing/config",
            h.tcp_base()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    h.stop().await;
}
