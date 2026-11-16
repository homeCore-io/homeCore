//! Plugin runtimes: the logic behind adding one and letting it log in.
//!
//! An admin adds a runtime in homeCore, which makes its login on the spot and
//! hands back a connection key once. Nothing asks to join and nothing waits for
//! approval: the runtime is a container the operator runs themselves, and adding
//! it here is the approval.
//!
//! This module is decisions and checks only, with no HTTP. See
//! `docs/pluginRuntimesPlan.md`.

use anyhow::{anyhow, bail, Context, Result};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The oldest runtime host this homeCore works with. 0.2.0 is the first that
/// connects with a key; an older one cannot reach this homeCore at all.
/// Raise it when core needs something a runtime only does from a later version.
pub const MIN_HOST_VERSION: &str = "0.2.0";

/// True when a runtime reported a host older than [`MIN_HOST_VERSION`]. A
/// runtime that has not reported one yet is not called outdated.
pub fn is_outdated(host_version: Option<&str>) -> bool {
    host_version.is_some_and(|v| {
        crate::registry::compare_versions(v, MIN_HOST_VERSION) == std::cmp::Ordering::Less
    })
}

/// A new runtime id: `rt-` and eight hex digits.
pub fn new_runtime_id() -> Result<String> {
    let mut raw = [0u8; 4];
    getrandom::fill(&mut raw).map_err(|e| anyhow!("OS RNG unavailable: {e}"))?;
    Ok(format!("rt-{}", hex::encode(raw)))
}

/// The plugin id a runtime registers under, which is also its broker username.
///
/// Derived from the kind and the id so it is stable and legible in a plugin
/// list: `plugin.python-rt4f2a9c17`.
pub fn plugin_id_for(kind: &str, runtime_id: &str) -> Result<String> {
    let kind = kind.trim().to_lowercase();
    if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("runtime kind must be alphanumeric");
    }
    let short: String = runtime_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if short.is_empty() {
        bail!("runtime_id has no usable characters");
    }
    Ok(format!("plugin.{kind}-{short}"))
}

/// Mint the MQTT password a plugin placed on a runtime connects with.
///
/// Same shape as the credential `plugin_install` mints for a binary plugin.
pub fn mint_mqtt_password() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The address the admin gave for reaching homeCore, checked and tidied.
///
/// It goes into the key verbatim, so a typo here is a runtime that cannot
/// connect. Better to refuse it while the admin is still looking at the form.
pub fn check_homecore_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    let parsed = reqwest::Url::parse(url).with_context(|| {
        format!("{url} is not an address; write it like http://homecore.local:8080")
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        bail!("the address must start with http:// or https://");
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        bail!("the address needs a host name or IP");
    }
    Ok(url.to_string())
}

fn bare(host: &str) -> &str {
    host.trim().trim_start_matches('[').trim_end_matches(']')
}

fn is_unspecified(host: &str) -> bool {
    let h = bare(host);
    h.is_empty() || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_unspecified())
}

/// True when the broker answers only on homeCore's own machine.
pub fn is_loopback(host: &str) -> bool {
    let h = bare(host);
    h.eq_ignore_ascii_case("localhost") || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The broker host a runtime should dial, for the key.
///
/// `broker_host` is what core knows: the embedded broker's listen address, or
/// an external broker's address. A listen address like `0.0.0.0` names no
/// machine, and loopback only works from homeCore's own network, so in both
/// cases the runtime dials the host it reaches homeCore at. Any other address
/// is a real one and is used as it is.
pub fn broker_host_for_key(broker_host: &str, homecore_url: &str) -> String {
    if is_unspecified(broker_host) || is_loopback(broker_host) {
        reqwest::Url::parse(homecore_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| bare(h).to_string()))
            .unwrap_or_else(|| bare(broker_host).to_string())
    } else {
        bare(broker_host).to_string()
    }
}

