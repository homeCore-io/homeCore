//! `hc-cli` — command-line administration tool for homeCore.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use hc_api_types::api_keys::{CreateApiKeyRequest, CreateApiKeyResponse};
use hc_api_types::auth::{CreateUserRequest, LoginRequest, LoginResponse};
use hc_auth::Role;
use hc_cli::client::{pick_local, Client, Transport};
use hc_cli::config::{Config, StoredCredentials};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(version, about = "homeCore administration CLI")]
struct Cli {
    /// Base URL for TCP connection (e.g. `http://10.0.10.10:8080`).
    /// If omitted, defaults to the UDS admin socket with TCP fallback.
    #[arg(long, global = true)]
    host: Option<String>,

    /// Path to a config file; defaults to `~/.config/hc-cli/config.toml`.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Output format: `human` or `json`.
    #[arg(long, global = true, default_value = "human")]
    output: String,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// First-run bootstrap: create an admin user, optionally mint one
    /// service API key in the same step. For additional keys later, use
    /// `hc-cli api-key create`.
    Setup {
        /// Admin username (default `admin`).
        #[arg(long, default_value = "admin")]
        admin_username: String,
        /// Admin password. If not provided, prompts interactively.
        #[arg(long)]
        admin_password: Option<String>,
        /// Also create a service user + API key in one step. Use this when
        /// you know the first consumer up front (a script, a bridge, hc-mcp);
        /// otherwise run `hc-cli api-key create` later to mint keys as needed.
        #[arg(long, default_value = "false")]
        create_service_key: bool,
        /// Label for the service user and key (required when
        /// `--create-service-key` is set). Used as both the username and
        /// the key's display label.
        #[arg(long, default_value = "api-service")]
        service_label: String,
        /// Comma-separated scopes for the service key. Must be a subset of
        /// the service user's role scopes (controlled by `--service-role`).
        #[arg(
            long,
            default_value = "devices:read,automations:read,scenes:read,dashboards:read,areas:read"
        )]
        service_scopes: String,
        /// Role for the service user: `read_only` or `user`. Defaults to the
        /// safer `read_only` — bump to `user` for service accounts that
        /// command devices or edit automations.
        #[arg(long, default_value = "read_only")]
        service_role: String,
        /// Skip all prompts; fail if input would be required.
        #[arg(long, default_value = "false")]
        non_interactive: bool,
    },

    /// Auth-related commands.
    #[command(subcommand)]
    Auth(AuthCommand),

    /// API-key management.
    #[command(subcommand)]
    ApiKey(ApiKeyCommand),

    /// User management (admin-scoped operations).
    #[command(subcommand)]
    User(UserCommand),

    /// Query the audit log (admin-scoped).
    #[command(subcommand)]
    Audit(AuditCommand),

    /// MQTT broker tooling — currently: generate an external Mosquitto
    /// config from a homeCore `homecore.toml`.
    #[command(subcommand)]
    Broker(BrokerCommand),
}

