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
//!
//! The archive is streamed as `application/zip` with a timestamped filename.
//! Requires Admin role.
//!
//! ## Restore (`POST /system/restore`)
//!
//! Accepts a JSON body with base64-encoded ZIP content.  Extracts to a staging
//! directory, validates the archive structure, then copies files into place.
//! A server restart is required after restore for changes to take effect.
//! Requires Admin role.

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

use crate::{auth_middleware::AuthUser, AppState};

/// Paths threaded from main.rs into AppState so the backup handler can find them.
#[derive(Clone)]
pub struct BackupPaths {
    pub state_db_path: PathBuf,
    pub history_db_path: PathBuf,
    pub config_path: PathBuf,
    pub rules_dir: PathBuf,
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
        Ok(Ok(bytes)) => (
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
            .into_response(),
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

/// `POST /system/restore` — accept a raw ZIP body and restore files from it.
///
/// Validates the archive, then overwrites matching files in their canonical
/// locations.  A server restart is required afterwards.
pub async fn restore_handler(
    State(state): State<AppState>,
    AuthUser(claims): AuthUser,
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

    let zip_bytes = body.to_vec();

    match tokio::task::spawn_blocking(move || extract_restore(&paths, &zip_bytes)).await {
        Ok(Ok(summary)) => (StatusCode::OK, Json(summary)).into_response(),
        Ok(Err(e)) => {
            warn!(error = %e, "Restore failed");
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

/// Validate and extract a backup ZIP, copying files to their canonical locations.
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

        // Map archive path → filesystem destination.
        let dest = match name_str.as_str() {
            "state.redb" => Some(paths.state_db_path.clone()),
            "history.db" => Some(paths.history_db_path.clone()),
            "config/homecore.toml" => Some(paths.config_path.clone()),
            "config/modes.toml" => paths.config_path.parent().map(|p| p.join("modes.toml")),
            "config/mode_definitions.json" => paths
                .config_path
                .parent()
                .map(|p| p.join("mode_definitions.json")),
            _ if name_str.starts_with("rules/") => {
                // e.g. "rules/my_rule.ron" → rules_dir/my_rule.ron
                let filename = name.file_name().map(|n| n.to_os_string());
                filename.map(|n| paths.rules_dir.join(n))
            }
            _ => {
                skipped.push(name_str);
                continue;
            }
        };

        let Some(dest) = dest else {
            skipped.push(name_str);
            continue;
        };

        // Ensure parent directory exists.
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
        "message": "Restore complete. Please restart the HomeCore server for changes to take effect.",
    }))
}
