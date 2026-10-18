//! Backup and restore handlers.
//!
//! ## Backup (`POST /system/backup`)
//!
//! Creates a zip archive of all persistent state and config:
//! - `state.redb`        — device registry, rules, scenes, areas, users (redb)
//! - `history.db`        — time-series state history (SQLite)
//! - `config/homecore.toml` — main config file (if present)
//! - `config/modes.toml`    — modes config (if present)
//! - `config/mode_definitions.json` — criteria-mode definitions (if present)
//! - `rules/*.ron`          — all rule files (if present)
//! - `plugins/<plugin_id>/config.toml` — each registered plugin's config
//!   file (if present). Plugin IDs are used as the archive key so a
//!   backup taken on host A can restore correctly to host B even when
//!   the absolute config paths differ — destinations on restore come
//!   from the *running host's* `[[plugins]]` table.
//! - `calendars/*` — the calendars rules check (`.ics` and their `.meta.json`)
//!
//! Not included: the audit log (`audit.db`), which records what happened to
//! the house rather than what the house is.
//!
//! The archive is streamed as `application/zip` with a timestamped filename.
//! Requires Admin role.
//!
//! ## Restore (`POST /system/restore`)
//!
//! Accepts the **raw ZIP bytes as the request body** — not JSON, and not
//! base64. Requires Admin role.
//!
//! - `?check=true` reads the archive and says what it would put back and what
//!   it would skip, changing nothing.
//! - Otherwise the archive is checked the same way and **staged** beside the
//!   state database as `restore-pending.zip`. Nothing live is touched: the
//!   databases are open, and writing over them under a running homeCore could
//!   leave a house half one and half the other. homeCore puts the backup in
//!   place at its next start, before it opens anything ([`apply_pending`]),
//!   so restart it (`POST /system/restart`) to finish.
//!
//! ```sh
//! curl -X POST --data-binary @backup.zip \
//!      -H "Authorization: Bearer $TOKEN" \
//!      http://localhost:8080/api/v1/system/restore
//! ```
//!
//! The route carries `DefaultBodyLimit::max(500 MiB)`, so an archive larger than
//! that is rejected with 413 before the handler runs.

use axum::{
    body::Bytes,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde_json::json;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use tracing::{info, warn};
use zip::{write::SimpleFileOptions, ZipWriter};

use crate::{audit, auth_middleware::AuthUser, AppState};

/// Paths threaded from main.rs into AppState so the backup handler can find them.
#[derive(Clone)]
pub struct BackupPaths {
    pub state_db_path: PathBuf,
    pub history_db_path: PathBuf,
    pub config_path: PathBuf,
    pub rules_dir: PathBuf,
    /// One entry per registered plugin. On backup, each entry's file is
    /// archived under `plugins/<id>/config.toml`; on restore, the
    /// destination path is looked up by `id` against this same table —
    /// so cross-host restores work even when the absolute paths differ.
    pub plugin_configs: Vec<PluginConfigEntry>,
    /// Where the calendars live, if anywhere.
    pub calendars_dir: Option<PathBuf>,
    /// Backups homeCore makes by itself (`[backup]`), and where it keeps them.
    pub kept: Option<KeptBackups>,
}

/// `[backup]`, resolved: how often, how many, and the folder.
#[derive(Clone)]
pub struct KeptBackups {
    pub dir: PathBuf,
    /// 0 makes none on a schedule.
    pub every_hours: u32,
    pub keep: u32,
}

const KEPT_PREFIX: &str = "homecore-backup-";

fn kept_name_ok(name: &str) -> bool {
    name.starts_with(KEPT_PREFIX)
        && name.ends_with(".zip")
        && !name.contains(['/', '\\'])
        && !name.contains("..")
}

/// The kept backups, newest first: (name, bytes, modified).
fn kept_list(dir: &Path) -> Vec<(String, u64, chrono::DateTime<Utc>)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<_> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if !kept_name_ok(&name) {
                return None;
            }
            let md = e.metadata().ok()?;
            let at = md.modified().ok().map(chrono::DateTime::<Utc>::from)?;
            Some((name, md.len(), at))
        })
        .collect();
    // The name carries the time, so it sorts as well as the clock does.
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out
}