#[derive(Subcommand, Debug)]
enum BrokerCommand {
    /// Generate Mosquitto `mosquitto.conf`, `aclfile`, and a `passwd.setup.sh`
    /// helper from a homeCore config. Use when deploying an external broker
    /// for real topic authz (see `mqttAuthzPlan.md`).
    GenerateMosquittoConfig {
        /// Path to `homecore.toml`. Default: ./config/homecore.toml.
        #[arg(long, default_value = "config/homecore.toml")]
        config: PathBuf,
        /// Directory to write generated files into. Created if missing.
        #[arg(long, default_value = "mosquitto-config")]
        out_dir: PathBuf,
        /// Listener port to write into mosquitto.conf. Default: 1883.
        #[arg(long, default_value_t = 1883)]
        listener_port: u16,
        /// Overwrite any existing files in the output directory.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum AuditCommand {
    /// List audit events with optional filters.
    Query {
        #[arg(long)]
        actor_id: Option<uuid::Uuid>,
        /// user | api_key | local_admin | ip_whitelist | system | anonymous
        #[arg(long)]
        actor_type: Option<String>,
        #[arg(long)]
        event_type: Option<String>,
        #[arg(long)]
        target_kind: Option<String>,
        #[arg(long)]
        target_id: Option<String>,
        /// success | denied | error
        #[arg(long)]
        result: Option<String>,
        /// RFC3339 timestamp lower bound (inclusive).
        #[arg(long)]
        from: Option<String>,
        /// RFC3339 timestamp upper bound (inclusive).
        #[arg(long)]
        to: Option<String>,
        #[arg(long, default_value = "50")]
        limit: u32,
        #[arg(long, default_value = "0")]
        offset: u32,
    },
}

#[derive(Subcommand, Debug)]
enum AuthCommand {
    /// Interactive password login. Stores the issued token in the config.
    Login {
        /// Username to log in as.
        #[arg(long)]
        username: String,
        /// Password (prompts if not provided).
        #[arg(long)]
        password: Option<String>,
    },
    /// Show who the current credential authenticates as.
    Whoami,
    /// Discard the stored credential from the config.
    Logout,
}

#[derive(Subcommand, Debug)]
enum ApiKeyCommand {
    /// Create a new API key. The token is printed once — save it.
    Create {
        /// Human-readable label.
        #[arg(long)]
        label: String,
        /// Comma-separated scopes (e.g. `devices:read,rules:read`).
        #[arg(long)]
        scopes: String,
        /// Lifetime in days. Omit for no expiry.
        #[arg(long)]
        expires: Option<u32>,
        /// Optional CIDR restrictions (repeatable).
        #[arg(long)]
        cidr: Vec<String>,
        /// Target owner UID (requires api_keys:admin; defaults to self).
        #[arg(long)]
        owner: Option<uuid::Uuid>,
    },
    /// List API keys. Without `api_keys:admin`, shows only self-owned.
    List {
        /// Filter by owner UID (requires api_keys:admin if not self).
        #[arg(long)]
        owner: Option<uuid::Uuid>,
        /// Include revoked keys in the listing.
        #[arg(long, default_value = "false")]
        include_revoked: bool,
    },
    /// Show details of one API key by ID.
    Show {
        #[arg(long)]
        id: uuid::Uuid,
    },
    /// Revoke an API key by ID.
    Revoke {
        #[arg(long)]
        id: uuid::Uuid,
    },
    /// Update mutable fields on an API key. Any flag omitted is left
    /// unchanged. Does NOT rotate the secret — use `rotate` for that.
    Update {
        #[arg(long)]
        id: uuid::Uuid,
        #[arg(long)]
        label: Option<String>,
        /// Comma-separated scopes (replaces current).
        #[arg(long)]
        scopes: Option<String>,
        #[arg(long)]
        expires: Option<u32>,
        /// Comma-separated CIDRs (replaces current). Pass an empty string
        /// `""` to clear.
        #[arg(long)]
        cidrs: Option<String>,
    },
    /// Rotate the secret on an existing API key. Issues a fresh token
    /// while keeping the id, scopes, label, and expiry. The new token is
    /// printed once — save it.
    Rotate {
        #[arg(long)]
        id: uuid::Uuid,
    },
}

#[derive(Subcommand, Debug)]
enum UserCommand {
    /// List all users.
    List,
    /// Show one user by id or username.
    Show {
        /// User id (UUID). Mutually exclusive with --username.
        #[arg(long)]
        id: Option<uuid::Uuid>,
        /// Username. Mutually exclusive with --id.
        #[arg(long)]
        username: Option<String>,
    },
    /// Create a new user.
    Create {
        #[arg(long)]
        username: String,
        /// `admin`, `user`, or `read_only`.
        #[arg(long, default_value = "user")]
        role: String,
        /// Password (prompts if not provided).
        #[arg(long)]
        password: Option<String>,
    },
    /// Delete a user by id.
    Delete {
        #[arg(long)]
        id: uuid::Uuid,
    },
    /// Change a user's role.
    SetRole {
        #[arg(long)]
        id: uuid::Uuid,
        /// `admin`, `user`, or `read_only`.
        #[arg(long)]
        role: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .without_time()
        .init();

    let cli = Cli::parse();
    let config_path = cli
        .config
        .clone()
        .or_else(Config::default_path)
        .context("cannot determine config path")?;
    let cfg = Config::load(&config_path)?;

    match &cli.cmd {
        Command::Setup {
            admin_username,
            admin_password,
            create_service_key,
            service_label,
            service_scopes,
            service_role,
            non_interactive,
        } => {
            cmd_setup(
                &cli,
                &cfg,
                &config_path,
                admin_username,
                admin_password.as_deref(),
                *create_service_key,
                service_label,
                service_scopes,
                service_role,
                *non_interactive,
            )
            .await
        }
        Command::Auth(cmd) => match cmd {
            AuthCommand::Login { username, password } => {
                cmd_auth_login(&cli, &cfg, &config_path, username, password.as_deref()).await
            }
            AuthCommand::Whoami => cmd_auth_whoami(&cli, &cfg).await,
            AuthCommand::Logout => cmd_auth_logout(&cfg, &config_path),
        },
        Command::ApiKey(cmd) => match cmd {
            ApiKeyCommand::Create {
                label,
                scopes,
                expires,
                cidr,
                owner,
            } => {
                cmd_api_key_create(&cli, &cfg, label, scopes, *expires, cidr.clone(), *owner).await
            }
            ApiKeyCommand::List {
                owner,
                include_revoked,
            } => cmd_api_key_list(&cli, &cfg, *owner, *include_revoked).await,
            ApiKeyCommand::Show { id } => cmd_api_key_show(&cli, &cfg, *id).await,
            ApiKeyCommand::Revoke { id } => cmd_api_key_revoke(&cli, &cfg, *id).await,
            ApiKeyCommand::Update {
                id,
                label,
                scopes,
                expires,
                cidrs,
            } => {
                cmd_api_key_update(
                    &cli,
                    &cfg,
                    *id,
                    label.as_deref(),
                    scopes.as_deref(),
                    *expires,
                    cidrs.as_deref(),
                )
                .await
            }
            ApiKeyCommand::Rotate { id } => cmd_api_key_rotate(&cli, &cfg, *id).await,
        },
        Command::User(cmd) => match cmd {
            UserCommand::List => cmd_user_list(&cli, &cfg).await,
            UserCommand::Show { id, username } => {
                cmd_user_show(&cli, &cfg, *id, username.as_deref()).await
            }
            UserCommand::Create {
                username,
                role,
                password,
            } => cmd_user_create(&cli, &cfg, username, role, password.as_deref()).await,
            UserCommand::Delete { id } => cmd_user_delete(&cli, &cfg, *id).await,
            UserCommand::SetRole { id, role } => cmd_user_set_role(&cli, &cfg, *id, role).await,
        },
        Command::Broker(BrokerCommand::GenerateMosquittoConfig {
            config,
            out_dir,
            listener_port,
            force,
        }) => cmd_broker_gen_mosquitto(config, out_dir, *listener_port, *force),
        Command::Audit(AuditCommand::Query {
            actor_id,
            actor_type,
            event_type,
            target_kind,
            target_id,
            result,
            from,
            to,
            limit,
            offset,
        }) => {
            cmd_audit_query(
                &cli,
                &cfg,
                *actor_id,
                actor_type.as_deref(),
                event_type.as_deref(),
                target_kind.as_deref(),
                target_id.as_deref(),
                result.as_deref(),
                from.as_deref(),
                to.as_deref(),
                *limit,
                *offset,
            )
            .await
        }
    }
}

// ── Client construction ────────────────────────────────────────────────────

async fn make_client(cli: &Cli, cfg: &Config) -> Result<Client> {
    if let Some(host) = &cli.host {
        let token = cfg.credentials.as_ref().and_then(|c| {
            if c.host == *host {
                Some(c.token.clone())
            } else {
                None
            }
        });
        let c = Client::new(Transport::Tcp {
            base_url: host.clone(),
            token,
        });
        c.probe()
            .await
            .with_context(|| format!("host {host} unreachable"))?;
        return Ok(c);
    }

    let token = cfg.credentials.as_ref().map(|c| c.token.clone());
    pick_local(&cfg.uds_path, &cfg.tcp_url, token).await
}

// ── setup ──────────────────────────────────────────────────────────────────

async fn cmd_setup(
    cli: &Cli,
    cfg: &Config,
    _config_path: &std::path::Path,
    admin_username: &str,
    admin_password: Option<&str>,
    create_service_key: bool,
    service_label: &str,
    service_scopes_csv: &str,
    service_role: &str,
    non_interactive: bool,
) -> Result<()> {
    let client = make_client(cli, cfg).await?;

    // Check current user count. If no users exist, we need to bootstrap;
    // otherwise setup is a no-op (other than optionally the mcp-service step).
    let existing: Vec<serde_json::Value> = client
        .get("/auth/users")
        .await
        .context("listing users — admin access required (UDS or admin token)")?;
    let has_admin = existing
        .iter()
        .any(|v| v.get("role").and_then(|r| r.as_str()) == Some("admin"));

    if !has_admin {
        let password = match admin_password {
            Some(p) => p.to_string(),
            None => {
                if non_interactive {
                    bail!("--admin-password required in non-interactive mode");
                }
                rpassword::prompt_password(format!("Password for {admin_username}: "))
                    .context("reading password")?
            }
        };
        if password.len() < 8 {
            bail!("admin password must be at least 8 characters");
        }
        let req = CreateUserRequest {
            username: admin_username.into(),
            password,
            role: Role::Admin,
        };
        let created: serde_json::Value = client.post("/auth/users", &req).await?;
        println!(
            "Created admin user: {}",
            created
                .get("username")
                .and_then(|v| v.as_str())
                .unwrap_or(admin_username)
        );
    } else {
        println!("Admin user already present — skipping admin creation.");
    }

    if create_service_key {
        let role = parse_role(service_role)?;
        let scopes: Vec<String> = service_scopes_csv
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if scopes.is_empty() {
            bail!("--service-scopes must list at least one scope");
        }

        // Reuse an existing user with this label if present; otherwise create.
        let svc_exists = existing
            .iter()
            .any(|v| v.get("username").and_then(|u| u.as_str()) == Some(service_label));
        let svc_uid: uuid::Uuid = if !svc_exists {
            let svc_pw = rand_password(24);
            let req = CreateUserRequest {
                username: service_label.into(),
                password: svc_pw,
                role,
            };
            let created: serde_json::Value = client.post("/auth/users", &req).await?;
            let uid_s = created.get("id").and_then(|v| v.as_str()).unwrap_or("");
            uuid::Uuid::parse_str(uid_s).with_context(|| format!("parsing {service_label} uid"))?
        } else {
            let e = existing
                .iter()
                .find(|v| v.get("username").and_then(|u| u.as_str()) == Some(service_label))
                .unwrap();
            uuid::Uuid::parse_str(e.get("id").and_then(|v| v.as_str()).unwrap_or(""))
                .with_context(|| format!("parsing existing {service_label} uid"))?
        };

        let req = CreateApiKeyRequest {
            label: service_label.into(),
            scopes,
            expires_in_days: None,
            allowed_cidrs: vec![],
            owner_uid: Some(svc_uid),
        };
        let resp: CreateApiKeyResponse = client.post("/auth/api-keys", &req).await?;
        println!();
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!(
            "  API key for `{}` (SAVE NOW — will not be shown again)",
            service_label
        );
        println!();
        println!("  {}", resp.token);
        println!();
        println!("  Scopes: {}", resp.scopes.join(", "));
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    }

    Ok(())
}

// ── broker: generate-mosquitto-config ──────────────────────────────────────

#[derive(serde::Deserialize)]
struct HomecoreTomlRoot {
    broker: Option<BrokerTomlSection>,
}

#[derive(serde::Deserialize)]
struct BrokerTomlSection {
    #[serde(default)]
    clients: Vec<BrokerClientToml>,
}

#[derive(serde::Deserialize)]
struct BrokerClientToml {
    id: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    allow_pub: Vec<String>,
    #[serde(default)]
    allow_sub: Vec<String>,
}

fn cmd_broker_gen_mosquitto(
    config: &std::path::Path,
    out_dir: &std::path::Path,
    listener_port: u16,
    force: bool,
) -> Result<()> {
    let text =
        std::fs::read_to_string(config).with_context(|| format!("reading {}", config.display()))?;
    let parsed: HomecoreTomlRoot =
        toml::from_str(&text).with_context(|| format!("parsing {}", config.display()))?;
    let clients = parsed.broker.map(|b| b.clients).unwrap_or_default();
    if clients.is_empty() {
        bail!(
            "no [[broker.clients]] entries found in {} — nothing to generate",
            config.display()
        );
    }

    std::fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    let paths = [
        out_dir.join("mosquitto.conf"),
        out_dir.join("aclfile"),
        out_dir.join("passwd.setup.sh"),
    ];
    if !force {
        for p in &paths {
            if p.exists() {
                bail!("{} already exists (pass --force to overwrite)", p.display());
            }
        }
    }

    // mosquitto.conf
    let conf = format!(
        "# Generated by hc-cli. Do not edit by hand — regenerate from\n\
         # homecore.toml with `hc-cli broker generate-mosquitto-config`.\n\
         \n\
         listener {listener_port}\n\
         protocol mqtt\n\
         \n\
         # Require authn. Anonymous connections are rejected.\n\
         allow_anonymous false\n\
         password_file /mosquitto/config/passwd\n\
         \n\
         # Per-user topic ACLs — the real reason we're running Mosquitto\n\
         # instead of rumqttd (see mqttAuthzPlan.md).\n\
         acl_file /mosquitto/config/aclfile\n\
         \n\
         # Standard logging; adjust as needed for your deployment.\n\
         log_dest stdout\n\
         log_type error\n\
         log_type warning\n\
         log_type notice\n\
         log_type information\n\
         persistence true\n\
         persistence_location /mosquitto/data/\n"
    );
    write_file(&paths[0], &conf)?;

    // aclfile
    let mut acl = String::new();
    acl.push_str(
        "# Generated by hc-cli. Patterns are derived from [[broker.clients]]\n\
         # in homecore.toml.\n\n\
         # ── Rules for homeCore itself (reads and writes everything). ──────\n\
         user internal.core\n\
         topic readwrite homecore/#\n\
         topic readwrite $SYS/#\n\n",
    );
    for c in &clients {
        if c.id == "internal.core" {
            continue; // already written above
        }
        acl.push_str(&format!("# ── {} ──\n", c.id));
        acl.push_str(&format!("user {}\n", c.id));
        for pat in &c.allow_pub {
            acl.push_str(&format!("topic write {pat}\n"));
        }
        for pat in &c.allow_sub {
            acl.push_str(&format!("topic read {pat}\n"));
        }
        acl.push('\n');
    }
    write_file(&paths[1], &acl)?;

    // passwd.setup.sh — a helper the operator edits with real passwords,
    // then runs inside a container that has `mosquitto_passwd` available.
    let mut pw = String::new();
    pw.push_str("#!/usr/bin/env bash\n");
    pw.push_str("# Generated by hc-cli. Edit each password below to MATCH the corresponding\n");
    pw.push_str("# `password = \"...\"` entry in your homecore.toml (plaintext). Plugins\n");
    pw.push_str("# send the plaintext on CONNECT; Mosquitto hashes + compares.\n");
    pw.push_str("#\n");
    pw.push_str("# Run this script from within a container that has mosquitto_passwd,\n");
    pw.push_str("# e.g.:  docker run --rm -v $(pwd):/out eclipse-mosquitto \\\n");
    pw.push_str("#                   /out/passwd.setup.sh\n");
    pw.push_str("set -euo pipefail\n");
    pw.push_str("cd \"$(dirname \"$0\")\"\n");
    pw.push_str(": > passwd\n\n");
    for c in &clients {
        pw.push_str(&format!(
            "mosquitto_passwd -b passwd {} CHANGE_ME_{}\n",
            c.id,
            c.id.to_ascii_uppercase().replace('.', "_")
        ));
    }
    pw.push_str("\necho \"passwd file written.\"\n");
    write_file(&paths[2], &pw)?;
    // Set executable bit on the script.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&paths[2], std::fs::Permissions::from_mode(0o755));

    println!("Wrote:");
    for p in &paths {
        println!("  {}", p.display());
    }
    println!();
    println!("Next steps:");
    println!("  1. Edit passwd.setup.sh and replace each CHANGE_ME_<ID> with");
    println!("     the same plaintext password you have in homecore.toml.");
    println!("  2. Run passwd.setup.sh inside an eclipse-mosquitto container");
    println!("     to produce the hashed `passwd` file Mosquitto wants.");
    println!("  3. Deploy Mosquitto with all three files mounted, then set");
    println!("     `[broker].external_url` in homecore.toml to point at it.");
    println!("     See mqttAuthzPlan.md for the full recipe.");
    Ok(())
}

fn write_file(path: &std::path::Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn parse_role(s: &str) -> Result<Role> {
    match s.trim().to_ascii_lowercase().as_str() {
        "admin" => Ok(Role::Admin),
        "user" => Ok(Role::User),
        "read_only" | "readonly" | "read-only" => Ok(Role::ReadOnly),
        "observer" => Ok(Role::Observer),
        "device_operator" | "deviceoperator" | "device-operator" => Ok(Role::DeviceOperator),
        "rule_editor" | "ruleeditor" | "rule-editor" => Ok(Role::RuleEditor),
        "service_operator" | "serviceoperator" | "service-operator" => Ok(Role::ServiceOperator),
        other => bail!(
            "unknown role `{other}` — expected \
             admin | user | read_only | observer | device_operator | rule_editor | service_operator"
        ),
    }
}

// ── auth: whoami / logout ──────────────────────────────────────────────────

async fn cmd_auth_whoami(cli: &Cli, cfg: &Config) -> Result<()> {
    let client = make_client(cli, cfg).await?;
    let v: serde_json::Value = client.get("/auth/me").await?;
    if cli.output == "json" {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        let username = v.get("username").and_then(|s| s.as_str()).unwrap_or("(?)");
        let role = v.get("role").and_then(|s| s.as_str()).unwrap_or("(?)");
        let id = v.get("id").and_then(|s| s.as_str()).unwrap_or("(?)");
        println!("username: {username}");
        println!("role:     {role}");
        println!("id:       {id}");
        // For UDS admin bypass, the /me endpoint may report the associated
        // user account. If the caller is using a stored token, that token
        // authenticates as `username`.
        match client.transport() {
            hc_cli::Transport::Uds { socket } => {
                println!("via:      UDS {}", socket.display());
            }
            hc_cli::Transport::Tcp { base_url, token } => {
                let form = if let Some(t) = token {
                    if t.starts_with("hc_sk_") {
                        "api key"
                    } else {
                        "JWT"
                    }
                } else {
                    "unauth"
                };
                println!("via:      TCP {base_url} ({form})");
            }
        }
    }
    Ok(())
}

fn cmd_auth_logout(cfg: &Config, config_path: &std::path::Path) -> Result<()> {
    let mut new_cfg = cfg.clone();
    if new_cfg.credentials.is_none() {
        println!("(no stored credentials)");
        return Ok(());
    }
    new_cfg.credentials = None;
    new_cfg.save(config_path)?;
    println!("Stored credentials cleared from {}", config_path.display());
    Ok(())
}

// ── users ──────────────────────────────────────────────────────────────────

async fn cmd_user_list(cli: &Cli, cfg: &Config) -> Result<()> {
    let client = make_client(cli, cfg).await?;
    let users: Vec<serde_json::Value> = client.get("/auth/users").await?;
    if cli.output == "json" {
        println!("{}", serde_json::to_string_pretty(&users)?);
        return Ok(());
    }
    if users.is_empty() {
        println!("(no users)");
        return Ok(());
    }
    println!(
        "{:<38}  {:<20}  {:<10}  Created",
        "ID", "Username", "Role"
    );
    for u in &users {
        let id = u.get("id").and_then(|s| s.as_str()).unwrap_or("");
        let username = u.get("username").and_then(|s| s.as_str()).unwrap_or("");
        let role = u.get("role").and_then(|s| s.as_str()).unwrap_or("");
        let created = u
            .get("created_at")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .split('T')
            .next()
            .unwrap_or("");
        println!(
            "{:<38}  {:<20}  {:<10}  {}",
            id,
            truncate(username, 20),
            role,
            created
        );
    }
    Ok(())
}

async fn cmd_user_show(
    cli: &Cli,
    cfg: &Config,
    id: Option<uuid::Uuid>,
    username: Option<&str>,
) -> Result<()> {
    if id.is_some() && username.is_some() {
        bail!("provide exactly one of --id or --username");
    }
    if id.is_none() && username.is_none() {
        bail!("provide --id or --username");
    }
    let client = make_client(cli, cfg).await?;
    let users: Vec<serde_json::Value> = client.get("/auth/users").await?;
    let matched = users.into_iter().find(|u| {
        if let Some(target) = id {
            u.get("id").and_then(|s| s.as_str()) == Some(target.to_string().as_str())
        } else {
            u.get("username").and_then(|s| s.as_str()) == username
        }
    });
    let Some(u) = matched else {
        bail!("user not found");
    };
    if cli.output == "json" {
        println!("{}", serde_json::to_string_pretty(&u)?);
    } else {
        println!(
            "id:       {}",
            u.get("id").and_then(|s| s.as_str()).unwrap_or("")
        );
        println!(
            "username: {}",
            u.get("username").and_then(|s| s.as_str()).unwrap_or("")
        );
        println!(
            "role:     {}",
            u.get("role").and_then(|s| s.as_str()).unwrap_or("")
        );
        println!(
            "created:  {}",
            u.get("created_at").and_then(|s| s.as_str()).unwrap_or("")
        );
    }
    Ok(())
}

async fn cmd_user_create(
    cli: &Cli,
    cfg: &Config,
    username: &str,
    role: &str,
    password: Option<&str>,
) -> Result<()> {
    let client = make_client(cli, cfg).await?;
    let role = parse_role(role)?;
    let password = match password {
        Some(p) => p.to_string(),
        None => rpassword::prompt_password(format!("Password for {username}: "))?,
    };
    if password.len() < 8 {
        bail!("password must be at least 8 characters");
    }
    let req = CreateUserRequest {
        username: username.into(),
        password,
        role,
    };
    let resp: serde_json::Value = client.post("/auth/users", &req).await?;
    if cli.output == "json" {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        println!("Created user:");
        println!(
            "  id:       {}",
            resp.get("id").and_then(|s| s.as_str()).unwrap_or("")
        );
        println!(
            "  username: {}",
            resp.get("username").and_then(|s| s.as_str()).unwrap_or("")
        );
        println!(
            "  role:     {}",
            resp.get("role").and_then(|s| s.as_str()).unwrap_or("")
        );
    }
    Ok(())
}

async fn cmd_user_delete(cli: &Cli, cfg: &Config, id: uuid::Uuid) -> Result<()> {
    let client = make_client(cli, cfg).await?;
    let path = format!("/auth/users/{id}");
    client.delete(&path).await?;
    println!("Deleted user {id}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_audit_query(
    cli: &Cli,
    cfg: &Config,
    actor_id: Option<uuid::Uuid>,
    actor_type: Option<&str>,
    event_type: Option<&str>,
    target_kind: Option<&str>,
    target_id: Option<&str>,
    result: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    limit: u32,
    offset: u32,
) -> Result<()> {
    let client = make_client(cli, cfg).await?;
    let mut query: Vec<(&str, String)> = Vec::new();
    if let Some(v) = actor_id {
        query.push(("actor_id", v.to_string()));
    }
    if let Some(v) = actor_type {
        query.push(("actor_type", v.to_string()));
    }
    if let Some(v) = event_type {
        query.push(("event_type", v.to_string()));
    }
    if let Some(v) = target_kind {
        query.push(("target_kind", v.to_string()));
    }
    if let Some(v) = target_id {
        query.push(("target_id", v.to_string()));
    }
    if let Some(v) = result {
        query.push(("result", v.to_string()));
    }
    if let Some(v) = from {
        query.push(("from", v.to_string()));
    }
    if let Some(v) = to {
        query.push(("to", v.to_string()));
    }
    query.push(("limit", limit.to_string()));
    query.push(("offset", offset.to_string()));
    let qs: String = query
        .iter()
        .map(|(k, v)| {
            format!(
                "{k}={}",
                percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&");

    let path = format!("/audit?{qs}");
    let rows: Vec<serde_json::Value> = client.get(&path).await?;

    if cli.output == "json" {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("(no matching audit events)");
        return Ok(());
    }
    println!(
        "{:<20}  {:<10}  {:<24}  {:<22}  {:<8}  Target",
        "Timestamp", "Result", "Event", "Actor", "Kind"
    );
    for r in &rows {
        let ts = r
            .get("ts")
            .and_then(|s| s.as_str())
            .unwrap_or("").split('.')
            .next()
            .unwrap_or("")
            .replace('T', " ");
        let result = r.get("result").and_then(|s| s.as_str()).unwrap_or("");
        let ev = r.get("event_type").and_then(|s| s.as_str()).unwrap_or("");
        let actor = r.get("actor_label").and_then(|s| s.as_str()).unwrap_or("?");
        let kind = r.get("target_kind").and_then(|s| s.as_str()).unwrap_or("");
        let target = r.get("target_id").and_then(|s| s.as_str()).unwrap_or("");
        println!(
            "{:<20}  {:<10}  {:<24}  {:<22}  {:<8}  {}",
            truncate(&ts, 20),
            result,
            truncate(ev, 24),
            truncate(actor, 22),
            kind,
            target
        );
    }
    Ok(())
}

async fn cmd_user_set_role(cli: &Cli, cfg: &Config, id: uuid::Uuid, role: &str) -> Result<()> {
    let client = make_client(cli, cfg).await?;
    let role = parse_role(role)?;
    let path = format!("/auth/users/{id}/role");
    let body = hc_api_types::auth::SetRoleRequest { role };
    let resp: serde_json::Value = client.patch(&path, &body).await?;
    if cli.output == "json" {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        println!(
            "Set user {id} role to {}",
            resp.get("role").and_then(|s| s.as_str()).unwrap_or("?")
        );
    }
    Ok(())
}

// ── auth login ─────────────────────────────────────────────────────────────

async fn cmd_auth_login(
    cli: &Cli,
    cfg: &Config,
    config_path: &std::path::Path,
    username: &str,
    password: Option<&str>,
) -> Result<()> {
    // For login, we need a reachable server; UDS-default is fine if present
    // (login still works, just returns a JWT bound to the username).
    let client = make_client(cli, cfg).await?;

    let password = match password {
        Some(p) => p.to_string(),
        None => rpassword::prompt_password(format!("Password for {username}: "))?,
    };
    let req = LoginRequest {
        username: username.into(),
        password,
    };
    let resp: LoginResponse = client.post("/auth/login", &req).await?;

    // Determine which host the credential is bound to — the --host flag
    // when given, otherwise the config's TCP URL. UDS auth doesn't need
    // a stored token.
    let host = cli.host.clone().unwrap_or_else(|| cfg.tcp_url.clone());
    let mut new_cfg = cfg.clone();
    new_cfg.credentials = Some(StoredCredentials {
        host: host.clone(),
        token: resp.token,
    });
    new_cfg.save(config_path)?;
    println!("Logged in as {} at {host}", resp.user.username);
    Ok(())
}

// ── api-key ────────────────────────────────────────────────────────────────

async fn cmd_api_key_create(
    cli: &Cli,
    cfg: &Config,
    label: &str,
    scopes_csv: &str,
    expires: Option<u32>,
    cidrs: Vec<String>,
    owner: Option<uuid::Uuid>,
) -> Result<()> {
    let client = make_client(cli, cfg).await?;
    let scopes: Vec<String> = scopes_csv
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let req = CreateApiKeyRequest {
        label: label.into(),
        scopes,
        expires_in_days: expires,
        allowed_cidrs: cidrs,
        owner_uid: owner,
    };
    let resp: CreateApiKeyResponse = client.post("/auth/api-keys", &req).await?;
    print_create_response(cli, &resp);
    Ok(())
}

fn print_create_response(cli: &Cli, resp: &CreateApiKeyResponse) {
    if cli.output == "json" {
        println!(
            "{}",
            serde_json::to_string_pretty(resp).unwrap_or_else(|_| "{}".into())
        );
    } else {
        println!("id:       {}", resp.id);
        println!("label:    {}", resp.label);
        println!("owner:    {}", resp.owner_uid);
        println!("scopes:   {}", resp.scopes.join(", "));
        println!(
            "expires:  {}",
            resp.expires_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "never".into())
        );
        println!();
        println!("token (save now — not shown again):");
        println!("  {}", resp.token);
    }
}

async fn cmd_api_key_list(
    cli: &Cli,
    cfg: &Config,
    owner: Option<uuid::Uuid>,
    include_revoked: bool,
) -> Result<()> {
    let client = make_client(cli, cfg).await?;
    let mut resp: Vec<hc_api_types::api_keys::ApiKeySummary> = client.get("/auth/api-keys").await?;
    if let Some(o) = owner {
        resp.retain(|k| k.owner_uid == o);
    }
    if !include_revoked {
        resp.retain(|k| k.revoked_at.is_none());
    }
    if cli.output == "json" {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }
    if resp.is_empty() {
        println!("(no API keys)");
        return Ok(());
    }
    println!(
        "{:<38}  {:<20}  {:<8}  {:<16}  Scopes",
        "ID", "Label", "Status", "Last used"
    );
    for k in &resp {
        let status = key_status(k);
        let last_used = k
            .last_used_at
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "—".into());
        println!(
            "{:<38}  {:<20}  {:<8}  {:<16}  {}",
            k.id,
            truncate(&k.label, 20),
            status,
            last_used,
            k.scopes.join(",")
        );
    }
    Ok(())
}

fn key_status(k: &hc_api_types::api_keys::ApiKeySummary) -> &'static str {
    if k.revoked_at.is_some() {
        "revoked"
    } else if k
        .expires_at
        .map(|e| e <= chrono::Utc::now())
        .unwrap_or(false)
    {
        "expired"
    } else {
        "active"
    }
}

async fn cmd_api_key_show(cli: &Cli, cfg: &Config, id: uuid::Uuid) -> Result<()> {
    let client = make_client(cli, cfg).await?;
    let all: Vec<hc_api_types::api_keys::ApiKeySummary> = client.get("/auth/api-keys").await?;
    let k = all
        .into_iter()
        .find(|k| k.id == id)
        .ok_or_else(|| anyhow::anyhow!("API key {id} not found"))?;
    if cli.output == "json" {
        println!("{}", serde_json::to_string_pretty(&k)?);
    } else {
        println!("id:          {}", k.id);
        println!("label:       {}", k.label);
        println!("owner:       {}", k.owner_uid);
        println!("status:      {}", key_status(&k));
        println!("prefix:      hc_sk_{}…", k.prefix);
        println!("scopes:      {}", k.scopes.join(", "));
        println!(
            "created:     {}",
            k.created_at.format("%Y-%m-%d %H:%M:%S UTC")
        );
        println!(
            "last_used:   {}",
            k.last_used_at
                .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| "never".into())
        );
        println!(
            "expires:     {}",
            k.expires_at
                .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| "never".into())
        );
        if !k.allowed_cidrs.is_empty() {
            println!("cidrs:       {}", k.allowed_cidrs.join(", "));
        }
        if let Some(r) = k.revoked_at {
            println!("revoked_at:  {}", r.format("%Y-%m-%d %H:%M:%S UTC"));
        }
    }
    Ok(())
}

async fn cmd_api_key_revoke(cli: &Cli, cfg: &Config, id: uuid::Uuid) -> Result<()> {
    let client = make_client(cli, cfg).await?;
    let path = format!("/auth/api-keys/{id}");
    client.delete(&path).await?;
    println!("Revoked API key {id}");
    Ok(())
}

async fn cmd_api_key_update(
    cli: &Cli,
    cfg: &Config,
    id: uuid::Uuid,
    label: Option<&str>,
    scopes_csv: Option<&str>,
    expires: Option<u32>,
    cidrs_csv: Option<&str>,
) -> Result<()> {
    let client = make_client(cli, cfg).await?;
    let scopes = scopes_csv.map(|s| {
        s.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect::<Vec<_>>()
    });
    let cidrs = cidrs_csv.map(|s| {
        if s.trim().is_empty() {
            Vec::new()
        } else {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect::<Vec<_>>()
        }
    });
    let body = hc_api_types::api_keys::UpdateApiKeyRequest {
        label: label.map(str::to_string),
        scopes,
        expires_in_days: expires,
        allowed_cidrs: cidrs,
    };
    let path = format!("/auth/api-keys/{id}");
    let resp: hc_api_types::api_keys::ApiKeySummary = client.patch(&path, &body).await?;
    if cli.output == "json" {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        println!("Updated API key {id}");
        println!("  label:   {}", resp.label);
        println!("  scopes:  {}", resp.scopes.join(", "));
    }
    Ok(())
}

async fn cmd_api_key_rotate(cli: &Cli, cfg: &Config, id: uuid::Uuid) -> Result<()> {
    let client = make_client(cli, cfg).await?;
    let path = format!("/auth/api-keys/{id}/rotate");
    let resp: CreateApiKeyResponse = client.post(&path, &serde_json::json!({})).await?;
    if cli.output != "json" {
        println!("Rotated API key {id} — new secret issued.");
        println!();
    }
    print_create_response(cli, &resp);
    Ok(())
}

// ── helpers ────────────────────────────────────────────────────────────────

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n.saturating_sub(1)])
    }
}

fn rand_password(len: usize) -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| {
            let i = rng.gen_range(0..CHARSET.len());
            CHARSET[i] as char
        })
        .collect()
}
