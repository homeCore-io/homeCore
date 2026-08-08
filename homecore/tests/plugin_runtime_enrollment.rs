//! Plugin-runtime enrollment, over real HTTP.
//!
//! The unit tests cover the decision rules; these cover the things only a server
//! can be wrong about — status codes, what leaks into a response, and whether the
//! gates actually run before the logic they guard.
//!
//! **Budget note.** `POST /enroll` is rate-limited by a process-global static
//! keyed by source IP, so every test in this binary draws from the same
//! 5-per-minute allowance from 127.0.0.1. Count the enrollments before adding a
//! test. The limit itself is exercised in `plugin_runtime_enroll_limits.rs`,
//! which is a separate binary for exactly that reason.

mod common;

use base64::Engine;
use common::Harness;
use ed25519_dalek::{Signer, SigningKey};
use hc_api_types::plugin_runtimes::{EnrollRequest, EnrollResponse, RuntimeCapabilities};
use tempfile::TempDir;

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn local_only() -> Vec<ipnet::IpNet> {
    vec!["127.0.0.1/32".parse().unwrap()]
}

/// Build a request signed the way a real runtime host signs it.
fn signed(seed: u8, runtime_id: &str) -> EnrollRequest {
    let sk = SigningKey::from_bytes(&[seed; 32]);
    let mut req = EnrollRequest {
        runtime_id: runtime_id.into(),
        public_key: b64().encode(sk.verifying_key().to_bytes()),
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
    req.signature = b64().encode(sk.sign(req.signing_payload().as_bytes()).to_bytes());
    req
}

async fn post_enroll(base: &str, req: &EnrollRequest) -> (reqwest::StatusCode, String) {
    let r = reqwest::Client::new()
        .post(format!("{base}/api/v1/plugin-runtimes/enroll"))
        .json(req)
        .send()
        .await
        .expect("enroll request");
    let status = r.status();
    (status, r.text().await.unwrap_or_default())
}

/// The whole open-mode flow, and what each step is allowed to say.
///
/// Three enrollments — the budget is five.
#[tokio::test]
async fn open_enrollment_pends_then_approves() {
    let tmp = TempDir::new().unwrap();
    let h = Harness::start_with(&tmp, local_only(), Default::default())
        .await
        .unwrap();
    let base = h.tcp_base();
    let http = reqwest::Client::new();

    // 1 — a tampered request never reaches the policy decision.
    let mut bad = signed(3, "rt-tampered");
    bad.capabilities.abi = "cp311-manylinux_2_28".into();
    let (status, _) = post_enroll(&base, &bad).await;
    assert_eq!(
        status,
        reqwest::StatusCode::UNAUTHORIZED,
        "a request whose capabilities were altered after signing must be refused"
    );

    // 2 — a genuine request is left pending, with a code and a secret.
    let req = signed(7, "rt-goodcitizen");
    let (status, body) = post_enroll(&base, &req).await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "{body}");
    let pending: EnrollResponse = serde_json::from_str(&body).unwrap();
    let code = pending.code.clone().expect("a code to compare");
    let secret = pending.enrollment_secret.clone().expect("a poll secret");
    assert!(
        pending.credentials.is_none(),
        "a pending enrollment must not carry credentials: {body}"
    );

    // The admin list shows the same code — that comparison is the security.
    let listed: serde_json::Value = http
        .get(format!("{base}/api/v1/plugin-runtimes"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = &listed["runtimes"][0];
    assert_eq!(entry["code"].as_str(), Some(code.as_str()));
    assert_eq!(entry["status"].as_str(), Some("pending"));
    assert_eq!(
        entry["source_ip"].as_str(),
        Some("127.0.0.1"),
        "the operator judges the source alongside the code"
    );

    // Polling with the wrong secret must not confirm the runtime exists.
    let wrong = http
        .get(format!("{base}/api/v1/plugin-runtimes/rt-goodcitizen"))
        .bearer_auth("hc_sk_definitely-not-the-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(
        wrong.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a wrong secret and an unknown id must be indistinguishable"
    );

    // Approve, then the runtime's own poll hands over credentials.
    let approved = http
        .post(format!(
            "{base}/api/v1/plugin-runtimes/rt-goodcitizen/approve"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(approved.status(), reqwest::StatusCode::OK);

    let polled: EnrollResponse = http
        .get(format!("{base}/api/v1/plugin-runtimes/rt-goodcitizen"))
        .bearer_auth(&secret)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let creds = polled.credentials.expect("credentials after approval");
    assert_eq!(creds.plugin_id, "plugin.python-rtgoodci");
    assert!(!creds.mqtt_password.is_empty());

    // 3 — the same id with a different key is a different runtime, and must not
    // inherit the approval or displace the record.
    let impostor = signed(9, "rt-goodcitizen");
    let (status, body) = post_enroll(&base, &impostor).await;
    assert_eq!(
        status,
        reqwest::StatusCode::CONFLICT,
        "an id read from a log must be useless without the key: {body}"
    );

    h.stop().await;
}

/// Denying increments the count and, at the limit, starts a cooldown.
///
/// One enrollment.
#[tokio::test]
async fn denial_counts_toward_a_cooldown() {
    let tmp = TempDir::new().unwrap();
    let policy = hc_config::PluginRuntimesSection {
        max_denials: 1,
        ..Default::default()
    };
    let h = Harness::start_with(&tmp, local_only(), policy)
        .await
        .unwrap();
    let base = h.tcp_base();
    let http = reqwest::Client::new();

    let req = signed(11, "rt-unwelcome");
    let (status, body) = post_enroll(&base, &req).await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "{body}");

    let denied: serde_json::Value = http
        .post(format!("{base}/api/v1/plugin-runtimes/rt-unwelcome/deny"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(denied["status"].as_str(), Some("denied"));
    assert_eq!(
        denied["code"],
        serde_json::Value::Null,
        "a resolved record must stop advertising a code to compare"
    );

    h.stop().await;
}
