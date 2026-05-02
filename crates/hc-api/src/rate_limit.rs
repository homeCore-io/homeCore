//! Per-IP rate limiting for the login endpoint.
//!
//! Sliding window over the last `WINDOW`. If the same source IP submits more
//! than `MAX_ATTEMPTS` requests in that window, further requests get 429 with
//! a `Retry-After` header until older entries age out.
//!
//! Counts every request, not just failures — keeps the implementation small.
//! 5/min is comfortable for a normal login while killing scripted brute force.
//!
//! Note: when homeCore sits behind a reverse proxy that doesn't forward the
//! original client IP, every request looks like it came from the proxy and
//! the limiter degrades to a global cap. Operators who terminate TLS upstream
//! should ensure they pass the real client IP through (or limit at the proxy).

use axum::{
    extract::{ConnectInfo, Request},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_ATTEMPTS: usize = 5;
const WINDOW: Duration = Duration::from_secs(60);

fn buckets() -> &'static Mutex<HashMap<IpAddr, Vec<Instant>>> {
    static B: OnceLock<Mutex<HashMap<IpAddr, Vec<Instant>>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record an attempt from `ip`. Returns `Ok(())` if accepted, or
/// `Err(retry_after_seconds)` if the IP has hit the limit.
fn record(ip: IpAddr, now: Instant) -> Result<(), u64> {
    let mut state = buckets().lock().expect("rate limiter mutex poisoned");

    // GC empty / fully-aged-out entries opportunistically. Cheap because the
    // map only ever holds IPs that recently touched the login endpoint.
    state.retain(|_, ts| {
        ts.retain(|t| now.duration_since(*t) < WINDOW);
        !ts.is_empty()
    });

    let entry = state.entry(ip).or_default();
    if entry.len() >= MAX_ATTEMPTS {
        let oldest = *entry.first().expect("len>=MAX_ATTEMPTS implies non-empty");
        let retry_after = WINDOW
            .saturating_sub(now.duration_since(oldest))
            .as_secs()
            .saturating_add(1);
        return Err(retry_after);
    }
    entry.push(now);
    Ok(())
}

pub async fn login_rate_limit(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    match record(addr.ip(), Instant::now()) {
        Ok(()) => next.run(request).await,
        Err(retry_after) => {
            tracing::warn!(
                ip = %addr.ip(),
                retry_after_seconds = retry_after,
                "login rate limit exceeded"
            );
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("Retry-After", retry_after.to_string())],
                Json(json!({
                    "error": "too many login attempts",
                    "retry_after_seconds": retry_after,
                })),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, n))
    }

    fn reset() {
        buckets().lock().unwrap().clear();
    }

    #[test]
    fn allows_up_to_max_attempts() {
        reset();
        let now = Instant::now();
        for i in 0..MAX_ATTEMPTS {
            assert!(record(ip(1), now).is_ok(), "attempt {i} should pass");
        }
    }

    #[test]
    fn rejects_after_max_attempts() {
        reset();
        let now = Instant::now();
        for _ in 0..MAX_ATTEMPTS {
            record(ip(2), now).unwrap();
        }
        let err = record(ip(2), now).unwrap_err();
        assert!(err >= 1 && err <= WINDOW.as_secs() + 1);
    }

    #[test]
    fn separate_ips_have_separate_buckets() {
        reset();
        let now = Instant::now();
        for _ in 0..MAX_ATTEMPTS {
            record(ip(3), now).unwrap();
        }
        // ip(3) is over its limit; ip(4) should still be fine.
        assert!(record(ip(3), now).is_err());
        assert!(record(ip(4), now).is_ok());
    }

    #[test]
    fn old_attempts_age_out() {
        reset();
        let past = Instant::now() - WINDOW - Duration::from_secs(1);
        for _ in 0..MAX_ATTEMPTS {
            record(ip(5), past).unwrap();
        }
        // After the window, the bucket should be empty again.
        assert!(record(ip(5), Instant::now()).is_ok());
    }
}
