//! `POST /system/backup` — create a zip archive of all persistent state and config.
//!
//! The archive contains:
//! - `state.redb`        — device registry, rules, scenes, areas, users (redb)
//! - `history.db`        — time-series state history (SQLite)
//! - `config/homecore.toml` — main config file (if present)
//! - `config/modes.toml`    — modes config (if present)
//! - `rules/*.toml`         — all rule files (if present)
//!
//! The archive is streamed as `application/zip` with a timestamped filename.
//! Requires Admin role.

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde_json::json;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use tracing::warn;
use zip::{write::SimpleFileOptions, ZipWriter};

use crate::{auth_middleware::AuthUser, AppState};

/// Paths threaded from main.rs into AppState so the backup handler can find them.
#[derive(Clone)]
pub struct BackupPaths {
    pub state_db_path: PathBuf,
    pub history_db_path: PathBuf,
    pub config_path: PathBuf,
    pub rules_dir: PathBuf,
}

pub async fn backup_handler(
    State(state): State<AppState>,
    AuthUser(claims): AuthUser,
) -> Response {
    if !claims.is_admin() {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "admin role required" }))).into_response();
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
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    // ── Databases ──────────────────────────────────────────────────────────
    add_file(&mut zip, &paths.state_db_path, "state.redb", opts)?;
    add_file(&mut zip, &paths.history_db_path, "history.db", opts)?;

    // ── Config files ───────────────────────────────────────────────────────
    add_file_opt(&mut zip, &paths.config_path, "config/homecore.toml", opts)?;
    // modes.toml lives in the same directory as the main config
    if let Some(parent) = paths.config_path.parent() {
        add_file_opt(&mut zip, &parent.join("modes.toml"), "config/modes.toml", opts)?;
    }

    // ── Rule files ─────────────────────────────────────────────────────────
    if paths.rules_dir.is_dir() {
        let entries = std::fs::read_dir(&paths.rules_dir)?;
        let mut rule_files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |ext| ext == "toml"))
            .collect();
        rule_files.sort();
        for path in rule_files {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let archive_name = format!("rules/{name}");
                add_file(&mut zip, &path, &archive_name, opts)?;
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
