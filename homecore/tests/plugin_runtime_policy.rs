//! The policy gates on enrollment: who may ask, and how many at once.
//!
//! Its own binary because it spends enrollment-budget requests, and the limiter
//! is a process-global static keyed by source IP. Three enrollments here, against
//! a budget of five.

mod common;

use base64::Engine;
use common::{make_user, Harness};
use ed25519_dalek::{Signer, SigningKey};
use hc_api_types::plugin_runtimes::{EnrollRequest, RuntimeCapabilities};
use hc_auth::Role;
use tempfile::TempDir;

fn signed(seed: u8, runtime_id: &str) -> EnrollRequest {
    let b64 = base64::engine::general_purpose::STANDARD;
    let sk = SigningKey::from_bytes(&[seed; 32]);
    let mut req = EnrollRequest {
        runtime_id: runtime_id.into(),
        public_key: b64.encode(sk.verifying_key().to_bytes()),
        signature: String::new(),
        capabilities: RuntimeCapabilities {
            kind: "python".into(),
            abi: "cp312-manylinux_2_28".into(),
            arch: "x86_64".into(),
        },
        host_version: "0.1.0".into(),
        sdk_version: "0.2.0".into(),
        hostname: "pyhost".into(),
        network_mode: "host".into(),
        token: None,
    };
    req.signature = b64.encode(sk.sign(req.signing_payload().as_bytes()).to_bytes());
    req
}

async fn enroll(base: &str, req: &EnrollRequest) -> reqwest::StatusCode {
    reqwest::Client::new()
        .post(format!("{base}/api/v1/plugin-runtimes/enroll"))
        .json(req)
        .send()
        .await
        .expect("enroll request")
        .status()
}

/// A runtime is a machine on your network. An enrollment from outside it should
/// not reach an approval screen at all, so the gate has to run before the
/// decision rather than being something the operator is trusted to notice.
///
/// The harness is started with an *empty* whitelist, which is the shape a
/// deployment has before anyone configures one — so this also pins the
/// fail-closed default.
#[tokio::test]
async fn enrollment_from_outside_the_whitelist_is_refused() {
    let tmp = TempDir::new().unwrap();
    let h = Harness::start_with(&tmp, Vec::new(), Default::default())
        .await
        .unwrap();

    let status = enroll(&h.tcp_base(), &signed(31, "rt-stranger")).await;
    assert_eq!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "an empty whitelist must refuse everyone, not admit everyone"
    );

    // ...and nothing was recorded, so a refused stranger cannot occupy a pending
    // slot or appear on the approval screen.
    //
    // Needs a real admin token: an empty whitelist is also an empty auth
    // bypass, which is the same setting doing both jobs and worth seeing.
    let admin = make_user(&h, "runtimes-admin", "correct horse battery", Role::Admin)
        .await
        .unwrap();
    let listed: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/api/v1/plugin-runtimes", h.tcp_base()))
        .bearer_auth(&admin.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        listed["runtimes"].as_array().map(|a| a.len()),
        Some(0),
        "a refused enrollment must leave no trace to approve: {listed}"
    );

    h.stop().await;
}

/// The pending ceiling. Without it, an open endpoint is a way to fill the
/// approval screen with plausible-looking requests until one is approved out of
/// fatigue.
#[tokio::test]
async fn the_pending_ceiling_refuses_the_next_one() {
    let tmp = TempDir::new().unwrap();
    let policy = hc_config::PluginRuntimesSection {
        max_pending: 1,
        ..Default::default()
    };
    let h = Harness::start_with(&tmp, vec!["127.0.0.1/32".parse().unwrap()], policy)
        .await
        .unwrap();
    let base = h.tcp_base();

    assert_eq!(
        enroll(&base, &signed(41, "rt-first")).await,
        reqwest::StatusCode::ACCEPTED,
        "the first request fits under the ceiling"
    );
    assert_eq!(
        enroll(&base, &signed(42, "rt-second")).await,
        reqwest::StatusCode::FORBIDDEN,
        "the second exceeds it and must be refused"
    );

    // The one that got in is still there — the ceiling refuses newcomers rather
    // than evicting whoever is already waiting.
    let listed: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/api/v1/plugin-runtimes"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = listed["runtimes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["runtime_id"].as_str())
        .collect();
    assert_eq!(ids, vec!["rt-first"], "{listed}");

    h.stop().await;
}
