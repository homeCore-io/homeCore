//! `hc-broker` — embedded MQTT broker for homeCore.
//!
//! Wraps [`rumqttd`] to provide an in-process MQTT broker.  Handles lifecycle,
//! optional TLS, and per-client credential enforcement.
//!
//! ## Authentication
//!
//! When `BrokerConfig::clients` is non-empty the broker enables password
//! authentication.  Each client must present its `id` as the MQTT username and
//! the matching `password`.  Connections that supply wrong or missing credentials
//! are rejected at the CONNECT packet stage.
//!
//! ## TLS
//!
//! Set `cert_path`, `key_path`, and `tls_port` to listen on a second TLS port
//! in addition to the plain-text port.  Rustls is used (PEM certificate + key).
//!
//! ## Topic ACL note
//!
//! `ClientAcl::allow_pub` and `allow_sub` are stored in config and surfaced as
//! structured data (e.g. for generating external broker config), but **rumqttd
//! does not enforce per-topic publish/subscribe ACL at the broker level**.
//! Topic isolation is achieved instead through the plugin SDK, which only
//! publishes to its own device topics by convention.  For strict topic ACL use
//! an external broker (Mosquitto, EMQX) set as `[broker] external_url`, which
//! then runs instead of this one.

use anyhow::{Context, Result};
use rumqttd::{
    Broker as RumqttdBroker, Config, ConnectionSettings, RouterConfig, ServerSettings, TlsConfig,
};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use tracing::{info, warn};

/// A configured client first, then core's live list.
///
/// The live check runs on a blocking thread: it may hash a password, and the
/// broker's own threads carry every connected client's traffic.
async fn check_login(
    fixed: &HashMap<String, String>,
    live: &LiveLogins,
    client_id: String,
    user: String,
    pass: String,
) -> bool {
    // A configured client's id is its own: the broker drops whoever holds a
    // client id when another connects with it, so any login using
    // `internal.core` would push core itself off.
    if fixed.contains_key(&client_id) && client_id != user {
        tracing::warn!(%client_id, "broker login refused: another login's client id");
        return false;
    }
    if let Some(expected) = fixed.get(&user) {
        return same_bytes(expected.as_bytes(), pass.as_bytes());
    }
    let live = live.0.clone();
    let accepted = tokio::task::spawn_blocking(move || live(&client_id, &user, &pass))
        .await
        .unwrap_or(false);
    if !accepted {
        tracing::debug!("broker login refused: not a configured client or a login core made");
    }
    accepted
}

/// Constant-time equality, so a refused login says nothing about how close it was.
pub fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Per-client credential and ACL entry.
///
/// `allow_pub` / `allow_sub` are stored as metadata (for documentation and
/// external broker config generation) but are not enforced by the embedded
/// rumqttd broker.
#[derive(Debug, Clone)]
pub struct ClientAcl {
    pub client_id: String,
    pub password: String,
    /// Topic patterns this client may publish to (metadata, not enforced by embedded broker).
    pub allow_pub: Vec<String>,
    /// Topic patterns this client may subscribe to (metadata, not enforced by embedded broker).
    pub allow_sub: Vec<String>,
}

/// Logins core creates as it goes: plugin runtimes, and every plugin core
/// installs or places, each with its own password.
///
/// Asked only when a username is not one of `[[broker.clients]]`, with the
/// username and password from the CONNECT packet, and answered from core's
/// current state, so a login works the moment it is made and stops the moment
/// it is removed. It may be slow (a password hash, a small file read): the
/// broker runs it on a blocking thread.
#[derive(Clone)]
pub struct LiveLogins(pub Arc<LoginCheck>);

/// `(client id, username, password) -> accepted`.
pub type LoginCheck = dyn Fn(&str, &str, &str) -> bool + Send + Sync;

impl std::fmt::Debug for LiveLogins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LiveLogins")
    }
}

