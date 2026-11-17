//! Axum HTTP server that receives POST data from the Ecowitt gateway.
//!
//! The Ecowitt "custom server" feature sends `application/x-www-form-urlencoded`
//! data.  We accept that format and convert to device updates.

use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::StatusCode,
    routing::post,
    Form, Router,
};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use plugin_sdk_rs::types::PluginNotice;
use plugin_sdk_rs::PluginNotices;

use crate::form_parser::parse_form_data;
use crate::registry::DeviceRegistry;

/// Max body size for `/data/report` POSTs. Real Ecowitt payloads are
/// ~500 bytes; 8 KiB leaves headroom for future fields without giving
/// a malformed POST a multi-megabyte memory vector. Axum's default is
/// 2 MB, which is several orders of magnitude too generous here.
const REPORT_BODY_LIMIT_BYTES: usize = 8 * 1024;

/// Shared state for the HTTP server and poller.
pub struct SharedState {
    pub registry: Mutex<DeviceRegistry>,
    pub device_prefix: String,
    /// Source-IP allowlist. Empty = accept any (today's behavior); when
    /// populated, only requests whose peer IP is in the set are
    /// processed. Construction uses HashSet so per-request lookup is
    /// O(1) regardless of list size.
    pub allowed_source_ips: HashSet<IpAddr>,
    /// When the last gateway report was accepted: an upload the gateway
    /// posted, or a successful poll of it. `None` means "not once since
    /// startup".
    ///
    /// A gateway posts on a fixed interval (60 s by default), so silence is
    /// not quiet — it is a fault. Nothing used to notice it: bind to loopback
    /// while the gateway sits on the LAN and every POST lands on a closed port,
    /// with no error at either end. One deployment ran that way for two months.
    /// [`watch_for_silence`] turns that into a log line.
    pub last_report: Mutex<Option<Instant>>,
}

impl SharedState {
    /// Record that sensor data arrived from the gateway, by upload or by
    /// poll. Either one is proof the gateway is reaching us, so either one
    /// keeps the silence watchdog quiet.
    pub async fn note_report(&self, via: &'static str, devices: usize) {
        note_report(&self.last_report, via, devices).await;
    }
}

/// [`SharedState::note_report`] on the timestamp alone.
pub async fn note_report(last_report: &Mutex<Option<Instant>>, via: &'static str, devices: usize) {
    let mut last = last_report.lock().await;
    if last.is_none() {
        info!(via, devices, "First gateway report received");
    }
    *last = Some(Instant::now());
}

/// Returns `true` if the request's peer IP is permitted.
///
/// Pure helper so the rule is unit-testable without spinning up a
/// listener. Empty allowlist means "accept everything," matching the
/// pre-allowlist default behavior.
pub fn ip_allowed(allowed: &HashSet<IpAddr>, peer: IpAddr) -> bool {
    allowed.is_empty() || allowed.contains(&peer)
}

/// Build the axum router.
pub fn router(state: Arc<SharedState>) -> Router {
    Router::new()
        .route("/data/report/", post(handle_report))
        .route("/data/report", post(handle_report))
        .layer(DefaultBodyLimit::max(REPORT_BODY_LIMIT_BYTES))
        .with_state(state)
}

async fn handle_report(
    State(state): State<Arc<SharedState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(fields): Form<HashMap<String, String>>,
) -> StatusCode {
    if !ip_allowed(&state.allowed_source_ips, addr.ip()) {
        warn!(peer = %addr.ip(), "Rejecting POST from non-allowlisted source");
        return StatusCode::FORBIDDEN;
    }
    debug!(
        peer = %addr.ip(),
        fields = fields.len(),
        "Received POST from Ecowitt gateway"
    );

    let updates = parse_form_data(&fields, &state.device_prefix);
    if updates.is_empty() {
        warn!("POST contained no parseable sensor data");
        return StatusCode::OK;
    }

    let count = updates.len();
    state.note_report("upload", count).await;

    let mut registry = state.registry.lock().await;
    registry.process_updates(updates).await;
    debug!(devices = count, "Processed Ecowitt data update");
    StatusCode::OK
}