/// Open the embedded broker to the network, in the text of `homecore.toml`.
///
/// Sets `[broker].host = "0.0.0.0"` and, when no `[[broker.clients]]` exist,
/// adds one for core itself with a new password: core refuses to start a
/// network broker with no logins, and once there are logins core's own client
/// needs one. Comments and everything else in the file are kept. Returns the
/// new text; the caller writes it and the change takes a restart.
pub fn open_broker_in_config(text: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text.parse().context("homecore.toml does not parse")?;
    if doc.get("broker").is_none() {
        doc["broker"] = toml_edit::table();
    }
    let broker = doc["broker"]
        .as_table_mut()
        .ok_or_else(|| anyhow!("[broker] in homecore.toml is not a table"))?;
    broker["host"] = toml_edit::value("0.0.0.0");

    let has_clients = broker
        .get("clients")
        .and_then(|c| c.as_array_of_tables())
        .is_some_and(|a| !a.is_empty());
    if !has_clients {
        let mut client = toml_edit::Table::new();
        client["id"] = toml_edit::value("internal.core");
        client["password"] = toml_edit::value(mint_mqtt_password());
        client["allow_pub"] = toml_edit::value(toml_edit::Array::from_iter(["homecore/#"]));
        client["allow_sub"] = toml_edit::value(toml_edit::Array::from_iter(["homecore/#"]));
        let mut clients = toml_edit::ArrayOfTables::new();
        clients.push(client);
        broker.insert("clients", toml_edit::Item::ArrayOfTables(clients));
    }
    Ok(doc.to_string())
}

/// Every login core makes, checked when someone connects to the broker.
///
/// Two kinds, both answered from what is true now, so a login works the moment
/// it is made and stops when it is removed:
///
/// - a runtime, by its plugin id and its one secret (hashed in the store);
/// - a plugin core installed or placed on a runtime, by the `[homecore]`
///   `password` in its config file. That file is where core wrote the password
///   when it made it, and deleting the plugin deletes the file.
#[derive(Clone)]
pub struct BrokerLogins {
    pub runtimes: Arc<hc_state::PluginRuntimeStore>,
    /// `config/plugins`, where core keeps the config of plugins it installs or
    /// places.
    pub config_plugins_dir: PathBuf,
    /// Plugins listed in `homecore.toml` with their own config path.
    pub listed: Vec<(String, PathBuf)>,
}

impl BrokerLogins {
    pub fn accepts(&self, user: &str, pass: &str) -> bool {
        if pass.is_empty() {
            return false;
        }
        if let Ok(Some(rt)) = self.runtimes.by_plugin_id(user) {
            return hc_auth::api_key::verify_token(pass, &rt.key_hash);
        }
        match self.config_for(user) {
            Some(path) => {
                password_in(&path).is_some_and(|p| same_bytes(p.as_bytes(), pass.as_bytes()))
            }
            None => false,
        }
    }

    fn config_for(&self, plugin_id: &str) -> Option<PathBuf> {
        if let Some((_, path)) = self.listed.iter().find(|(id, _)| id == plugin_id) {
            return Some(path.clone());
        }
        // The username arrives from the network: it may name a file in
        // config/plugins and nothing else.
        let safe = !plugin_id.is_empty()
            && plugin_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            && !plugin_id.starts_with('.');
        safe.then(|| self.config_plugins_dir.join(format!("{plugin_id}.toml")))
    }
}