/// Configuration for the embedded broker.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    pub host: String,
    /// Plain-text MQTT v3.1.1 port (used by Gen1 devices and the internal core client).
    pub port: u16,
    /// Optional MQTT v5 port for Gen2/Gen3 devices (e.g. Shelly Plus/Pro).
    /// When set, a second listener is started that speaks MQTT v5.
    pub v5_port: Option<u16>,
    /// Optional TLS port.  Requires `cert_path` and `key_path` to be set.
    pub tls_port: Option<u16>,
    /// Path to PEM-encoded server certificate (for TLS listener).
    pub cert_path: Option<String>,
    /// Path to PEM-encoded private key (for TLS listener).
    pub key_path: Option<String>,
    /// Registered clients.  If non-empty, the broker requires credentials.
    pub clients: Vec<ClientAcl>,
    /// Logins core makes, accepted beside `clients` whenever credentials are
    /// required. Ignored while `clients` is empty: a broker with no clients
    /// asks for no login at all, as it always has.
    pub live_logins: Option<LiveLogins>,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 1883,
            v5_port: Some(1884),
            tls_port: None,
            cert_path: None,
            key_path: None,
            clients: vec![],
            live_logins: None,
        }
    }
}

/// Owns the embedded `rumqttd` broker.
pub struct Broker {
    config: BrokerConfig,
}

impl Broker {
    pub fn new(config: BrokerConfig) -> Self {
        Self { config }
    }

    /// Build a `rumqttd::Config` from our `BrokerConfig`.
    fn build_config(&self) -> Config {
        let host: Ipv4Addr = self.config.host.parse().unwrap_or(Ipv4Addr::UNSPECIFIED);

        let router = RouterConfig {
            max_connections: 1000,
            max_outgoing_packet_count: 200,
            max_segment_size: 104_857_600,
            max_segment_count: 10,
            ..Default::default()
        };

        // Build auth map: client_id → password.
        // rumqttd checks (username, password) from the MQTT CONNECT packet.
        // The plugin SDK and internal client both set username = client_id.
        let auth: Option<HashMap<String, String>> = if self.config.clients.is_empty() {
            None
        } else {
            let map: HashMap<String, String> = self
                .config
                .clients
                .iter()
                .map(|c| (c.client_id.clone(), c.password.clone()))
                .collect();
            info!(clients = map.len(), "Broker authentication enabled");
            Some(map)
        };

        let mut connection_settings = ConnectionSettings {
            connection_timeout_ms: 5000,
            max_payload_size: 262_144,
            max_inflight_count: 200,
            auth: auth.clone(),
            external_auth: None,
            dynamic_filters: false,
        };
        // With a live list, every login goes through one check: rumqttd asks
        // the handler instead of its own map whenever one is set.
        if let (Some(fixed), Some(live)) = (auth, self.config.live_logins.clone()) {
            let fixed = Arc::new(fixed);
            connection_settings.set_auth_handler(
                move |client_id: String, user: String, pass: String| {
                    let fixed = fixed.clone();
                    let live = live.clone();
                    async move { check_login(&fixed, &live, client_id, user, pass).await }
                },
            );
        }

        // Plain-text MQTT v3.1.1 listener (Gen1 devices + internal core client).
        let tcp = ServerSettings {
            name: "homecore-tcp".into(),
            listen: SocketAddrV4::new(host, self.config.port).into(),
            tls: None,
            next_connection_delay_ms: 1,
            connections: connection_settings.clone(),
        };

        let mut v4_servers = HashMap::new();
        v4_servers.insert("tcp".to_string(), tcp);

        // Optional MQTT v5 listener on a separate port (Gen2/Gen3 devices).
        // v4 and v5 cannot share a port — rumqttd binds each map entry independently.
        let mut v5_servers = HashMap::new();
        if let Some(v5_port) = self.config.v5_port {
            let tcp_v5 = ServerSettings {
                name: "homecore-tcp-v5".into(),
                listen: SocketAddrV4::new(host, v5_port).into(),
                tls: None,
                next_connection_delay_ms: 1,
                connections: connection_settings.clone(),
            };
            v5_servers.insert("tcp-v5".to_string(), tcp_v5);
            info!(
                port = v5_port,
                "MQTT v5 listener enabled (for Gen2/Gen3 devices)"
            );
        }

        // Optional TLS listener (v3.1.1).
        if let (Some(tls_port), Some(cert), Some(key)) = (
            self.config.tls_port,
            &self.config.cert_path,
            &self.config.key_path,
        ) {
            let tls_config = TlsConfig::Rustls {
                capath: None,
                certpath: cert.clone(),
                keypath: key.clone(),
            };

            if tls_config.validate_paths() {
                let tls = ServerSettings {
                    name: "homecore-tls".into(),
                    listen: SocketAddrV4::new(host, tls_port).into(),
                    tls: Some(tls_config),
                    next_connection_delay_ms: 1,
                    connections: connection_settings,
                };
                v4_servers.insert("tls".to_string(), tls);
                info!(port = tls_port, cert = %cert, "TLS listener enabled");
            } else {
                warn!(
                    cert = %cert,
                    key = %key,
                    "TLS cert/key files not found — TLS listener disabled"
                );
            }
        }

        Config {
            id: 0,
            router,
            v4: Some(v4_servers),
            v5: if v5_servers.is_empty() {
                None
            } else {
                Some(v5_servers)
            },
            ws: None,
            cluster: None,
            console: None,
            bridge: None,
            metrics: Default::default(),
            prometheus: Default::default(),
        }
    }

