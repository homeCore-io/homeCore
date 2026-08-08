//! `homecore.toml`, described.
//!
//! The same vocabulary every plugin already uses to describe its own config —
//! so Administration → Configuration is rendered by the renderer hc-web ships
//! for the Plugin Studio, not by a second settings UI written by hand.
//!
//! Two rules hold this honest:
//!
//! 1. **Coverage.** A descriptor is authoritative: any key it omits becomes
//!    uneditable, silently. `descriptor_covers_every_config_field` compares
//!    this against the JSON Schema derived from the structs and fails on
//!    anything not described or explicitly justified below.
//! 2. **Serde names, not Rust names.** The key is what serde writes. On
//!    hc-thermostat a descriptor that disagreed with `#[serde(rename)]` saved
//!    cleanly and did nothing, with no error anywhere.
//!
//! Sections are ordered the way someone thinks about their house — network,
//! then where it is, then who can reach it, then where it keeps things — not
//! in the order the structs happen to be declared.

use hc_types::config_descriptor::{Cond, Descriptor, Field, Section};
use serde_json::Value;

/// Keys deliberately not described here, each with a reason. The coverage test
/// takes this list; anything else missing is a failure.
pub const JUSTIFIED_OMISSIONS: &[&str] = &[
    // Owned by the Plugins screen, which installs, upgrades and removes them
    // against the registry. Hand-editing `[[plugins]]` behind that screen's
    // back is how you get a record and a binary that disagree.
    "plugins[].id",
    "plugins[].binary",
    "plugins[].config",
    "plugins[].enabled",
    // Owned by the Notifications screen: `[[notify.channels]]` is an
    // array-of-tables whose shape depends on the channel type, and core's
    // `PUT /system/config` has a dedicated `array_of_tables` mode for it.
    "notify.channels[].name",
    // Never editable inline. Auto-generated on first boot and persisted to
    // `jwt_secret_file` with 0600; putting it in a web form would put the
    // signing key in a browser and in the audit trail.
    "auth.jwt_secret",
    // Meaningless in the two-container model — hc-web serves the UI from its
    // own image. See claude-notes/plans (CI/CD + deployment redesign).
    "web_admin.dist_path",
];

/// The descriptor for `homecore.toml`.
pub fn system_config_descriptor() -> Value {
    Descriptor::new("homecore")
        .title("Configuration")
        .section(server())
        .section(broker())
        .section(location())
        .section(auth())
        .section(admin_uds())
        .section(storage())
        .section(directories())
        .section(battery())
        .section(influx())
        .section(metrics())
        .section(registry())
        .section(plugin_runtimes())
        .section(lifecycle())
        .section(logging())
        .section(logging_stderr())
        .section(logging_file())
        .section(logging_rules_file())
        .section(logging_syslog())
        .section(logging_stream())
        .section(web_admin())
        .build()
}

fn server() -> Section {
    Section::new("server", "Server")
        .help("Where the API and this UI answer.")
        .field(
            Field::host("server.host")
                .label("Host")
                .help("Bind address. 0.0.0.0 = all interfaces.")
                .default("0.0.0.0"),
        )
        .field(
            Field::port("server.port")
                .label("Port")
                .help("TCP port for the API. Default 8080.")
                .default(8080),
        )
}

fn broker() -> Section {
    Section::new("broker", "MQTT broker")
        .help(
            "The embedded broker every plugin connects to. It ships with core; \
             an external Mosquitto is only needed for enforced topic ACLs.",
        )
        .field(
            Field::host("broker.host")
                .label("Host")
                .help(
                    "127.0.0.1 = single-host. 0.0.0.0 = LAN-reachable, which \
                     multi-host plugins need.",
                )
                .default("127.0.0.1"),
        )
        .field(
            Field::port("broker.port")
                .label("Port")
                .help("Default 1883.")
                .default(1883),
        )
        .field(
            Field::port("broker.v5_port")
                .label("MQTT v5 port")
                .help("Optional second listener speaking MQTT 5. Empty = off."),
        )
        .field(
            Field::port("broker.tls_port")
                .label("TLS port")
                .help("Optional TLS listener. Requires a certificate and key below."),
        )
        .field(
            Field::text("broker.cert_path")
                .label("Certificate path")
                .help("PEM certificate for the TLS listener.")
                .render("path")
                .visible_when(Cond::truthy("broker.tls_port")),
        )
        .field(
            Field::text("broker.key_path")
                .label("Key path")
                .help("PEM private key for the TLS listener.")
                .render("path")
                .visible_when(Cond::truthy("broker.tls_port")),
        )
        .field(
            Field::table("broker.clients")
                .label("Client credentials")
                .help(
                    "One row per MQTT client. The embedded broker enforces the \
                     password on CONNECT; the topic patterns are enforced only \
                     when you run against an external Mosquitto.",
                )
                .key_by("id")
                .columns([
                    Field::text("id").label("Client ID"),
                    Field::secret("password").label("Password"),
                    Field::list("allow_pub", "text").label("Publish"),
                    Field::list("allow_sub", "text").label("Subscribe"),
                ]),
        )
}