/// Constant-time equality, so a refused login says nothing about how close it was.
fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The `[homecore] password` in a plugin's config, if it has one.
fn password_in(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: toml::Value = toml::from_str(&text).ok()?;
    doc.get("homecore")?
        .get("password")?
        .as_str()
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_host_is_outdated_and_an_unknown_one_is_not() {
        assert!(is_outdated(Some("0.1.0")));
        assert!(!is_outdated(Some("0.2.0")));
        assert!(!is_outdated(Some("0.10.0")));
        assert!(!is_outdated(None));
    }

    #[test]
    fn a_runtime_id_and_its_plugin_id() {
        let id = new_runtime_id().unwrap();
        assert!(id.starts_with("rt-") && id.len() == 11, "{id}");
        assert_eq!(
            plugin_id_for("python", "rt-4f2a9c17").unwrap(),
            "plugin.python-rt4f2a9c17"
        );
    }

    #[test]
    fn a_hostile_kind_cannot_forge_a_plugin_id() {
        assert!(plugin_id_for("python/../hue", "rt-abc").is_err());
        assert!(plugin_id_for("", "rt-abc").is_err());
    }

    #[test]
    fn the_homecore_address_is_checked_before_it_goes_in_a_key() {
        assert_eq!(
            check_homecore_url(" http://homecore.local:8080/ ").unwrap(),
            "http://homecore.local:8080"
        );
        assert!(check_homecore_url("homecore.local:8080").is_err());
        assert!(check_homecore_url("ftp://homecore.local").is_err());
    }

    #[test]
    fn the_key_names_a_broker_the_runtime_can_dial() {
        let url = "http://homecore.local:8080";
        assert_eq!(broker_host_for_key("0.0.0.0", url), "homecore.local");
        assert_eq!(broker_host_for_key("127.0.0.1", url), "homecore.local");
        // An external broker, or a broker bound to one address, is a real place.
        assert_eq!(broker_host_for_key("10.0.0.9", url), "10.0.0.9");
        assert_eq!(broker_host_for_key("mqtt.lan", url), "mqtt.lan");
        assert_eq!(
            broker_host_for_key("0.0.0.0", "http://[fd00::5]:8080"),
            "fd00::5"
        );
    }

    #[test]
    fn opening_the_broker_adds_a_login_for_core_when_there_is_none() {
        let before =
            "# my house\n[server]\nport = 8080\n\n[broker]\nhost = \"127.0.0.1\"\nport = 1883\n";
        let after = open_broker_in_config(before).unwrap();
        assert!(after.starts_with("# my house"), "{after}");
        let doc: toml::Value = toml::from_str(&after).unwrap();
        assert_eq!(doc["broker"]["host"].as_str(), Some("0.0.0.0"));
        assert_eq!(doc["broker"]["port"].as_integer(), Some(1883));
        let clients = doc["broker"]["clients"].as_array().unwrap();
        assert_eq!(clients[0]["id"].as_str(), Some("internal.core"));
        assert_eq!(clients[0]["password"].as_str().unwrap().len(), 32);
    }

    #[test]
    fn opening_the_broker_keeps_the_clients_already_there() {
        let before = "[broker]\nhost = \"127.0.0.1\"\n\n[[broker.clients]]\nid = \"internal.core\"\npassword = \"mine\"\n";
        let doc: toml::Value = toml::from_str(&open_broker_in_config(before).unwrap()).unwrap();
        let clients = doc["broker"]["clients"].as_array().unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0]["password"].as_str(), Some("mine"));
    }

    fn logins(dir: &Path) -> BrokerLogins {
        let db = Arc::new(redb::Database::create(dir.join("t.redb")).unwrap());
        BrokerLogins {
            runtimes: Arc::new(hc_state::PluginRuntimeStore::new(db).unwrap()),
            config_plugins_dir: dir.join("plugins"),
            listed: vec![("plugin.listed".into(), dir.join("listed.toml"))],
        }
    }

    #[test]
    fn a_runtime_logs_in_with_its_secret() {
        let dir = tempfile::tempdir().unwrap();
        let l = logins(dir.path());
        let key = hc_auth::api_key::generate().unwrap();
        l.runtimes
            .upsert(&hc_state::RuntimeRecord {
                runtime_id: "rt-1".into(),
                name: "R".into(),
                kind: "python".into(),
                abi: "cp312-manylinux_2_28".into(),
                plugin_id: "plugin.python-rt1".into(),
                key_hash: key.hash.clone(),
                homecore_url: "http://h:8080".into(),
                max_plugins: None,
                created_at: chrono::Utc::now(),
                arch: None,
                host_version: None,
                hostname: None,
                network_mode: None,
                source_ip: None,
                last_seen_at: None,
            })
            .unwrap();
        assert!(l.accepts("plugin.python-rt1", &key.full_token));
        assert!(!l.accepts("plugin.python-rt1", "wrong"));
    }

    #[test]
    fn a_plugin_logs_in_with_the_password_in_its_config() {
        let dir = tempfile::tempdir().unwrap();
        let l = logins(dir.path());
        std::fs::create_dir_all(dir.path().join("plugins")).unwrap();
        std::fs::write(
            dir.path().join("plugins/plugin.demo.toml"),
            "[homecore]\nplugin_id = \"plugin.demo\"\npassword = \"p1\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("listed.toml"),
            "[homecore]\npassword = \"p2\"\n",
        )
        .unwrap();
        assert!(l.accepts("plugin.demo", "p1"));
        assert!(!l.accepts("plugin.demo", "p2"));
        assert!(l.accepts("plugin.listed", "p2"));
        // Removed means its file is gone, and so is the login.
        std::fs::remove_file(dir.path().join("plugins/plugin.demo.toml")).unwrap();
        assert!(!l.accepts("plugin.demo", "p1"));
    }

    #[test]
    fn a_username_cannot_reach_outside_config_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let l = logins(dir.path());
        std::fs::write(
            dir.path().join("listed.toml"),
            "[homecore]\npassword = \"p\"\n",
        )
        .unwrap();
        assert!(!l.accepts("../listed", "p"));
        assert!(!l.accepts("..", "p"));
        assert!(!l.accepts("plugin.none", ""));
    }
}