/// Make one backup into the folder and drop the oldest past `keep`.
fn make_kept(paths: &BackupPaths, kept: &KeptBackups) -> anyhow::Result<(String, usize)> {
    std::fs::create_dir_all(&kept.dir)?;
    let bytes = build_zip(paths)?;
    let name = format!("{KEPT_PREFIX}{}.zip", Utc::now().format("%Y%m%dT%H%M%SZ"));
    let tmp = kept.dir.join(format!(".{name}.part"));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, kept.dir.join(&name))?;
    for (old, _, _) in kept_list(&kept.dir)
        .into_iter()
        .skip(kept.keep.max(1) as usize)
    {
        if let Err(e) = std::fs::remove_file(kept.dir.join(&old)) {
            warn!(file = %old, error = %e, "Could not remove an old backup");
        }
    }
    Ok((name, bytes.len()))
}

/// Make backups on the `[backup]` schedule, for as long as homeCore runs.
/// The first comes one interval after the newest one already kept, so a
/// restart doesn't make an extra.
pub fn spawn_schedule(state: AppState) {
    let Some(paths) = state.backup_paths.clone() else {
        return;
    };
    let Some(kept) = paths.kept.clone().filter(|k| k.every_hours > 0) else {
        return;
    };
    tokio::spawn(async move {
        let every = chrono::Duration::hours(i64::from(kept.every_hours));
        loop {
            let newest = {
                let dir = kept.dir.clone();
                tokio::task::spawn_blocking(move || kept_list(&dir).first().map(|k| k.2))
                    .await
                    .ok()
                    .flatten()
            };
            let wait = newest
                .map(|at| (at + every - Utc::now()).to_std().unwrap_or_default())
                .unwrap_or_default();
            tokio::time::sleep(wait).await;
            let (p, k) = (paths.clone(), kept.clone());
            match tokio::task::spawn_blocking(move || make_kept(&p, &k)).await {
                Ok(Ok((name, bytes))) => {
                    info!(file = %name, bytes, "Scheduled backup made");
                    let mut e = hc_state::AuditEntry::success(
                        hc_state::AuditActorType::System,
                        None,
                        "system",
                        "system.backup_created",
                    )
                    .with_target("system", "backup");
                    e.detail = json!({ "filename": name, "bytes": bytes, "scheduled": true });
                    audit::emit(&state, e).await;
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "Scheduled backup failed; trying again in an hour");
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                }
                Err(e) => {
                    warn!(error = %e, "Scheduled backup task panicked");
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                }
            }
        }
    });
}

/// `GET /system/backups` — the schedule and the backups kept on the house.
pub async fn list_kept_handler(
    State(state): State<AppState>,
    AuthUser(claims): AuthUser,
) -> Response {
    if !claims.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "admin role required" })),
        )
            .into_response();
    }
    let Some(kept) = state.backup_paths.as_ref().and_then(|p| p.kept.clone()) else {
        return Json(json!({ "every_hours": 0, "keep": 0, "dir": null, "backups": [] }))
            .into_response();
    };
    let dir = kept.dir.clone();
    let list = tokio::task::spawn_blocking(move || kept_list(&dir))
        .await
        .unwrap_or_default();
    Json(json!({
        "every_hours": kept.every_hours,
        "keep": kept.keep,
        "dir": kept.dir.display().to_string(),
        "backups": list
            .into_iter()
            .map(|(name, bytes, at)| json!({ "name": name, "bytes": bytes, "created_at": at }))
            .collect::<Vec<_>>(),
    }))
    .into_response()
}

/// `GET /system/backups/{name}` — one kept backup, as a download.
pub async fn download_kept_handler(
    State(state): State<AppState>,
    AuthUser(claims): AuthUser,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    if !claims.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "admin role required" })),
        )
            .into_response();
    }
    let Some(kept) = state.backup_paths.as_ref().and_then(|p| p.kept.clone()) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such backup" })),
        )
            .into_response();
    };
    if !kept_name_ok(&name) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such backup" })),
        )
            .into_response();
    }
    match tokio::fs::read(kept.dir.join(&name)).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/zip".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{name}\""),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such backup" })),
        )
            .into_response(),
    }
}

