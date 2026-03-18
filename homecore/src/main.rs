use anyhow::Result;
use hc_api::AppState;
use hc_auth::{hash_password, JwtService, Role, User};
use hc_broker::{Broker, BrokerConfig, ClientAcl};
use hc_core::{Core, EventBus};
use hc_mqtt_client::{MqttClient, MqttClientConfig};
use hc_notify::{ChannelConfig, NotificationService};
use hc_state::StateStore;
use hc_topic_map::{TopicMapEntry, TopicMapper, BUILTIN_TRANSFORMS};
use serde::Deserialize;
use tracing::info;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

/// Top-level config shape (subset — just what main.rs needs to parse).
#[derive(Deserialize, Default)]
struct AppConfig {
    #[serde(default)]
    broker: BrokerSection,
    #[serde(default)]
    location: LocationSection,
    #[serde(rename = "topic_map", default)]
    topic_map: Vec<TopicMapEntry>,
    #[serde(default)]
    auth: AuthSection,
    #[serde(default)]
    notify: NotifySection,
}

/// `[broker]` section of homecore.toml.
#[derive(Deserialize)]
struct BrokerSection {
    #[serde(default = "default_broker_host")]
    host: String,
    #[serde(default = "default_broker_port")]
    port: u16,
    tls_port: Option<u16>,
    cert_path: Option<String>,
    key_path: Option<String>,
    /// Per-client credentials. When any entries are present the broker
    /// requires authentication on all connections.
    #[serde(default)]
    clients: Vec<ClientAclConfig>,
}

impl Default for BrokerSection {
    fn default() -> Self {
        Self {
            host: default_broker_host(),
            port: default_broker_port(),
            tls_port: None,
            cert_path: None,
            key_path: None,
            clients: vec![],
        }
    }
}

fn default_broker_host() -> String { "0.0.0.0".into() }
fn default_broker_port() -> u16 { 1883 }

/// A single `[[broker.clients]]` entry.
#[derive(Deserialize, Clone)]
struct ClientAclConfig {
    id: String,
    password: String,
    #[serde(default)]
    allow_pub: Vec<String>,
    #[serde(default)]
    allow_sub: Vec<String>,
}

#[derive(Deserialize, Default)]
struct NotifySection {
    #[serde(default)]
    channels: Vec<ChannelConfig>,
}

#[derive(Deserialize)]
struct LocationSection {
    latitude: f64,
    longitude: f64,
}

impl Default for LocationSection {
    fn default() -> Self {
        Self { latitude: 38.9072, longitude: -77.0369 }
    }
}

#[derive(Deserialize)]
struct AuthSection {
    /// HMAC-SHA256 secret for signing JWTs. If not set, a random secret is
    /// generated at startup (tokens will be invalidated on restart).
    jwt_secret: Option<String>,
    #[serde(default = "default_expiry")]
    token_expiry_hours: u64,
}

fn default_expiry() -> u64 { 24 }

impl Default for AuthSection {
    fn default() -> Self {
        Self { jwt_secret: None, token_expiry_hours: 24 }
    }
}