    /// Refuse to start the embedded broker in the most dangerous configuration:
    /// bound to a non-loopback address with no `[[broker.clients]]` configured.
    /// rumqttd does not enforce topic ACLs, so anonymous + remotely-reachable
    /// means anything on the LAN can publish to any topic — including device
    /// command and core management topics.
    ///
    /// Operators who genuinely want this combination (e.g. behind an isolated
    /// VLAN with external firewall) can set `HC_ALLOW_ANONYMOUS_REMOTE_BROKER=1`
    /// to bypass the check; the guard still emits a loud `error!` log.
    fn validate_safety(&self) -> Result<()> {
        let host: Ipv4Addr = self.config.host.parse().unwrap_or(Ipv4Addr::UNSPECIFIED);
        let is_loopback_only = host.is_loopback();
        let anonymous = self.config.clients.is_empty();

        if is_loopback_only || !anonymous {
            return Ok(());
        }

        if std::env::var("HC_ALLOW_ANONYMOUS_REMOTE_BROKER").as_deref() == Ok("1") {
            tracing::error!(
                host = %self.config.host,
                "Embedded broker accepting anonymous connections on a non-loopback \
                 address — explicitly allowed via HC_ALLOW_ANONYMOUS_REMOTE_BROKER=1. \
                 Anyone reachable on this network can publish to any topic. \
                 This is unsafe outside an isolated VLAN."
            );
            return Ok(());
        }

        anyhow::bail!(
            "[broker] is bound to {host} (non-loopback) with no [[broker.clients]] \
             credentials configured. The embedded rumqttd broker only enforces \
             CONNECT authentication — without credentials, anyone on this network \
             can publish to any topic.\n\n\
             Choose one:\n  \
               1. Set [broker].host = \"127.0.0.1\" to keep MQTT loopback-only.\n  \
               2. Add at least one [[broker.clients]] entry with credentials \
                  (see https://homecore.io/docs/administration/broker).\n  \
               3. If you understand the risk and have isolated this network \
                  another way, set HC_ALLOW_ANONYMOUS_REMOTE_BROKER=1."
        );
    }

