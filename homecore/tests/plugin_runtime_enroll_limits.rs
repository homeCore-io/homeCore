//! The enrollment rate limit, and nothing else.
//!
//! This binary contains exactly one test on purpose. `POST /enroll` is limited
//! by a process-global static keyed by source IP, so exhausting the budget —
//! which is the whole point here — would starve any other test sharing the
//! process. `common/mod.rs` documents the same trap for login.
//!
//! Every request counts, not just refused ones: the limiter is middleware and
//! runs before the handler.

mod common;

use base64::Engine;
use common::Harness;
use ed25519_dalek::{Signer, SigningKey};
use hc_api_types::plugin_runtimes::{EnrollRequest, RuntimeCapabilities};
use tempfile::TempDir;

/// Matches `rate_limit::MAX_ATTEMPTS`.
const BUDGET: usize = 5;

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

/// An open enrollment endpoint that anyone on the network may call needs a
/// ceiling, or it is a way to fill an operator's approval screen until something
/// gets approved out of fatigue.
#[tokio::test]
async fn enrollment_is_rate_limited_per_source() {
    let tmp = TempDir::new().unwrap();
    let policy = hc_config::PluginRuntimesSection {
        // Large enough that the cap under test is the rate limit, not the
        // pending ceiling — otherwise this would pass for the wrong reason.
        max_pending: 100,
        ..Default::default()
    };
    let h = Harness::start_with(&tmp, vec!["127.0.0.1/32".parse().unwrap()], policy)
        .await
        .unwrap();
    let http = reqwest::Client::new();
    let url = format!("{}/api/v1/plugin-runtimes/enroll", h.tcp_base());

    for i in 0..BUDGET {
        let req = signed(20 + i as u8, &format!("rt-budget{i}"));
        let r = http.post(&url).json(&req).send().await.unwrap();
        assert_ne!(
            r.status(),
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "request {i} is within the budget and must not be limited"
        );
    }

    let over = signed(99, "rt-overbudget");
    let r = http.post(&url).json(&over).send().await.unwrap();
    assert_eq!(
        r.status(),
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "the request past the budget must be refused"
    );
    assert!(
        r.headers().contains_key("retry-after"),
        "a limited caller needs to be told when to come back, not just refused"
    );

    h.stop().await;
}
