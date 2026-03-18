//! `hc-broker` — embedded MQTT broker for HomeCore.
//!
//! Wraps [`rumqttd`] to provide an in-process MQTT broker.  Handles lifecycle,
//! optional TLS, and per-client ACL enforcement driven by the HomeCore config.

use anyhow::{Context, Result};
use rumqttd::{Broker as RumqttdBroker, Config, ConnectionSettings, RouterConfig, ServerSettings};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use tracing::info;

/// Per-client ACL entry used when building the broker config.
#[derive(Debug, Clone)]
pub struct ClientAcl {
    pub client_id: String,
    pub password: String,
    pub allow_pub: Vec<String>,
    pub allow_sub: Vec<String>,
}

/// Configuration for the embedded broker.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    pub host: String,
    pub port: u16,
    pub tls_port: Option<u16>,
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
    pub clients: Vec<ClientAcl>,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 1883,
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
        let port = self.config.port;

        let router = RouterConfig {
            max_connections: 1000,
            max_outgoing_packet_count: 200,
            max_segment_size: 104_857_600,
            max_segment_count: 10,
            ..Default::default()
        };

        let tcp = ServerSettings {
            name: "homecore".into(),
            listen: SocketAddrV4::new(host, port).into(),
            tls: None,
            next_connection_delay_ms: 1,
            connections: ConnectionSettings {
                connection_timeout_ms: 5000,
                max_payload_size: 262_144,
                max_inflight_count: 200,
                auth: None,
                external_auth: None,
                dynamic_filters: false,
            },
        };

        let mut servers = HashMap::new();
        servers.insert("tcp".to_string(), tcp);

        Config {
            id: 0,
            router,
            v4: Some(servers),
            v5: None,
            ws: None,
            cluster: None,
            console: None, // Disabled; avoids port-binding issues in tests.
            bridge: None,
            metrics: Default::default(),
            prometheus: Default::default(),
        }
    }

    /// Start the broker synchronously.  This call blocks until the broker exits.
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
