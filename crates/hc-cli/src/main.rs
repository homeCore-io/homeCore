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
        #[arg(long, default_value = "devices:read,automations:read,scenes:read,dashboards:read,areas:read")]
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
    },
    /// List API keys. Without `api_keys:admin` scope, shows only self-owned.
    List,
    /// Revoke an API key by ID.
    Revoke {
        #[arg(long)]
        id: uuid::Uuid,
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
        Command::Auth(AuthCommand::Login { username, password }) => {
            cmd_auth_login(&cli, &cfg, &config_path, username, password.as_deref()).await
        }
        Command::ApiKey(cmd) => match cmd {
            ApiKeyCommand::Create {
                label,
                scopes,
                expires,
                cidr,
            } => cmd_api_key_create(&cli, &cfg, label, scopes, *expires, cidr.clone()).await,
            ApiKeyCommand::List => cmd_api_key_list(&cli, &cfg).await,
            ApiKeyCommand::Revoke { id } => cmd_api_key_revoke(&cli, &cfg, *id).await,
        },
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
            created.get("username").and_then(|v| v.as_str()).unwrap_or(admin_username)
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
            uuid::Uuid::parse_str(uid_s)
                .with_context(|| format!("parsing {service_label} uid"))?
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

fn parse_role(s: &str) -> Result<Role> {
    match s.trim().to_ascii_lowercase().as_str() {
        "admin" => Ok(Role::Admin),
        "user" => Ok(Role::User),
        "read_only" | "readonly" | "read-only" => Ok(Role::ReadOnly),
        other => bail!("unknown role `{other}` — expected admin | user | read_only"),
    }
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
        owner_uid: None,
    };
    let resp: CreateApiKeyResponse = client.post("/auth/api-keys", &req).await?;

    if cli.output == "json" {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        println!("id:      {}", resp.id);
        println!("label:   {}", resp.label);
        println!("scopes:  {}", resp.scopes.join(", "));
        println!(
            "expires: {}",
            resp.expires_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "never".into())
        );
        println!();
        println!("token (save now — not shown again):");
        println!("  {}", resp.token);
    }
    Ok(())
}

async fn cmd_api_key_list(cli: &Cli, cfg: &Config) -> Result<()> {
    let client = make_client(cli, cfg).await?;
    let resp: Vec<hc_api_types::api_keys::ApiKeySummary> =
        client.get("/auth/api-keys").await?;
    if cli.output == "json" {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else if resp.is_empty() {
        println!("(no API keys)");
    } else {
        println!("{:<38}  {:<20}  {:<40}", "ID", "Label", "Scopes");
        for k in &resp {
            let revoked = if k.revoked_at.is_some() { " [revoked]" } else { "" };
            println!(
                "{:<38}  {:<20}  {}{revoked}",
                k.id,
                truncate(&k.label, 20),
                k.scopes.join(",")
            );
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
