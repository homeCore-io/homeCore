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
            abi: "cp312-manylinux_2_28".into(),
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
        serde_json::json!({"name": "Python runtime 1", "homecore_url": "http://homecore.local:8080"}),
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
            serde_json::json!({"name": " ", "homecore_url": "http://h:8080"}),
            "name",
        ),
        (
            serde_json::json!({"name": "R", "homecore_url": "h:8080"}),
            "http",
        ),
        (
            serde_json::json!({"name": "R", "kind": "node", "homecore_url": "http://h:8080"}),
            "python",
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
        serde_json::json!({"name": "R", "homecore_url": "http://h:8080"}),
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
        serde_json::json!({"name": "Pi", "homecore_url": "http://h:8080", "max_plugins": 3}),
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