/// Generate a random alphanumeric password of the given length.
fn random_password(len: usize) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    // Use a mix of time + process ID as a simple source of entropy for the
    // bootstrap password.  This is not cryptographically strong, but it only
    // needs to be human-typable for the first login before the admin changes it.
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(42);
    let pid = std::process::id() as u128;

    let charset: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
    let mut result = String::with_capacity(len);
    let mut state = seed ^ (pid << 32);
    for i in 0..len {
        let mut h = DefaultHasher::new();
        (state ^ (i as u128 * 0x9e3779b97f4a7c15)).hash(&mut h);
        state = h.finish() as u128;
        result.push(charset[state as usize % charset.len()] as char);
    }
    result
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    info!("HomeCore starting");

    // Load optional config file.
    let config: AppConfig = std::fs::read_to_string("config/homecore.toml")
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();

    // 1. Embedded MQTT broker.
    let broker_cfg = BrokerConfig {
        host:      config.broker.host.clone(),
        port:      config.broker.port,
        tls_port:  config.broker.tls_port,
        cert_path: config.broker.cert_path.clone(),
        key_path:  config.broker.key_path.clone(),
        clients:   config.broker.clients.iter().map(|c| ClientAcl {
            client_id:  c.id.clone(),
            password:   c.password.clone(),
            allow_pub:  c.allow_pub.clone(),
            allow_sub:  c.allow_sub.clone(),
        }).collect(),
    };
    Broker::new(broker_cfg).spawn()?;

    // 2. Internal MQTT client.
    // If broker auth is configured, find the "internal.core" credential and use it.
    let internal_cred = config.broker.clients.iter()
        .find(|c| c.id == "internal.core")
        .cloned();
    let mqtt_cfg = MqttClientConfig {
        broker_host: "127.0.0.1".into(),
        broker_port: config.broker.port,
        client_id:   "internal.core".into(),
        username:    internal_cred.as_ref().map(|c| c.id.clone()),
        password:    internal_cred.as_ref().map(|c| c.password.clone()),
    };
    let (mqtt_client, mut mqtt_rx) = MqttClient::new(mqtt_cfg);
    let publish_handle = mqtt_client.publish_handle();

    // 3. State store.
    let store = StateStore::open("/tmp/homecore-state.redb", "/tmp/homecore-history.db").await?;

    // 4. Event bus — the shared backbone all crates communicate through.
    let bus = EventBus::new(1024);

    // 5. Forwarder: MQTT client broadcast → EventBus.
    {
        let bus_clone = bus.clone();
        tokio::spawn(async move {
            loop {
                match mqtt_rx.recv().await {
                    Ok(event) => {
                        let _ = bus_clone.publish(event);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("MQTT→bus forwarder lagged by {n}");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // 6. Drive the MQTT event loop in its own task.
    tokio::spawn(async move {
        if let Err(e) = mqtt_client.run().await {
            tracing::error!(error = %e, "MQTT client exited");
        }
    });

    // 7. Core: state bridge + rule engine.
    let rules = store.list_rules().await?;
    info!(count = rules.len(), "Loaded rules from store");

    let mut core = Core::new(bus.clone(), store.clone(), Some(publish_handle.clone()))
        .with_location(config.location.latitude, config.location.longitude);

    // Wire topic mapper if any entries are configured (includes built-in transforms).
    if !config.topic_map.is_empty() {
        info!(count = config.topic_map.len(), "Loading topic map entries");
        match TopicMapper::new(config.topic_map, Some(BUILTIN_TRANSFORMS)) {
            Ok(mapper) => { core = core.with_mapper(mapper); }
            Err(e) => { tracing::warn!(error = %e, "Topic mapper init failed; running without it"); }
        }
    }

    // Wire notification service if channels are configured.
    if !config.notify.channels.is_empty() {
        let count = config.notify.channels.len();
        let svc = NotificationService::from_configs(config.notify.channels);
        info!(channels = count, registered = svc.channel_names().len(), "Notification service ready");
        core = core.with_notify(svc);
    }

    let rules_handle = core.start(rules).await?;

    // 8. JWT service.
    let jwt_secret = match &config.auth.jwt_secret {
        Some(s) => s.clone(),
        None => {
            tracing::warn!("No jwt_secret configured — generating a random secret. Tokens will not survive restarts.");
            random_password(64)
        }
    };
    let jwt = JwtService::new_hs256(jwt_secret.as_bytes(), config.auth.token_expiry_hours);

    // 9. Bootstrap: create default admin if no users exist.
    let count = store.user_count().await?;
    if count == 0 {
        let password = random_password(16);
        let hash = tokio::task::spawn_blocking({
            let p = password.clone();
            move || hash_password(&p)
        })
        .await??;

        let admin = User {
            id: Uuid::new_v4(),
            username: "admin".to_string(),
            password_hash: hash,
            role: Role::Admin,
            created_at: chrono::Utc::now(),
        };
        store.create_user(&admin).await?;
        tracing::warn!(
            "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
        );
        tracing::warn!("  Default admin account created.");
        tracing::warn!("  Username : admin");
        tracing::warn!("  Password : {password}");
        tracing::warn!("  Change this password immediately after first login!");
        tracing::warn!(
            "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
        );
    }

    // 10. REST + WebSocket API.
    let app_state = AppState::new(store, bus, Some(publish_handle), Some(rules_handle), jwt);
    hc_api::serve("0.0.0.0", 8080, app_state).await?;

    Ok(())
}