fn location() -> Section {
    Section::new("location", "Location")
        .help("Sunrise and sunset are computed here, offline. No cloud call.")
        .field(
            Field::number("location.latitude")
                .label("Latitude")
                .help("Decimal degrees. Positive = north.")
                .min(-90.0)
                .max(90.0),
        )
        .field(
            Field::number("location.longitude")
                .label("Longitude")
                .help("Decimal degrees. Positive = east.")
                .min(-180.0)
                .max(180.0),
        )
        .field(
            Field::text("location.timezone")
                .label("Timezone")
                .help("IANA name, e.g. America/New_York.")
                .placeholder("America/New_York"),
        )
}

fn auth() -> Section {
    Section::new("auth", "Authentication")
        .field(
            Field::int("auth.token_expiry_hours")
                .label("Access-token expiry")
                .unit("hours")
                .help("Default 24.")
                .default(24)
                .min(1),
        )
        .field(
            Field::int("auth.refresh_token_expiry_days")
                .label("Refresh-token expiry")
                .unit("days")
                .help("How long a session survives without signing in again. Default 30.")
                .default(30)
                .min(1),
        )
        .field(
            Field::int("auth.audit_retention_days")
                .label("Audit retention")
                .unit("days")
                .help("How far back the audit trail is kept. Default 365.")
                .default(365)
                .min(1),
        )
        .field(
            Field::text("auth.jwt_secret_file")
                .label("JWT secret file")
                .render("path")
                .help(
                    "Default <base_dir>/jwt_secret. The file is generated on \
                     first boot; the secret itself is never editable here.",
                ),
        )
        .field(
            Field::text("auth.initial_admin_password_file")
                .label("Initial admin password file")
                .render("path")
                .help(
                    "First boot only. Default <base_dir>/INITIAL_ADMIN_PASSWORD. \
                     Empty disables the file.",
                ),
        )
        .field(
            Field::list("auth.whitelist", "text")
                .label("IP whitelist (CIDR)")
                .help(
                    "DEPRECATED — prefer the admin socket below. Every address \
                     listed bypasses sign-in entirely and is treated as Admin.",
                ),
        )
}

fn admin_uds() -> Section {
    Section::new("auth.admin_uds", "Admin socket")
        .help("Local admin access over a Unix socket, for hc-cli and scripts on this host.")
        .field(
            Field::toggle("auth.admin_uds.enabled")
                .label("Enabled")
                .help("Listen on a Unix socket for admin operations from this host.")
                .default(false),
        )
        .field(
            Field::text("auth.admin_uds.path")
                .label("Socket path")
                .render("path")
                .help("Default /run/homecore/admin.sock.")
                .visible_when(Cond::truthy("auth.admin_uds.enabled")),
        )
        .field(
            Field::text("auth.admin_uds.group")
                .label("POSIX group")
                .help("Group that owns the socket. Its members can connect.")
                .visible_when(Cond::truthy("auth.admin_uds.enabled")),
        )
        .field(
            Field::text("auth.admin_uds.mode")
                .label("Mode")
                .help("Octal, e.g. 0660.")
                .placeholder("0660")
                .visible_when(Cond::truthy("auth.admin_uds.enabled")),
        )
        .field(
            Field::list("auth.admin_uds.allowed_uids", "int")
                .label("Extra allowed UIDs")
                .help("The process UID is always allowed; these are in addition.")
                .visible_when(Cond::truthy("auth.admin_uds.enabled")),
        )
}

fn storage() -> Section {
    Section::new("storage", "Storage")
        .help("Both files live under the home directory unless you give an absolute path.")
        .field(
            Field::text("storage.state_db_path")
                .label("State database")
                .render("path")
                .help("redb file holding devices, areas and rules. Default <base_dir>/data/state.redb."),
        )
        .field(
            Field::text("storage.history_db_path")
                .label("History database")
                .render("path")
                .help(
                    "SQLite file holding device history. This is the one that \
                     grows. Default <base_dir>/data/history.db.",
                ),
        )
}