impl BackupPaths {
    /// Where a staged restore waits for the next start.
    pub fn pending(&self) -> PathBuf {
        self.state_db_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(PENDING)
    }
}

/// The staged restore's file name, beside the state database.
pub const PENDING: &str = "restore-pending.zip";
/// What a staged restore becomes once applied — kept, one deep, so the last
/// restore can be looked at afterwards.
pub const APPLIED: &str = "restore-applied.zip";

/// A single plugin's config file location, threaded into `BackupPaths`.
#[derive(Clone)]
pub struct PluginConfigEntry {
    /// Plugin id, e.g. `"plugin.hue"`. Used as the archive key.
    pub id: String,
    /// Resolved absolute path on the running host.
    pub path: PathBuf,
}

pub async fn backup_handler(State(state): State<AppState>, AuthUser(claims): AuthUser) -> Response {
    if !claims.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "admin role required" })),
        )
            .into_response();
    }

    let paths = match &state.backup_paths {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "backup paths not configured" })),
            )
                .into_response();
        }
    };

    let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let filename = format!("homecore-backup-{timestamp}.zip");

    let result = tokio::task::spawn_blocking(move || build_zip(&paths)).await;

    match result {
        Ok(Ok(bytes)) => {
            let mut audit_e = audit::entry_from_claims(&claims, "system.backup_created")
                .with_target("system", "backup");
            audit_e.detail = json!({
                "filename": filename,
                "bytes":    bytes.len(),
            });
            audit::emit(&state, audit_e).await;
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "application/zip"),
                    (
                        header::CONTENT_DISPOSITION,
                        &format!("attachment; filename=\"{filename}\""),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        Ok(Err(e)) => {
            warn!(error = %e, "Backup creation failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("backup failed: {e}") })),
            )
                .into_response()
        }
        Err(e) => {
            warn!(error = %e, "Backup task panicked");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "backup task panicked" })),
            )
                .into_response()
        }
    }
}

/// Build the zip archive synchronously (called inside `spawn_blocking`).
fn build_zip(paths: &BackupPaths) -> anyhow::Result<Vec<u8>> {
    let buf = Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(buf);
    // Every entry carries when the backup was taken (UTC), so a restore can
    // say how old it is. Without it zip stamps 1980.
    let now = Utc::now();
    let taken = {
        use chrono::{Datelike, Timelike};
        zip::DateTime::from_date_and_time(
            now.year() as u16,
            now.month() as u8,
            now.day() as u8,
            now.hour() as u8,
            now.minute() as u8,
            now.second() as u8,
        )
        .unwrap_or_default()
    };
    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(taken);

    // ── Databases ──────────────────────────────────────────────────────────
    add_file(&mut zip, &paths.state_db_path, "state.redb", opts)?;
    add_file(&mut zip, &paths.history_db_path, "history.db", opts)?;

    // ── Config files ───────────────────────────────────────────────────────
    add_file_opt(&mut zip, &paths.config_path, "config/homecore.toml", opts)?;
    // modes.toml lives in the same directory as the main config
    if let Some(parent) = paths.config_path.parent() {
        add_file_opt(
            &mut zip,
            &parent.join("modes.toml"),
            "config/modes.toml",
            opts,
        )?;
        add_file_opt(
            &mut zip,
            &parent.join("mode_definitions.json"),
            "config/mode_definitions.json",
            opts,
        )?;
    }

    // ── Rule files ─────────────────────────────────────────────────────────
    if paths.rules_dir.is_dir() {
        let entries = std::fs::read_dir(&paths.rules_dir)?;
        let mut rule_files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .is_some_and(|ext| ext == "ron" || ext == "toml")
            })
            .collect();
        rule_files.sort();
        for path in rule_files {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let archive_name = format!("rules/{name}");
                add_file(&mut zip, &path, &archive_name, opts)?;
            }
        }
    }

    // ── Plugin configs ─────────────────────────────────────────────────────
    // Optional per entry — a freshly-added plugin may not have written its
    // config file yet, in which case add_file_opt skips silently.
    for plugin in &paths.plugin_configs {
        let archive_name = format!("plugins/{}/config.toml", plugin.id);
        add_file_opt(&mut zip, &plugin.path, &archive_name, opts)?;
    }

    // ── Calendars ──────────────────────────────────────────────────────────
    if let Some(dir) = paths.calendars_dir.as_ref().filter(|d| d.is_dir()) {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        for path in files {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                add_file_opt(&mut zip, &path, &format!("calendars/{name}"), opts)?;
            }
        }
    }

    let cursor = zip.finish()?;
    Ok(cursor.into_inner())
}