    /// Every address the broker will listen on.
    fn listen_addrs(&self) -> Vec<SocketAddrV4> {
        let host: Ipv4Addr = self.config.host.parse().unwrap_or(Ipv4Addr::UNSPECIFIED);
        let mut ports = vec![self.config.port];
        ports.extend(self.config.v5_port);
        if self.config.cert_path.is_some() && self.config.key_path.is_some() {
            ports.extend(self.config.tls_port);
        }
        ports
            .into_iter()
            .map(|p| SocketAddrV4::new(host, p))
            .collect()
    }

    /// Refuse to start when a port is already taken.
    ///
    /// rumqttd binds on its own threads and only logs a failure there, so a
    /// broker that could not bind used to leave homeCore running with no broker:
    /// its own client retrying forever, plugins never started, the API never
    /// up. The usual cause is another homeCore on the same ports, often one
    /// still shutting down. Trying each address here first turns that into an
    /// error at startup that says so. The ports are released before rumqttd
    /// binds them a moment later.
    fn check_ports(&self) -> Result<()> {
        for addr in self.listen_addrs() {
            if let Err(e) = std::net::TcpListener::bind(addr) {
                anyhow::bail!(
                    "the MQTT broker can't listen on {addr}: {e}. Another program has that \
                     port, often another homeCore still running or shutting down. Stop it \
                     and start again, or change the port under [broker] in homecore.toml."
                );
            }
        }
        Ok(())
    }

    /// Start the broker synchronously.  Blocks until the broker exits.
    /// Call [`Broker::spawn`] to run it in a background thread instead.
    pub fn start(self) -> Result<()> {
        self.validate_safety()?;
        let port = self.config.port;
        info!(port, "Embedded MQTT broker starting");
        let mut broker = RumqttdBroker::new(self.build_config());
        broker.start().context("rumqttd broker exited")?;
        Ok(())
    }