fn directories() -> Section {
    Section::new("directories", "Directories")
        .help("Each is watched: change a file and core reloads it without a restart.")
        .field(
            Field::text("rules.dir")
                .label("Rules")
                .render("path")
                .help("Default <base_dir>/rules. Hot-reloaded on change."),
        )
        .field(
            Field::text("profiles.dir")
                .label("Ecosystem profiles")
                .render("path")
                .help("Default <base_dir>/config/profiles."),
        )
        .field(
            Field::text("calendars.dir")
                .label("Calendars")
                .render("path")
                .help("Default <base_dir>/config/calendars."),
        )
        .field(
            Field::int("calendars.expansion_days")
                .label("Recurring-event expansion")
                .unit("days")
                .help("How far forward to expand an RRULE. Default 400.")
                .default(400)
                .min(1),
        )
}

fn battery() -> Section {
    Section::new("battery", "Battery alerts")
        .help("When a battery-powered device counts as low, and who hears about it.")
        .field(
            Field::number("battery.threshold_pct")
                .label("Low threshold")
                .unit("%")
                .help("At or below this, a device is flagged low.")
                .min(0.0)
                .max(100.0),
        )
        .field(
            Field::number("battery.recover_band_pct")
                .label("Recovery band")
                .unit("%")
                .help(
                    "Recovery needs battery above threshold + this, so a device \
                     sitting on the line does not flap.",
                )
                .min(0.0)
                .max(100.0),
        )
        .field(
            Field::text("battery.notify_channel")
                .label("Notify channel")
                .help("A channel name from Notifications. Empty = rule-driven only."),
        )
        .field(
            Field::toggle("battery.notify_on_recovered")
                .label("Notify on recovery")
                .help("Also send when a device climbs back above the threshold."),
        )
}

fn influx() -> Section {
    Section::new("influx", "InfluxDB export")
        .help("Optional. Streams device state to InfluxDB v2 for long-term graphing.")
        .field(
            Field::toggle("influx.enabled")
                .label("Enabled")
                .help("Master switch.")
                .default(false),
        )
        .field(
            Field::url("influx.url")
                .label("URL")
                .help("InfluxDB v2 base URL, e.g. http://10.0.10.20:8086")
                .visible_when(Cond::truthy("influx.enabled")),
        )
        .field(
            Field::secret("influx.token")
                .label("API token")
                .help("Token with write permission to the bucket.")
                .visible_when(Cond::truthy("influx.enabled")),
        )
        .field(
            Field::text("influx.org")
                .label("Org")
                .help("Organization name.")
                .visible_when(Cond::truthy("influx.enabled")),
        )
        .field(
            Field::text("influx.bucket")
                .label("Bucket")
                .help("Target bucket.")
                .visible_when(Cond::truthy("influx.enabled")),
        )
        .field(
            Field::duration("influx.flush_interval_secs")
                .label("Flush interval")
                .unit("secs")
                .help("Longest a point waits before being written. Default 10.")
                .visible_when(Cond::truthy("influx.enabled")),
        )
        .field(
            Field::int("influx.batch_size")
                .label("Batch size")
                .help("Points per write. Default 1000.")
                .visible_when(Cond::truthy("influx.enabled")),
        )
        .field(
            Field::int("influx.channel_capacity")
                .label("Channel capacity")
                .help("Backlog before the oldest points are dropped. Default 10000.")
                .visible_when(Cond::truthy("influx.enabled")),
        )
        .field(
            Field::list("influx.include_devices", "text")
                .label("Include devices")
                .help("Glob per line. Empty exports nothing; use * for everything.")
                .visible_when(Cond::truthy("influx.enabled")),
        )
        .field(
            Field::list("influx.exclude_attributes", "text")
                .label("Exclude attributes")
                .help("Drop noisy attributes such as last_seen or uptime.")
                .visible_when(Cond::truthy("influx.enabled")),
        )
        .field(
            Field::toggle("influx.export_bools")
                .label("Export booleans")
                .help("Emit true/false attributes as 0/1 numeric fields.")
                .visible_when(Cond::truthy("influx.enabled")),
        )
}

fn metrics() -> Section {
    Section::new("metrics", "Prometheus metrics").field(
        Field::list("metrics.whitelist", "text")
            .label("Allowed addresses (CIDR)")
            .help(
                "Who may scrape /metrics without signing in. Empty = the \
                     endpoint is open, which is the historical behaviour.",
            ),
    )
}

fn registry() -> Section {
    Section::new("registry", "Plugin registry")
        .help("Where the Plugins screen looks for installable plugins. Both fields or neither.")
        .field(
            Field::text("registry.url")
                .label("Index URL")
                .help("URL, path or file:// of the signed index.json."),
        )
        .field(
            Field::text("registry.public_key")
                .label("Signing key")
                .help("Base64 ed25519 public key the index is verified against."),
        )
}