/// How long the receiver may hear nothing before it says so. A gateway posts
/// every 60 s by default, so this is many missed reports, not a hiccup.
const SILENCE_WARN_AFTER: Duration = Duration::from_secs(10 * 60);

/// How often the watchdog re-checks (and so how often it repeats a complaint).
const SILENCE_CHECK_EVERY: Duration = Duration::from_secs(5 * 60);

/// What the silence watchdog needs to know to word its notices: where the
/// receiver listens, and whether the gateway is also polled.
#[derive(Clone, Debug)]
pub struct SilenceWatch {
    pub bind_addr: String,
    pub port: u16,
    /// `(gateway_ip, poll_interval_secs)` when `[ecowitt].gateway_ip` is set.
    pub poll: Option<(String, u64)>,
}

impl SilenceWatch {
    /// How long without a report counts as silence. Ten minutes, or three
    /// missed polls when the poll interval is longer than that.
    fn warn_after(&self) -> Duration {
        match self.poll {
            Some((_, secs)) => SILENCE_WARN_AFTER.max(Duration::from_secs(secs.saturating_mul(3))),
            None => SILENCE_WARN_AFTER,
        }
    }

    fn bound_to_loopback(&self) -> bool {
        self.bind_addr
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(true)
    }
}

/// Complain when the gateway's reports stop arriving.
///
/// The listener being up says nothing about whether data is reaching it. Bind
/// to loopback while the gateway sits on the LAN and every POST is dropped by
/// the kernel: the gateway sees no error, the plugin sees no request, homeCore
/// keeps serving the last values it ever got, and the sensors quietly go stale.
/// That failure ran undetected for two months in one deployment.
///
/// A report is an upload or a successful poll, so a setup that only polls is
/// healthy as long as its polls return data.
pub async fn watch_for_silence(
    state: Arc<SharedState>,
    watch: SilenceWatch,
    notices: PluginNotices,
) {
    loop {
        tokio::time::sleep(SILENCE_CHECK_EVERY).await;
        let since = state.last_report.lock().await.map(|at| at.elapsed());
        check_silence(since, &watch, &notices);
    }
}

