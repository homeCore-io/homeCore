use anyhow::{Context, Result};
use serde::Deserialize;

/// The operator-config JSON Schema, published on the capability manifest so the
/// hc-web editor renders a typed form. `None` when built without `schema`.
#[cfg(feature = "schema")]
pub fn config_schema() -> Option<serde_json::Value> {
    serde_json::to_value(schemars::schema_for!(WledConfig)).ok()
}

#[cfg(not(feature = "schema"))]
pub fn config_schema() -> Option<serde_json::Value> {
    None
}

/// The plugin's own **config descriptor** — how this configuration should be
/// presented, which a JSON Schema cannot express: units, live data sources, and
/// prose. Published on the capability manifest; core serves it at
/// `GET /plugins/{id}/config/descriptor` and the editor renders it directly.
///
/// Coverage note (phase 6): a published descriptor is authoritative, so an
/// omitted key is uneditable. The Devices section binds to the **live device
/// registry** rather than this file's `[[devices]]` array — naming and room
/// assignment belong to the registry (core owns inventory, per the
/// device-identity model), so those edits go to `/devices`.
///
/// That leaves the array's own keys unreachable from the form: `host` and
/// `hc_id` (written by mDNS discovery and the Discover action) and the
/// per-device `poll_interval_secs` override, where the global poll interval
/// covers the common case. The plugin *does* read all three, so correcting a
/// stale host means editing TOML — a real gap, listed as justified in
/// `descriptor_covers_every_schema_field`. `homecore.plugin_id` is bootstrap
/// identity fixed at install.
pub fn config_descriptor() -> serde_json::Value {
    use plugin_sdk_rs::config_descriptor::{Descriptor, Field, Section, Source};

    Descriptor::new("plugin.wled")
        .title("WLED")
        .section(
            Section::new("discovery", "Discovery")
                .field(
                    Field::duration("wled.poll_interval_secs")
                        .label("Poll interval")
                        .unit("secs")
                        .default(30)
                        .min(1)
                        .help("WLED pushes changes over its WebSocket as they happen. This is how long to wait before reconnecting when that drops, and how often to read the state of a WLED that has no WebSocket."),
                )
                .field(
                    Field::enumeration("wled.firmware_channel")
                        .label("Firmware updates")
                        .render("segmented")
                        .default("stable")
                        .help("Which releases count as an update. Betas and nightly builds are for trying what's next.")
                        .option("stable", "Stable")
                        .option("beta", "Stable and betas")
                        .option("nightly", "Nightly builds too"),
                )
                .field(
                    Field::int("wled.firmware_check_hours")
                        .label("Look for firmware every")
                        .unit("hours")
                        .default(24)
                        .min(0)
                        .help("0 never looks. Each look is one request to GitHub per firmware source."),
                )
                .field(
                    Field::enumeration("wled.catalog")
                        .label("Effect and palette catalogue")
                        .render("segmented")
                        .default("controller")
                        .help("For new controllers. Read from the controller is always current; a copy in the app works while a controller is offline.")
                        .option("controller", "Read from the controller")
                        .option("app", "Keep a copy in the app"),
                )
                .field(
                    Field::text("wled.backups_dir")
                        .label("Backups folder")
                        .default("backups")
                        .help("Where controller backups are kept — taken before every firmware install, and on request. Relative to this plugin's config folder."),
                )
                .field(
                    Field::list("wled.discovery_hosts", "host")
                        .label("Cross-subnet hosts")
                        .default(Vec::<String>::new())
                        .help(
                            "Local WLEDs are found automatically over mDNS, which \
                             can't cross VLANs. List an IP/hostname here only to \
                             reach a WLED on another subnet — leave empty on a flat \
                             network.",
                        ),
                ),
        )
        .section(
            Section::new("devices", "Devices").field(
                Field::table("devices")
                    .label("WLED devices")
                    .render("cards")
                    .key_by("device_id")
                    .help("Every WLED found by mDNS or the Discover action — set its name and room.")
                    .source(
                        Source::core_resource("devices")
                            .item_key("device_id")
                            .labels("name", "device_id"),
                    )
                    .columns([
                        Field::text("name").label("Name"),
                        Field::select("area")
                            .label("Room")
                            .placeholder("Unassigned")
                            .allow_create()
                            .source(Source::core_resource("areas")),
                    ]),
            ),
        )
        .section(
            Section::new("logging", "Logging")
                .field(
                    Field::text("logging.level")
                        .label("Level")
                        .default("info")
                        .placeholder("info | debug | hc_wled=debug"),
                )
                .field(
                    Field::enumeration("logging.log_forward_level")
                        .label("Forward to core")
                        .render("segmented")
                        .default("info")
                        .help(
                            "Minimum level forwarded to homeCore over MQTT; \
                             anything below is written locally only.",
                        )
                        .option("off", "Off")
                        .option("error", "Error")
                        .option("warn", "Warn")
                        .option("info", "Info")
                        .option("debug", "Debug"),
                )
                .field(
                    Field::enumeration("logging.rotation")
                        .label("Rotate")
                        .render("segmented")
                        .default("daily")
                        .option("hourly", "Hourly")
                        .option("daily", "Daily")
                        .option("weekly", "Weekly")
                        .option("never", "Never"),
                )
                .field(
                    Field::int("logging.max_size_mb")
                        .label("Rotate at size")
                        .unit("MB")
                        .default(100)
                        .min(0)
                        .help("Whichever comes first, this or the schedule. 0 disables size-based rotation."),
                )
                .field(
                    Field::int("logging.prune_after_days")
                        .label("Prune after")
                        .unit("days")
                        .default(0)
                        .min(0)
                        .help("Delete rotated files older than this. 0 = never prune."),
                )
                .field(
                    Field::toggle("logging.compress")
                        .label("Compress rotated files")
                        .default(true),
                ),
        )
        .section(
            Section::new("connection", "Connection")
                .hidden()
                .field(Field::host("homecore.broker_host").label("Broker host"))
                .field(Field::port("homecore.broker_port").label("Broker port"))
                .field(Field::secret("homecore.password").label("Broker password")),
        )
        .build()
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WledConfig {
    /// Optional, like every field inside it.
    ///
    /// It was mandatory while every field it holds already had a default, so
    /// the only thing a config could be missing was the section header — and
    /// missing it was fatal: "missing field `homecore`", exit 1, before the
    /// plugin could log or attempt anything.
    ///
    /// That took hc-zwave down on a real house for the better part of an hour
    /// after an editor wrote a config containing only the fields its form
    /// covered. A plugin with a working default for every value in a section
    /// should not be one dropped section away from a restart loop.
    #[serde(default)]
    pub homecore: HomecoreConfig,
    #[serde(default)]
    pub logging: plugin_sdk_rs::logging::LoggingConfig,
    #[serde(default)]
    pub wled: WledGlobalConfig,
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HomecoreConfig {
    #[serde(default = "default_host")]
    pub broker_host: String,
    #[serde(default = "default_port")]
    pub broker_port: u16,
    #[serde(default = "default_plugin_id")]
    pub plugin_id: String,
    #[serde(default)]
    pub password: String,
}

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    1883
}
fn default_plugin_id() -> String {
    "plugin.wled".into()
}

impl Default for HomecoreConfig {
    fn default() -> Self {
        Self {
            broker_host: default_host(),
            broker_port: default_port(),
            plugin_id: default_plugin_id(),
            password: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WledGlobalConfig {
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    /// OPTIONAL cross-subnet fallback for `discover_devices`. Local-subnet
    /// WLEDs are found automatically over mDNS (`_wled._tcp.local`), which is
    /// link-local and cannot cross VLANs — so list an IP/hostname here only to
    /// reach a WLED on a subnet the homeCore host can route to but mDNS can't.
    /// Each listed host is queried once via `/json/nodes` and its WLED-Sync
    /// peer list is merged into the result. Leave empty on a flat network.
    #[serde(default)]
    pub discovery_hosts: Vec<String>,
    /// Which firmware releases count as an update: `stable`, `beta`
    /// (pre-releases too) or `nightly`.
    #[serde(default = "default_channel")]
    pub firmware_channel: String,
    /// How often to look for new firmware, in hours. 0 never looks.
    #[serde(default = "default_check_hours")]
    pub firmware_check_hours: u64,
    /// Where a client keeps a controller's effect and palette catalogue:
    /// `controller` (read from it when needed) or `app` (a copy, refreshed
    /// when its firmware changes). Per controller, `devices[].catalog`.
    #[serde(default = "default_catalog")]
    pub catalog: String,
    /// Where backups go, relative to this file's folder unless absolute.
    #[serde(default = "default_backups")]
    pub backups_dir: String,
}

fn default_poll() -> u64 {
    30
}
fn default_channel() -> String {
    "stable".into()
}
fn default_check_hours() -> u64 {
    24
}
fn default_catalog() -> String {
    "controller".into()
}
fn default_backups() -> String {
    "backups".into()
}

impl Default for WledGlobalConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll(),
            discovery_hosts: Vec::new(),
            firmware_channel: default_channel(),
            firmware_check_hours: default_check_hours(),
            catalog: default_catalog(),
            backups_dir: default_backups(),
        }
    }
}

impl WledGlobalConfig {
    pub fn firmware_channel(&self) -> crate::firmware::Channel {
        crate::firmware::Channel::parse(&self.firmware_channel)
    }

    pub fn backups_path(&self, config_path: &str) -> std::path::PathBuf {
        let p = std::path::Path::new(&self.backups_dir);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::path::Path::new(config_path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(p)
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceConfig {
    /// IP address or hostname of the WLED device.
    pub host: String,
    /// Stable homeCore device ID.
    pub hc_id: String,
    /// Human-readable display name.
    pub name: String,
    #[serde(default)]
    pub area: Option<String>,
    /// Per-device polling interval override (seconds).
    #[serde(default)]
    pub poll_interval_secs: Option<u64>,
    /// This controller's catalogue: `controller` or `app` (see
    /// `[wled].catalog`, the default).
    #[serde(default)]
    pub catalog: Option<String>,
}

impl WledConfig {
    pub fn load(path: &str) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading config: {path}"))?;
        toml::from_str(&content).with_context(|| format!("parsing config: {path}"))
    }
}
#[cfg(all(test, feature = "schema"))]
mod tests {

    /// A config with no `[homecore]` section must still load.
    ///
    /// It used to be fatal — "missing field `homecore`", exit 1, before the
    /// plugin could log or attempt anything — while every value in the section
    /// already had a working default. hc-zwave spent the better part of an hour
    /// in a 60-second restart loop over exactly this, after an editor wrote a
    /// config containing only the fields its form covered.
    #[test]
    fn a_config_without_the_homecore_section_still_loads() {
        let cfg: WledConfig = toml::from_str("").expect("[homecore] must be optional");
        assert_eq!(cfg.homecore.broker_host, "127.0.0.1");
        assert_eq!(cfg.homecore.broker_port, 1883);
        assert_eq!(cfg.homecore.plugin_id, "plugin.wled");
    }

    /// ...and an explicit section still wins, which is how this kind of fix
    /// usually goes wrong.
    #[test]
    fn an_explicit_homecore_section_beats_the_defaults() {
        let cfg: WledConfig = toml::from_str("[homecore]\nbroker_host = \"10.0.0.5\"\n").unwrap();
        assert_eq!(cfg.homecore.broker_host, "10.0.0.5");
        assert_eq!(cfg.homecore.plugin_id, "plugin.wled");
    }
    use super::*;

    /// A published descriptor is *authoritative* — the editor renders it
    /// instead of deriving from the schema — so any omitted config field
    /// becomes uneditable (the hc-sonos logging bug, `5bccebf`). The check
    /// lives in the SDK; every schema leaf must be covered or justified.
    #[test]
    fn descriptor_covers_every_schema_field() {
        let missing = plugin_sdk_rs::config_descriptor::missing_schema_coverage(
            &config_schema().expect("schema feature is on"),
            &config_descriptor(),
            &[
                // Bootstrap identity fixed at install, not an operator setting.
                "homecore.plugin_id",
                // The Devices table binds to the live registry, so naming and
                // room assignment go to core (which owns inventory) and never
                // reach this file's [[devices]] array. What remains there is
                // written by mDNS discovery and the Discover action:
                //
                //   host, hc_id          — discovered address and pinned identity
                //   poll_interval_secs   — per-device override, TOML-only today
                //
                // The plugin does read them (bridge.rs, main.rs), so this is a
                // genuine editing gap rather than dead config: correcting a
                // stale host means editing TOML. Revisit if that bites.
                "devices[].host",
                "devices[].hc_id",
                "devices[].poll_interval_secs",
                // Set per controller by the `update_device` action, from its
                // page; the default is `wled.catalog`.
                "devices[].catalog",
            ],
        );
        assert!(
            missing.is_empty(),
            "config fields missing from the descriptor: {missing:?}"
        );
    }
}

/// Changing the `[[devices]]` this file lists — adding a controller, moving
/// one to a new address, removing one — with the rest of the file left as it
/// was written. The plugin reads its devices when it starts, so each change
/// takes effect once it is restarted.
pub mod devices {
    use anyhow::{bail, Context, Result};
    use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

    fn load(path: &str) -> Result<DocumentMut> {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        text.parse::<DocumentMut>()
            .with_context(|| format!("parsing {path}"))
    }

    fn save(path: &str, doc: &DocumentMut) -> Result<()> {
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, doc.to_string())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn list(doc: &mut DocumentMut) -> &mut ArrayOfTables {
        if !doc.contains_key("devices") {
            doc.insert("devices", Item::ArrayOfTables(ArrayOfTables::new()));
        }
        doc["devices"]
            .as_array_of_tables_mut()
            .expect("devices is an array of tables")
    }

    fn find(list: &ArrayOfTables, hc_id: &str) -> Option<usize> {
        list.iter()
            .position(|t| t.get("hc_id").and_then(|v| v.as_str()) == Some(hc_id))
    }

    /// Add a controller. Its id is fixed from here on; adding one that is
    /// there already (by id or address) is refused.
    pub fn add(path: &str, hc_id: &str, host: &str, name: &str, area: Option<&str>) -> Result<()> {
        let mut doc = load(path)?;
        let l = list(&mut doc);
        if find(l, hc_id).is_some() {
            bail!("{hc_id} is already one of this plugin's controllers");
        }
        if l.iter()
            .any(|t| t.get("host").and_then(|v| v.as_str()) == Some(host))
        {
            bail!("{host} is already one of this plugin's controllers");
        }
        let mut t = Table::new();
        t.insert("host", value(host));
        t.insert("hc_id", value(hc_id));
        t.insert("name", value(name));
        if let Some(a) = area.filter(|a| !a.is_empty()) {
            t.insert("area", value(a));
        }
        l.push(t);
        save(path, &doc)
    }

    /// Change what can be changed about a controller from its page: where it
    /// is, how often it is polled without a WebSocket, where its catalogue is
    /// kept. `Some(None)` clears a setting.
    pub fn update(
        path: &str,
        hc_id: &str,
        host: Option<&str>,
        poll_interval_secs: Option<Option<u64>>,
        catalog: Option<Option<&str>>,
    ) -> Result<()> {
        let mut doc = load(path)?;
        let l = list(&mut doc);
        let i = find(l, hc_id).with_context(|| format!("no controller is {hc_id}"))?;
        let t = l.get_mut(i).expect("found");
        if let Some(h) = host {
            let h = h.trim();
            if h.is_empty() || h.contains(char::is_whitespace) {
                bail!("`{h}` isn't an address");
            }
            t.insert("host", value(h));
        }
        match poll_interval_secs {
            Some(Some(s)) => {
                t.insert("poll_interval_secs", value(s as i64));
            }
            Some(None) => {
                t.remove("poll_interval_secs");
            }
            None => {}
        }
        match catalog {
            Some(Some(c)) if c == "controller" || c == "app" => {
                t.insert("catalog", value(c));
            }
            Some(Some(c)) => {
                bail!("the catalogue is kept on the `controller` or in the `app`, not `{c}`")
            }
            Some(None) => {
                t.remove("catalog");
            }
            None => {}
        }
        save(path, &doc)
    }

    /// Stop looking after a controller. Whether it was there.
    pub fn remove(path: &str, hc_id: &str) -> Result<bool> {
        let mut doc = load(path)?;
        let l = list(&mut doc);
        let Some(i) = find(l, hc_id) else {
            return Ok(false);
        };
        l.remove(i);
        save(path, &doc)?;
        Ok(true)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_controller_is_added_moved_and_removed_and_the_rest_is_kept() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            let path = path.to_str().unwrap();
            std::fs::write(
                path,
                "# my WLEDs\n[wled]\npoll_interval_secs = 30 # half a minute\n\n[[devices]]\nhost = \"10.0.10.74\"\nhc_id = \"wled_6ace04\"\nname = \"wled-6ace04\"\n",
            )
            .unwrap();
            add(
                path,
                "wled_garage",
                "10.0.10.80",
                "Garage strip",
                Some("garage"),
            )
            .unwrap();
            assert!(add(path, "wled_x", "10.0.10.80", "Again", None).is_err());
            update(
                path,
                "wled_garage",
                Some("10.0.10.81"),
                Some(Some(15)),
                Some(Some("app")),
            )
            .unwrap();
            assert!(update(path, "wled_garage", None, None, Some(Some("cloud"))).is_err());
            let cfg: crate::config::WledConfig =
                toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            assert_eq!(cfg.devices.len(), 2);
            assert_eq!(cfg.devices[1].host, "10.0.10.81");
            assert_eq!(cfg.devices[1].poll_interval_secs, Some(15));
            assert_eq!(cfg.devices[1].catalog.as_deref(), Some("app"));
            assert_eq!(cfg.devices[1].area.as_deref(), Some("garage"));
            assert!(remove(path, "wled_garage").unwrap());
            assert!(!remove(path, "wled_garage").unwrap());
            let text = std::fs::read_to_string(path).unwrap();
            assert!(
                text.contains("# my WLEDs") && text.contains("# half a minute"),
                "{text}"
            );
        }
    }
}