fn plugin_runtimes() -> Section {
    Section::new("plugin_runtimes", "Plugin runtimes")
        .help(
            "Containers you run yourself that host plugins written in other \
             languages. homeCore does not manage the container — only whether \
             one is allowed to join.",
        )
        .field(
            Field::toggle("plugin_runtimes.enabled")
                .label("Allow plugin runtimes")
                .default(true),
        )
        .field(
            Field::enumeration("plugin_runtimes.mode")
                .label("Enrollment")
                .render("segmented")
                .default("open")
                .help(
                    "Open: a runtime may ask to join and you approve it by matching \
                     a code shown in its logs. Token: you issue a token first and \
                     nothing is ever left pending.",
                )
                .option("open", "Open")
                .option("token", "Token only")
                .visible_when(Cond::truthy("plugin_runtimes.enabled")),
        )
        .field(
            Field::toggle("plugin_runtimes.whitelist_only")
                .label("Local network only")
                .default(true)
                .help("Only allow enrollment from the addresses in Security -> whitelist.")
                .visible_when(Cond::truthy("plugin_runtimes.enabled")),
        )
        .field(
            Field::duration("plugin_runtimes.pending_ttl_mins")
                .label("Pending expires after")
                .unit("mins")
                .default(15)
                .min(1)
                .visible_when(Cond::all([
                    Cond::truthy("plugin_runtimes.enabled"),
                    Cond::eq("plugin_runtimes.mode", "open"),
                ])),
        )
        .field(
            Field::int("plugin_runtimes.max_pending")
                .label("Max pending at once")
                .default(5)
                .min(1)
                .help("Bounds how many requests can queue for your attention.")
                .visible_when(Cond::all([
                    Cond::truthy("plugin_runtimes.enabled"),
                    Cond::eq("plugin_runtimes.mode", "open"),
                ])),
        )
        .field(
            Field::int("plugin_runtimes.max_denials")
                .label("Denials before cooldown")
                .default(3)
                .min(1)
                .visible_when(Cond::all([
                    Cond::truthy("plugin_runtimes.enabled"),
                    Cond::eq("plugin_runtimes.mode", "open"),
                ])),
        )
        .field(
            Field::duration("plugin_runtimes.denial_cooldown_mins")
                .label("Cooldown")
                .unit("mins")
                .default(60)
                .min(1)
                .visible_when(Cond::all([
                    Cond::truthy("plugin_runtimes.enabled"),
                    Cond::eq("plugin_runtimes.mode", "open"),
                ])),
        )
}

fn lifecycle() -> Section {
    Section::new("lifecycle", "Startup & shutdown")
        .field(
            Field::duration("startup.plugin_ready_delay_secs")
                .label("Plugin ready delay")
                .unit("secs")
                .help("Wait after boot before publishing state, so plugins have subscribed."),
        )
        .field(
            Field::duration("shutdown.drain_timeout_secs")
                .label("Shutdown drain timeout")
                .unit("secs")
                .help("How long plugins get to flush before they are killed."),
        )
        .field(
            Field::int("scheduler.catchup_window_minutes")
                .label("Scheduler catch-up window")
                .unit("mins")
                .help(
                    "After a restart, fire time and solar triggers missed within \
                     this window. 0 = never catch up.",
                ),
        )
}

fn logging() -> Section {
    Section::new("logging", "Logging")
        .field(
            Field::enumeration("logging.level")
                .label("Level")
                .help("The default for every target below.")
                .option("error", "error")
                .option("warn", "warn")
                .option("info", "info")
                .option("debug", "debug")
                .option("trace", "trace")
                .default("info"),
        )
        .field(
            Field::enumeration("logging.time_display")
                .label("Timestamps")
                .option("local", "Local time")
                .option("utc", "UTC")
                .default("local"),
        )
}

fn logging_stderr() -> Section {
    Section::new("logging.stderr", "Logging — console")
        .field(Field::toggle("logging.stderr.enabled").label("Enabled"))
        .field(format_field("logging.stderr.format"))
        .field(
            Field::toggle("logging.stderr.ansi")
                .label("Colour")
                .help("Turn off when the output is captured by a service manager."),
        )
}