/// One watchdog pass: `since` is how long ago the last report arrived, `None`
/// for never. Clears the notices when data is flowing, raises one otherwise.
pub fn check_silence(since: Option<Duration>, watch: &SilenceWatch, notices: &PluginNotices) {
    let SilenceWatch {
        bind_addr, port, ..
    } = watch;
    match (since, &watch.poll) {
        (Some(ago), _) if ago < watch.warn_after() => {
            // Data is flowing. Drop anything we raised earlier so a
            // resolved problem stops being shown; the operator should not
            // have to dismiss a warning that fixed itself.
            notices.clear("gateway_silent");
            notices.clear("no_reports_received");
        }
        (Some(ago), None) => {
            let mins = ago.as_secs() / 60;
            warn!(
                silent_for_secs = ago.as_secs(),
                "No report from the Ecowitt gateway recently. It reports every 60 s, so this \
                 is many missed uploads. Check the gateway is powered and still has this host \
                 set as its custom-upload server."
            );
            notices.raise(
                PluginNotice::warning(
                    "gateway_silent",
                    format!(
                        "No report from the gateway for about {mins} minutes. It normally \
                         uploads every 60 seconds, so readings shown here are stale."
                    ),
                )
                .with_remedy(
                    "Check the gateway is powered and still has this host set as its \
                     custom-upload server.",
                ),
            );
        }
        (Some(ago), Some((gateway_ip, secs))) => {
            let mins = ago.as_secs() / 60;
            warn!(
                silent_for_secs = ago.as_secs(),
                gateway_ip = %gateway_ip,
                "No data from the Ecowitt gateway recently, by poll or upload. Earlier log \
                 lines say why each poll failed."
            );
            notices.raise(
                PluginNotice::warning(
                    "gateway_silent",
                    format!(
                        "No data from the gateway for about {mins} minutes. It is polled at \
                         {gateway_ip} every {secs} seconds and no poll or upload has brought \
                         readings since, so readings shown here are stale."
                    ),
                )
                .with_remedy(
                    "Check the gateway is powered and answers at its gateway_ip address. \
                     The plugin log says why each poll failed.",
                ),
            );
        }
        (None, Some((gateway_ip, _))) => {
            warn!(
                gateway_ip = %gateway_ip,
                "No data has EVER arrived from the Ecowitt gateway: no poll has returned \
                 readings and no upload has arrived. Earlier log lines say why each poll failed."
            );
            notices.raise(
                PluginNotice::error(
                    "no_reports_received",
                    format!(
                        "No data has arrived from the gateway since the plugin started. \
                         Polling {gateway_ip} has not returned any readings, and no upload \
                         has arrived either."
                    ),
                )
                .with_remedy(
                    "Check [ecowitt].gateway_ip is the gateway's address and that this host \
                     can reach it over HTTP. The plugin log says why each poll failed.",
                ),
            );
        }
        (None, None) if watch.bound_to_loopback() => {
            warn!(
                bind_addr = %bind_addr,
                port,
                "No gateway report has EVER arrived, and the receiver is bound to loopback, so a \
                 gateway anywhere else on the network cannot reach it, and its POSTs are being \
                 dropped with no error at either end. Set [ecowitt].bind_addr = \"0.0.0.0\" and \
                 list the gateway in [ecowitt].allowed_source_ips."
            );
            // Escalates the config warning raised at startup: this is no
            // longer "will not work", it is "has not worked, confirmed by
            // the absence of a single upload".
            notices.raise(
                PluginNotice::error(
                    "no_reports_received",
                    format!(
                        "No gateway upload has ever arrived, and the receiver is bound to \
                         {bind_addr}:{port}, so a gateway elsewhere on the network cannot \
                         reach it."
                    ),
                )
                .with_remedy(
                    "Set [ecowitt].gateway_ip to poll the gateway over outbound HTTP \
                     instead. That works even in a container on a bridge network. To \
                     keep receiving uploads, set [ecowitt].bind_addr = \"0.0.0.0\", list \
                     the gateway in [ecowitt].allowed_source_ips, and ensure the listen \
                     port is reachable from the gateway.",
                ),
            );
        }
        (None, None) => {
            warn!(
                bind_addr = %bind_addr,
                port,
                "No gateway report has EVER arrived. Check the gateway's custom-upload \
                 settings point at this host and port (Protocol=Ecowitt, \
                 Path=/data/report/), and that nothing between them is blocking the port."
            );
            // Reachable bind, still nothing: the receiver is listening
            // where it should, so the gap is upstream of us.
            notices.raise(
                PluginNotice::error(
                    "no_reports_received",
                    format!(
                        "No gateway upload has ever arrived. The receiver is listening on \
                         {bind_addr}:{port}, so nothing is reaching it."
                    ),
                )
                .with_remedy(
                    "Check the gateway's custom-upload settings point at this host and port \
                     (Protocol=Ecowitt, Path=/data/report/), and that nothing between them \
                     is blocking the port.",
                ),
            );
        }
    }
}