fn add_file(
    zip: &mut ZipWriter<Cursor<Vec<u8>>>,
    path: &Path,
    archive_name: &str,
    opts: SimpleFileOptions,
) -> anyhow::Result<()> {
    let data = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
    zip.start_file(archive_name, opts)?;
    zip.write_all(&data)?;
    Ok(())
}

fn add_file_opt(
    zip: &mut ZipWriter<Cursor<Vec<u8>>>,
    path: &Path,
    archive_name: &str,
    opts: SimpleFileOptions,
) -> anyhow::Result<()> {
    match std::fs::read(path) {
        Ok(data) => {
            zip.start_file(archive_name, opts)?;
            zip.write_all(&data)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Optional file absent — skip silently
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Could not read optional backup file");
        }
    }
    Ok(())
}

// ── Restore ──────────────────────────────────────────────────────────────────

/// `POST /system/restore` — check a backup (`?check=true`), or stage it to be
/// put in place when homeCore next starts.
#[derive(Debug, Default, serde::Deserialize)]
pub struct RestoreQuery {
    #[serde(default)]
    pub check: bool,
}

pub async fn restore_handler(
    State(state): State<AppState>,
    AuthUser(claims): AuthUser,
    axum::extract::Query(q): axum::extract::Query<RestoreQuery>,
    body: Bytes,
) -> Response {
    if !claims.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "admin role required" })),
        )
            .into_response();
    }

    let paths = match &state.backup_paths {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "backup paths not configured" })),
            )
                .into_response();
        }
    };

    if body.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "empty request body" })),
        )
            .into_response();
    }

    restore_from(&state, &claims, paths, body, q.check, None).await
}

/// `POST /system/backups/{name}/restore` — check or stage one of the backups
/// kept on the house, as `POST /system/restore` does an uploaded one, without
/// the round trip through a browser.
pub async fn restore_kept_handler(
    State(state): State<AppState>,
    AuthUser(claims): AuthUser,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<RestoreQuery>,
) -> Response {
    if !claims.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "admin role required" })),
        )
            .into_response();
    }
    let Some(paths) = state.backup_paths.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "backup paths not configured" })),
        )
            .into_response();
    };
    let Some(kept) = paths.kept.clone().filter(|_| kept_name_ok(&name)) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such backup" })),
        )
            .into_response();
    };
    match tokio::fs::read(kept.dir.join(&name)).await {
        Ok(bytes) => {
            restore_from(
                &state,
                &claims,
                paths,
                Bytes::from(bytes),
                q.check,
                Some(name),
            )
            .await
        }
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such backup" })),
        )
            .into_response(),
    }
}

/// Check an archive, and unless only checking, stage it for the next start.
async fn restore_from(
    state: &AppState,
    claims: &hc_auth::Claims,
    paths: BackupPaths,
    body: Bytes,
    check: bool,
    from: Option<String>,
) -> Response {
    let zip_len = body.len();
    let staged = tokio::task::spawn_blocking(move || -> anyhow::Result<RestorePlan> {
        let plan = plan_restore(&paths, &body)?;
        if !check {
            stage(&paths, &body)?;
        }
        Ok(plan)
    })
    .await;

    match staged {
        Ok(Ok(plan)) => {
            if check {
                return (
                    StatusCode::OK,
                    Json(json!({
                        "restored": plan.restored,
                        "skipped": plan.skipped,
                        "taken_at": plan.taken_at,
                    })),
                )
                    .into_response();
            }
            let mut audit_e = audit::entry_from_claims(claims, "system.restore_staged")
                .with_target("system", "backup");
            audit_e.detail = json!({
                "bytes": zip_len,
                "restored": plan.restored.len(),
                "taken_at": plan.taken_at,
                "kept": from,
            });
            audit::emit(state, audit_e).await;
            (
                StatusCode::OK,
                Json(json!({
                    "staged": true,
                    "restored": plan.restored,
                    "skipped": plan.skipped,
                    "taken_at": plan.taken_at,
                    "message": "Staged. homeCore puts this backup in place when it next starts — restart it to finish.",
                })),
            )
                .into_response()
        }
        Ok(Err(e)) => {
            warn!(error = %e, "Restore refused");
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
        Err(e) => {
            warn!(error = %e, "Restore task panicked");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "restore task panicked" })),
            )
                .into_response()
        }
    }
}