    /// Spawn the broker on a dedicated OS thread.  Returns after the broker
    /// has had a brief moment to bind its port.
    pub fn spawn(self) -> Result<()> {
        // Run the safety check on the calling thread so a misconfigured broker
        // aborts startup rather than logging an error in a background thread
        // and letting homeCore continue without a working broker.
        self.validate_safety()?;
        self.check_ports()?;
        let port = self.config.port;
        std::thread::Builder::new()
            .name("hc-broker".into())
            .spawn(move || {
                if let Err(e) = self.start() {
                    tracing::error!(error = %e, "Embedded broker exited with error");
                }
            })
            .context("failed to spawn broker thread")?;
        // Brief sleep so the broker is ready before callers try to connect.
        std::thread::sleep(std::time::Duration::from_millis(300));
        info!(port, "Embedded MQTT broker ready");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(host: &str, with_creds: bool) -> BrokerConfig {
        BrokerConfig {
            host: host.into(),
            port: 1883,
            v5_port: None,
            tls_port: None,
            cert_path: None,
            key_path: None,
            clients: if with_creds {
                vec![ClientAcl {
                    client_id: "internal.core".into(),
                    password: "x".into(),
                    allow_pub: vec![],
                    allow_sub: vec![],
                }]
            } else {
                vec![]
            },
            live_logins: None,
        }
    }

    fn live(user: &'static str, pass: &'static str) -> LiveLogins {
        LiveLogins(Arc::new(move |_, u, p| u == user && p == pass))
    }

    fn fixed() -> HashMap<String, String> {
        HashMap::from([("internal.core".to_string(), "x".to_string())])
    }

    #[tokio::test]
    async fn a_configured_client_logs_in_by_its_password() {
        let l = live("plugin.a", "secret");
        assert!(
            check_login(
                &fixed(),
                &l,
                "internal.core".into(),
                "internal.core".into(),
                "x".into()
            )
            .await
        );
        assert!(
            !check_login(
                &fixed(),
                &l,
                "internal.core".into(),
                "internal.core".into(),
                "y".into()
            )
            .await
        );
    }

    #[tokio::test]
    async fn a_login_core_made_is_accepted_beside_the_configured_ones() {
        let l = live("plugin.a", "secret");
        assert!(
            check_login(
                &fixed(),
                &l,
                "plugin.a".into(),
                "plugin.a".into(),
                "secret".into()
            )
            .await
        );
        assert!(
            !check_login(
                &fixed(),
                &l,
                "plugin.a".into(),
                "plugin.a".into(),
                "wrong".into()
            )
            .await
        );
        assert!(
            !check_login(
                &fixed(),
                &l,
                "plugin.b".into(),
                "plugin.b".into(),
                "secret".into()
            )
            .await
        );
    }

    /// A live check that would accept anything must not widen a configured
    /// client: the configured password decides for that name.
    #[tokio::test]
    async fn a_configured_name_is_never_handed_to_the_live_list() {
        let anything = LiveLogins(Arc::new(|_, _, _| true));
        assert!(
            !check_login(
                &fixed(),
                &anything,
                "internal.core".into(),
                "internal.core".into(),
                "y".into()
            )
            .await
        );
    }

    /// Nobody else may connect as a configured client: it would push that
    /// client (core itself) off the broker.
    #[tokio::test]
    async fn a_configured_clients_id_is_its_own() {
        let anything = LiveLogins(Arc::new(|_, _, _| true));
        assert!(
            !check_login(
                &fixed(),
                &anything,
                "internal.core".into(),
                "tasmota-plug".into(),
                "pw".into()
            )
            .await
        );
        assert!(
            check_login(
                &fixed(),
                &anything,
                "DVES_1A2B3C".into(),
                "tasmota-plug".into(),
                "pw".into()
            )
            .await
        );
    }

    #[test]
    fn a_port_already_in_use_is_refused_at_startup() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut c = cfg("127.0.0.1", false);
        c.port = taken.local_addr().unwrap().port();
        let err = Broker::new(c.clone())
            .check_ports()
            .unwrap_err()
            .to_string();
        assert!(err.contains(&c.port.to_string()), "{err}");
        assert!(err.contains("another homeCore"), "{err}");

        drop(taken);
        Broker::new(c).check_ports().expect("free once released");
    }

    #[test]
    fn same_bytes_compares_whole_values() {
        assert!(same_bytes(b"abc", b"abc"));
        assert!(!same_bytes(b"abc", b"abd"));
        assert!(!same_bytes(b"abc", b"ab"));
    }

    #[test]
    fn loopback_with_no_creds_is_allowed() {
        Broker::new(cfg("127.0.0.1", false))
            .validate_safety()
            .expect("loopback + anonymous is the safe default");
    }

    #[test]
    fn non_loopback_with_creds_is_allowed() {
        Broker::new(cfg("0.0.0.0", true))
            .validate_safety()
            .expect("credentials gate the LAN-reachable broker");
    }

    // Refuse-and-override behaviors share a process-global env var, so they
    // run together to avoid racing parallel test threads.
    #[test]
    fn non_loopback_anonymous_refuses_unless_overridden() {
        std::env::remove_var("HC_ALLOW_ANONYMOUS_REMOTE_BROKER");
        let err = Broker::new(cfg("0.0.0.0", false))
            .validate_safety()
            .expect_err("loopback-bound anonymous broker should refuse to start");
        let msg = format!("{err}");
        assert!(msg.contains("HC_ALLOW_ANONYMOUS_REMOTE_BROKER"));
        assert!(msg.contains("[[broker.clients]]"));

        std::env::set_var("HC_ALLOW_ANONYMOUS_REMOTE_BROKER", "1");
        let result = Broker::new(cfg("0.0.0.0", false)).validate_safety();
        std::env::remove_var("HC_ALLOW_ANONYMOUS_REMOTE_BROKER");
        result.expect("override env should bypass the safety check");
    }
}
