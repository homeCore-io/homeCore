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
}

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

    // ── Plugin configs ─────────────────────────────────────────────────────
    // Optional per entry — a freshly-added plugin may not have written its
    // config file yet, in which case add_file_opt skips silently.
    for plugin in &paths.plugin_configs {
        let archive_name = format!("plugins/{}/config.toml", plugin.id);
        add_file_opt(&mut zip, &plugin.path, &archive_name, opts)?;
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

    let zip_len = zip_bytes.len();
    match tokio::task::spawn_blocking(move || extract_restore(&paths, &zip_bytes)).await {
        Ok(Ok(summary)) => {
            let restored_count = summary
                .get("restored")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let mut audit_e = audit::entry_from_claims(&claims, "system.restore_completed")
                .with_target("system", "backup");
            audit_e.detail = json!({
                "bytes":    zip_len,
                "restored": restored_count,
            });
            audit::emit(&state, audit_e).await;
            (StatusCode::OK, Json(summary)).into_response()
        }
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
            _ if name_str.starts_with("plugins/") => {
                // Archive shape: "plugins/<plugin_id>/config.toml"
                // Look up the destination path from the running host's
                // [[plugins]] block — IDs match across hosts, paths don't.
                // An ID not registered locally is skipped (the source host
                // had a plugin this host doesn't run); an ID registered
                // locally writes to whatever path local config dictates.
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
        }
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
}