/// What a backup would put back, and what it would skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    pub restored: Vec<String>,
    pub skipped: Vec<String>,
    /// When the backup was taken, as the archive's state database says.
    pub taken_at: Option<String>,
}

/// Where an archive entry goes on this host, or `None` to skip it.
fn destination(paths: &BackupPaths, name: &Path) -> Option<PathBuf> {
    let name_str = name.display().to_string();
    match name_str.as_str() {
        "state.redb" => Some(paths.state_db_path.clone()),
        "history.db" => Some(paths.history_db_path.clone()),
        "config/homecore.toml" => Some(paths.config_path.clone()),
        "config/modes.toml" => paths.config_path.parent().map(|p| p.join("modes.toml")),
        "config/mode_definitions.json" => paths
            .config_path
            .parent()
            .map(|p| p.join("mode_definitions.json")),
        // e.g. "rules/my_rule.ron" → rules_dir/my_rule.ron
        _ if name_str.starts_with("rules/") => name.file_name().map(|n| paths.rules_dir.join(n)),
        _ if name_str.starts_with("calendars/") => {
            let dir = paths.calendars_dir.as_ref()?;
            name.file_name().map(|n| dir.join(n))
        }
        // Archive shape: "plugins/<plugin_id>/config.toml". The destination
        // comes from the running host's [[plugins]] block — ids match across
        // hosts, paths don't. An id this host doesn't run is skipped.
        _ if name_str.starts_with("plugins/") => {
            let id = name_str
                .strip_prefix("plugins/")
                .and_then(|s| s.strip_suffix("/config.toml"))
                .unwrap_or("");
            paths
                .plugin_configs
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.path.clone())
        }
        _ => None,
    }
}

/// Read an archive and say what it would do, changing nothing. Refuses one
/// that isn't a homeCore backup.
pub fn plan_restore(paths: &BackupPaths, zip_bytes: &[u8]) -> anyhow::Result<RestorePlan> {
    let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes))
        .map_err(|e| anyhow::anyhow!("invalid ZIP archive: {e}"))?;
    let mut plan = RestorePlan {
        restored: Vec::new(),
        skipped: Vec::new(),
        taken_at: None,
    };
    for i in 0..archive.len() {
        let file = archive.by_index(i)?;
        let Some(name) = file.enclosed_name().map(|p| p.to_path_buf()) else {
            plan.skipped.push(format!("unsafe path at index {i}"));
            continue;
        };
        let name_str = name.display().to_string();
        if name_str == "state.redb" {
            // Backups from before entries were stamped say 1980: unknown.
            plan.taken_at = file
                .last_modified()
                .filter(|t| t.year() > 1980)
                .and_then(|t| {
                    chrono::NaiveDate::from_ymd_opt(
                        t.year().into(),
                        t.month().into(),
                        t.day().into(),
                    )?
                    .and_hms_opt(
                        t.hour().into(),
                        t.minute().into(),
                        t.second().into(),
                    )
                })
                .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        }
        match destination(paths, &name) {
            Some(_) => plan.restored.push(name_str),
            None => plan.skipped.push(name_str),
        }
    }
    if !plan.restored.iter().any(|n| n == "state.redb") {
        anyhow::bail!("this isn't a homeCore backup: it has no state.redb");
    }
    Ok(plan)
}