/// Start the HTTP server on the configured bind address + port.
///
/// `bind_addr` is parsed; on failure we fall back to loopback rather
/// than crashing — the operator typo'd a config field, not a security
/// boundary.
pub async fn serve(bind_addr: &str, port: u16, state: Arc<SharedState>) {
    let ip: IpAddr = bind_addr.parse().unwrap_or_else(|e| {
        tracing::warn!(
            bind_addr,
            error = %e,
            "could not parse [ecowitt].bind_addr; falling back to 127.0.0.1"
        );
        IpAddr::from([127, 0, 0, 1])
    });
    let app = router(state);
    let addr = SocketAddr::new(ip, port);

    tracing::info!(bind = %addr, "Ecowitt HTTP receiver listening");

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, %addr, "Failed to bind HTTP listener");
            return;
        }
    };

    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    {
        tracing::error!(error = %e, "HTTP server error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn empty_allowlist_accepts_any() {
        let allow = HashSet::new();
        assert!(ip_allowed(&allow, ip("10.0.0.5")));
        assert!(ip_allowed(&allow, ip("192.168.1.1")));
        assert!(ip_allowed(&allow, ip("::1")));
    }

    #[test]
    fn populated_allowlist_only_admits_listed_peers() {
        let mut allow = HashSet::new();
        allow.insert(ip("192.168.1.50"));
        allow.insert(ip("192.168.1.51"));
        assert!(ip_allowed(&allow, ip("192.168.1.50")));
        assert!(ip_allowed(&allow, ip("192.168.1.51")));
        assert!(!ip_allowed(&allow, ip("192.168.1.99")));
        assert!(!ip_allowed(&allow, ip("127.0.0.1")));
    }

    #[test]
    fn allowlist_distinguishes_v4_from_v6_loopback() {
        // Operators sometimes assume "127.0.0.1" covers "::1" (or vice
        // versa). It doesn't — they're distinct addresses. This test
        // pins that behavior so a refactor doesn't accidentally relax
        // the check.
        let mut allow = HashSet::new();
        allow.insert(ip("127.0.0.1"));
        assert!(ip_allowed(&allow, ip("127.0.0.1")));
        assert!(!ip_allowed(&allow, ip("::1")));
    }

    fn watch(bind: &str, poll: Option<(&str, u64)>) -> SilenceWatch {
        SilenceWatch {
            bind_addr: bind.into(),
            port: 8080,
            poll: poll.map(|(ip, secs)| (ip.into(), secs)),
        }
    }

    fn codes(n: &PluginNotices) -> Vec<String> {
        n.current().into_iter().map(|n| n.code).collect()
    }

    /// A setup that only polls: the poll's report keeps the watchdog quiet,
    /// and clears a notice raised before the first poll landed.
    #[tokio::test]
    async fn a_successful_poll_counts_as_a_report() {
        let last_report = Mutex::new(None);
        let w = watch("127.0.0.1", Some(("192.0.2.10", 60)));
        let notices = PluginNotices::test_instance();

        check_silence(None, &w, &notices);
        assert_eq!(codes(&notices), ["no_reports_received"]);
        let msg = &notices.current()[0].message;
        assert!(msg.contains("Polling 192.0.2.10"), "{msg}");
        assert!(!msg.contains("bound to"), "{msg}");

        note_report(&last_report, "poll", 3).await;
        let since = last_report.lock().await.map(|at| at.elapsed());
        assert!(since.is_some());
        check_silence(since, &w, &notices);
        assert!(codes(&notices).is_empty());
    }

    #[test]
    fn silence_is_worded_for_the_mode() {
        let notices = PluginNotices::test_instance();
        let long_ago = Some(Duration::from_secs(20 * 60));

        check_silence(long_ago, &watch("0.0.0.0", None), &notices);
        let n = &notices.current()[0];
        assert_eq!(n.code, "gateway_silent");
        assert!(
            n.message.contains("uploads every 60 seconds"),
            "{}",
            n.message
        );

        check_silence(
            long_ago,
            &watch("0.0.0.0", Some(("192.0.2.10", 60))),
            &notices,
        );
        let n = &notices.current()[0];
        assert!(n.message.contains("polled at 192.0.2.10"), "{}", n.message);

        // Uploads only, never anything, loopback bind: the loopback wording.
        let notices = PluginNotices::test_instance();
        check_silence(None, &watch("127.0.0.1", None), &notices);
        assert!(notices.current()[0]
            .message
            .contains("bound to 127.0.0.1:8080"));
    }

    #[test]
    fn a_long_poll_interval_is_not_silence() {
        let notices = PluginNotices::test_instance();
        // Polled every 15 minutes, last poll 20 minutes ago: one late poll,
        // not a fault.
        let w = watch("0.0.0.0", Some(("192.0.2.10", 15 * 60)));
        check_silence(Some(Duration::from_secs(20 * 60)), &w, &notices);
        assert!(codes(&notices).is_empty());
        check_silence(Some(Duration::from_secs(50 * 60)), &w, &notices);
        assert_eq!(codes(&notices), ["gateway_silent"]);
    }
}