fn logging_file() -> Section {
    Section::new("logging.file", "Logging — file")
        .field(Field::toggle("logging.file.enabled").label("Enabled"))
        .field(
            Field::text("logging.file.dir")
                .label("Directory")
                .render("path")
                .help("Default <base_dir>/logs."),
        )
        .field(
            Field::text("logging.file.prefix")
                .label("Filename prefix")
                .help("homecore → homecore.2026-07-28.log"),
        )
        .field(rotation_field("logging.file.rotation"))
        .field(
            Field::int("logging.file.max_size_mb")
                .label("Rotate at")
                .unit("MB")
                .help("Also roll over once a file reaches this size. 0 = size is not a trigger."),
        )
        .field(
            Field::toggle("logging.file.compress")
                .label("Compress rotated files")
                .help("gzip each file once it rolls over."),
        )
        .field(
            Field::int("logging.file.prune_after_days")
                .label("Delete after")
                .unit("days")
                .help("Delete rotated files older than this. 0 = keep forever."),
        )
        .field(format_field("logging.file.format"))
}

fn logging_rules_file() -> Section {
    Section::new("logging.rules_file", "Logging — rules file")
        .help("A separate file carrying only rule-engine lines, so automations are readable on their own.")
        .field(Field::toggle("logging.rules_file.enabled").label("Enabled"))
        .field(
            Field::text("logging.rules_file.dir")
                .label("Directory")
                .render("path"),
        )
        .field(
            Field::text("logging.rules_file.prefix")
                .label("Filename prefix"),
        )
        .field(rotation_field("logging.rules_file.rotation"))
        .field(
            Field::int("logging.rules_file.max_size_mb")
                .label("Rotate at")
                .unit("MB"),
        )
        .field(
            Field::toggle("logging.rules_file.compress").label("Compress rotated files"),
        )
        .field(
            Field::int("logging.rules_file.prune_after_days")
                .label("Delete after")
                .unit("days"),
        )
        .field(format_field("logging.rules_file.format"))
}

fn logging_syslog() -> Section {
    Section::new("logging.syslog", "Logging — syslog")
        .field(Field::toggle("logging.syslog.enabled").label("Enabled"))
        .field(
            Field::host("logging.syslog.host")
                .label("Host")
                .visible_when(Cond::truthy("logging.syslog.enabled")),
        )
        .field(
            Field::port("logging.syslog.port")
                .label("Port")
                .visible_when(Cond::truthy("logging.syslog.enabled")),
        )
        .field(
            Field::enumeration("logging.syslog.transport")
                .label("Transport")
                .option("udp", "UDP")
                .option("tcp", "TCP")
                .option("unix", "Unix socket")
                .visible_when(Cond::truthy("logging.syslog.enabled")),
        )
        .field(
            Field::enumeration("logging.syslog.protocol")
                .label("Protocol")
                .help("RFC 5424 is the modern one; 3164 is BSD syslog.")
                .option("rfc5424", "RFC 5424")
                .option("rfc3164", "RFC 3164")
                .visible_when(Cond::truthy("logging.syslog.enabled")),
        )
        .field(
            Field::text("logging.syslog.facility")
                .label("Facility")
                .help("user, or local0 … local7.")
                .visible_when(Cond::truthy("logging.syslog.enabled")),
        )
        .field(
            Field::text("logging.syslog.app_name")
                .label("App name")
                .help("The process name reported to syslog.")
                .visible_when(Cond::truthy("logging.syslog.enabled")),
        )
        .field(
            Field::text("logging.syslog.level")
                .label("Level")
                .help("Overrides the global level for syslog only. Empty = follow it.")
                .visible_when(Cond::truthy("logging.syslog.enabled")),
        )
}

fn logging_stream() -> Section {
    Section::new("logging.stream", "Logging — live stream")
        .help("The feed behind Administration → Logs.")
        .field(Field::toggle("logging.stream.enabled").label("Enabled"))
        .field(
            Field::int("logging.stream.ring_buffer_size")
                .label("Backlog")
                .unit("lines")
                .help("How many recent lines a client sees when it connects."),
        )
}

fn web_admin() -> Section {
    Section::new("web_admin", "Bundled web UI")
        .help(
            "The UI core used to serve itself. hc-web is its own container now, \
             so this stays off unless you are running the retired layout.",
        )
        .field(
            Field::toggle("web_admin.enabled")
                .label("Serve the bundled UI")
                .help("Serve a UI from core at /.")
                .default(false),
        )
}

// ── shared field shapes ─────────────────────────────────────────────────────

fn format_field(key: &str) -> Field {
    Field::enumeration(key)
        .label("Format")
        .option("pretty", "Pretty")
        .option("compact", "Compact")
        .option("json", "JSON")
}

fn rotation_field(key: &str) -> Field {
    Field::enumeration(key)
        .label("Rotation")
        .option("minutely", "Every minute")
        .option("hourly", "Hourly")
        .option("daily", "Daily")
        .option("weekly", "Weekly")
        .option("never", "Never")
}