/// Put an archive beside the state database to be applied at the next start.
fn stage(paths: &BackupPaths, zip_bytes: &[u8]) -> anyhow::Result<()> {
    let dest = paths.pending();
    let tmp = dest.with_extension("zip.tmp");
    std::fs::write(&tmp, zip_bytes)
        .map_err(|e| anyhow::anyhow!("failed to stage {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &dest)
        .map_err(|e| anyhow::anyhow!("failed to stage {}: {e}", dest.display()))?;
    info!(path = %dest.display(), "Restore staged for the next start");
    Ok(())
}

/// At start, before anything is opened: put a staged restore in place.
/// Returns what it did, or `None` when nothing was staged. The staged archive
/// is kept as `restore-applied.zip`, so it isn't applied twice.
pub fn apply_pending(paths: &BackupPaths) -> Option<anyhow::Result<serde_json::Value>> {
    let pending = paths.pending();
    let bytes = match std::fs::read(&pending) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(Err(anyhow::anyhow!("reading {}: {e}", pending.display()))),
    };
    let result = plan_restore(paths, &bytes).and_then(|_| extract_restore(paths, &bytes));
    // Applied or not, it mustn't be tried again on every start.
    let applied = pending.with_file_name(APPLIED);
    if let Err(e) = std::fs::rename(&pending, &applied) {
        warn!(error = %e, "could not set the staged restore aside");
        let _ = std::fs::remove_file(&pending);
    }
    Some(result)
}

/// Copy a backup's files to their places on this host.
fn extract_restore(paths: &BackupPaths, zip_bytes: &[u8]) -> anyhow::Result<serde_json::Value> {
    let reader = Cursor::new(zip_bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| anyhow::anyhow!("invalid ZIP archive: {e}"))?;

    let mut restored = Vec::new();
    let mut skipped = Vec::new();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let Some(name) = file.enclosed_name().map(|p| p.to_path_buf()) else {
            skipped.push(format!("unsafe path at index {i}"));
            continue;
        };
        let name_str = name.display().to_string();
        let Some(dest) = destination(paths, &name) else {
            skipped.push(name_str);
            continue;
        };

        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        std::fs::write(&dest, &buf)
            .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", dest.display()))?;

        info!(file = %name_str, dest = %dest.display(), "Restored file from backup");
        restored.push(name_str);
    }

    Ok(json!({
        "restored": restored,
        "skipped": skipped,
    }))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Build a `BackupPaths` rooted at `dir`, with the per-file destinations
    /// already created under `dir`. `plugin_configs` is provided by the caller
    /// because each test wants different shapes.
    fn make_paths(dir: &Path, plugin_configs: Vec<PluginConfigEntry>) -> BackupPaths {
        let state_db = dir.join("state.redb");
        let history_db = dir.join("history.db");
        let config_path = dir.join("config/homecore.toml");
        let rules_dir = dir.join("rules");
        fs::create_dir_all(rules_dir.parent().unwrap()).unwrap();
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::create_dir_all(&rules_dir).unwrap();
        fs::write(&state_db, b"redb-bytes").unwrap();
        fs::write(&history_db, b"sqlite-bytes").unwrap();
        fs::write(&config_path, b"# core config\n").unwrap();
        BackupPaths {
            state_db_path: state_db,
            history_db_path: history_db,
            config_path,
            rules_dir,
            plugin_configs,
            calendars_dir: Some(dir.join("config/calendars")),
            kept: None,
        }
    }

    #[test]
    fn kept_backups_are_pruned_to_keep_and_only_ours_are_served() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_paths(tmp.path(), Vec::new());
        let dir = tmp.path().join("backups");
        fs::create_dir_all(&dir).unwrap();
        for n in ["20200101T000000Z", "20200102T000000Z", "20200103T000000Z"] {
            fs::write(dir.join(format!("homecore-backup-{n}.zip")), b"old").unwrap();
        }
        fs::write(dir.join("notes.txt"), b"mine").unwrap();
        let kept = KeptBackups {
            dir: dir.clone(),
            every_hours: 24,
            keep: 2,
        };
        let (name, _) = make_kept(&paths, &kept).unwrap();
        let left: Vec<String> = kept_list(&dir).into_iter().map(|k| k.0).collect();
        assert_eq!(
            left,
            vec![name, "homecore-backup-20200103T000000Z.zip".to_string()]
        );
        assert!(dir.join("notes.txt").exists());
        assert!(!kept_name_ok("homecore-backup-../../etc.zip"));
        assert!(!kept_name_ok("state.redb"));
    }

    fn write_plugin_config(dir: &Path, id: &str, contents: &str) -> PluginConfigEntry {
        let p = dir.join(format!("plugins/{id}/config.toml"));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, contents).unwrap();
        PluginConfigEntry {
            id: id.to_string(),
            path: p,
        }
    }

    /// An archive written by the previous zip major must still restore.
    ///
    /// A backup is the thing an operator reaches for when something has
    /// already gone wrong, and it may be months old — so "can this build read
    /// what the last one wrote" is the property, not "can it read its own
    /// output". The fixture was produced by the zip 2 build this repo shipped
    /// through 0.1.26; regenerate it only when adding a *new* old-format case,
    /// never to make a failure pass.
    #[test]
    fn an_archive_written_by_the_previous_zip_major_still_restores() {
        let bytes = include_bytes!("../tests/fixtures/backup-written-by-zip2.zip");
        let dir = tempdir().unwrap();
        let paths = make_paths(dir.path(), Vec::new());

        let report = extract_restore(&paths, bytes).expect("a zip-2 archive must restore");

        let restored = report["restored"]
            .as_array()
            .expect("restored list")
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(",");
        assert!(restored.contains("state.redb"), "{restored}");
        assert!(restored.contains("history.db"), "{restored}");

        // Contents, not just names — a restore that writes empty files would
        // otherwise look like a success.
        assert_eq!(
            fs::read(&paths.state_db_path).unwrap(),
            b"not-really-redb-but-bytes\x00\x01\x02".to_vec(),
            "state.redb did not come back byte for byte"
        );
    }

    #[test]
    fn backup_archive_includes_plugin_configs() {
        let dir = tempdir().unwrap();
        let hue = write_plugin_config(dir.path(), "plugin.hue", "bridge_ip = \"10.0.0.5\"\n");
        let yolink = write_plugin_config(dir.path(), "plugin.yolink", "uaid = \"abc\"\n");
        let paths = make_paths(dir.path(), vec![hue, yolink]);

        let bytes = build_zip(&paths).expect("build_zip");

        // Inspect the archive to confirm the plugin entries are present
        // with the right contents.
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"plugins/plugin.hue/config.toml".to_string()));
        assert!(names.contains(&"plugins/plugin.yolink/config.toml".to_string()));

        let mut hue_file = zip.by_name("plugins/plugin.hue/config.toml").unwrap();
        let mut hue_buf = String::new();
        hue_file.read_to_string(&mut hue_buf).unwrap();
        assert!(hue_buf.contains("10.0.0.5"));
    }

    #[test]
    fn restore_writes_plugin_configs_translating_paths_across_hosts() {
        // Simulate a cross-host restore: source plugin paths live under
        // `src_dir`, destination paths under `dst_dir`. Same plugin ids on
        // both sides so the path translation should land each file at its
        // dest-host location.
        let src_dir = tempdir().unwrap();
        let dst_dir = tempdir().unwrap();

        let src_hue =
            write_plugin_config(src_dir.path(), "plugin.hue", "bridge_ip = \"10.0.0.5\"\n");
        let src_paths = make_paths(src_dir.path(), vec![src_hue]);
        let bytes = build_zip(&src_paths).expect("build_zip");

        // Dest host has a *different* path for the same plugin id.
        let dst_hue_path = dst_dir
            .path()
            .join("opt/homecore/plugins/hc-hue/config.toml");
        let dst_hue = PluginConfigEntry {
            id: "plugin.hue".to_string(),
            path: dst_hue_path.clone(),
        };
        let dst_paths = make_paths(dst_dir.path(), vec![dst_hue]);

        let summary = extract_restore(&dst_paths, &bytes).expect("extract_restore");
        assert!(summary["restored"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("plugins/plugin.hue/config.toml")));

        // File landed at the dest-host path with source-host contents.
        let written = fs::read_to_string(&dst_hue_path).unwrap();
        assert!(written.contains("10.0.0.5"));
    }

    #[test]
    fn restore_skips_plugin_id_not_registered_on_dest_host() {
        // Source has plugin.hue; dest host's [[plugins]] does NOT include it.
        // Expected: file is listed in `skipped`, not written.
        let src_dir = tempdir().unwrap();
        let dst_dir = tempdir().unwrap();

        let src_hue =
            write_plugin_config(src_dir.path(), "plugin.hue", "bridge_ip = \"10.0.0.5\"\n");
        let src_paths = make_paths(src_dir.path(), vec![src_hue]);
        let bytes = build_zip(&src_paths).expect("build_zip");

        // Dest has no plugins registered.
        let dst_paths = make_paths(dst_dir.path(), vec![]);
        let summary = extract_restore(&dst_paths, &bytes).expect("extract_restore");

        assert!(
            summary["skipped"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_str() == Some("plugins/plugin.hue/config.toml")),
            "plugin entry should be skipped when not registered on dest host: {summary}"
        );
        assert!(!summary["restored"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("plugins/plugin.hue/config.toml")));
    }

    #[test]
    fn backup_with_no_plugins_omits_plugin_section() {
        let dir = tempdir().unwrap();
        let paths = make_paths(dir.path(), vec![]);
        let bytes = build_zip(&paths).expect("build_zip");
        let zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.clone().by_index(i).unwrap().name().to_string())
            .collect();
        assert!(!names.iter().any(|n| n.starts_with("plugins/")));
    }

    #[test]
    fn a_restore_is_checked_then_staged_and_applied_at_the_next_start() {
        let src = tempdir().unwrap();
        let src_paths = make_paths(src.path(), vec![]);
        let cal = src.path().join("config/calendars");
        fs::create_dir_all(&cal).unwrap();
        fs::write(cal.join("bins.ics"), b"BEGIN:VCALENDAR\nEND:VCALENDAR\n").unwrap();
        fs::write(src.path().join("rules/porch.ron"), b"(name: \"porch\")").unwrap();
        let bytes = build_zip(&src_paths).unwrap();

        let dst = tempdir().unwrap();
        let paths = make_paths(dst.path(), vec![]);
        fs::write(&paths.state_db_path, b"live-redb").unwrap();

        // Checking says what would happen, and when it was taken, and changes nothing.
        let plan = plan_restore(&paths, &bytes).unwrap();
        let taken = plan
            .taken_at
            .clone()
            .expect("a backup says when it was taken");
        let year: i32 = taken[..4].parse().unwrap();
        assert!(year >= 2026, "{taken}");
        assert!(plan.restored.contains(&"state.redb".to_string()));
        assert!(plan.restored.contains(&"calendars/bins.ics".to_string()));
        assert!(plan.restored.contains(&"rules/porch.ron".to_string()));
        assert_eq!(fs::read(&paths.state_db_path).unwrap(), b"live-redb");

        // Staging leaves the live files alone too.
        stage(&paths, &bytes).unwrap();
        assert_eq!(fs::read(&paths.state_db_path).unwrap(), b"live-redb");
        assert!(paths.pending().exists());

        // The next start puts it in place, once.
        let done = apply_pending(&paths)
            .expect("something was staged")
            .unwrap();
        assert!(done["restored"].as_array().unwrap().len() >= 4, "{done}");
        assert_eq!(fs::read(&paths.state_db_path).unwrap(), b"redb-bytes");
        assert!(dst.path().join("config/calendars/bins.ics").exists());
        assert!(!paths.pending().exists());
        assert!(dst.path().join(APPLIED).exists());
        assert!(apply_pending(&paths).is_none(), "applied only once");
    }

    #[test]
    fn something_that_isnt_a_backup_is_refused() {
        let dir = tempdir().unwrap();
        let paths = make_paths(dir.path(), vec![]);
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("holiday.jpg", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"jpeg").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        let err = plan_restore(&paths, &bytes).unwrap_err().to_string();
        assert!(err.contains("isn't a homeCore backup"), "{err}");
        assert!(plan_restore(&paths, b"not a zip").is_err());
    }
}
