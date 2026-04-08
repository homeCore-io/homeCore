//! `hc-broker` — embedded MQTT broker for HomeCore.
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
//! 0.19 does not enforce per-topic publish/subscribe ACL at the broker level**.
//! Topic isolation is achieved instead through the plugin SDK, which only
//! publishes to its own device topics by convention.  For strict topic ACL use
//! an external broker (Mosquitto, EMQX) pointed at via `external_url`.

use anyhow::{Context, Result};
use rumqttd::{
    Broker as RumqttdBroker, Config, ConnectionSettings, RouterConfig, ServerSettings, TlsConfig,
};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use tracing::{info, warn};

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

        let connection_settings = ConnectionSettings {
            connection_timeout_ms: 5000,
            max_payload_size: 262_144,
            max_inflight_count: 200,
            auth: auth.clone(),
            external_auth: None,
            dynamic_filters: false,
        };

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

    /// Start the broker synchronously.  Blocks until the broker exits.
    /// Call [`Broker::spawn`] to run it in a background thread instead.
    pub fn start(self) -> Result<()> {
        let port = self.config.port;
        info!(port, "Embedded MQTT broker starting");
        let mut broker = RumqttdBroker::new(self.build_config());
        broker.start().context("rumqttd broker exited")?;
        Ok(())
    }

    /// Spawn the broker on a dedicated OS thread.  Returns after the broker
    /// has had a brief moment to bind its port.
    pub fn spawn(self) -> Result<()> {
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
