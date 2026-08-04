//! One-time redb file-format migration, v2 → v3.
//!
//! redb 3 changed its on-disk format and both redb 3 and redb 4 **refuse** to
//! open a file written by redb 2:
//!
//! ```text
//! Manual upgrade required. Expected file format version 3, but file is version 2
//! ```
//!
//! There is no automatic path, and no generic one either: redb records each
//! table's key and value type names and rejects an open that disagrees, so a
//! byte-level copy is impossible. Every table has to be named with its real
//! types, which is what [`copy_tables`] does.
//!
//! # Safety of the operation
//!
//! Nothing is destroyed. The migration writes a brand-new file, fsyncs it,
//! moves the original aside to `<name>.v2-backup`, and only then renames the
//! new file into place. A failure at any point leaves the original where it
//! was — the process aborts with an error and the operator still has a
//! working database with the previous core.
//!
//! The backup is deliberately **not** cleaned up. It is a house's device
//! registry, users, API keys and rules; a few megabytes is a cheap insurance
//! premium against a migration bug nobody noticed for a week.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{info, warn};

/// Tables whose keys and values are both UTF-8 strings — all but one.
const STR_TABLES: &[&str] = &[
    "api_keys_by_id",
    "api_keys_by_prefix",
    "areas",
    "battery_state",
    "devices",
    "plugin_state",
    "refresh_tokens_by_id",
    "refresh_tokens_by_prefix",
    "rules",
    "scenes",
    "users_by_id",
    "users_by_name",
];

/// Tables with string keys and opaque byte values.
const BYTES_TABLES: &[&str] = &["device_schemas"];

/// What a migration moved, for logging and for the tests to assert on.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// `(table, rows)` for every table that existed and was copied.
    pub tables: Vec<(String, usize)>,
    /// Where the pre-migration file was kept.
    pub backup: PathBuf,
}

impl MigrationReport {
    pub fn total_rows(&self) -> usize {
        self.tables.iter().map(|(_, n)| n).sum()
    }
}

/// Migrate `path` from redb 2's format to the current one, if it needs it.
///
/// Returns `Ok(None)` when there is nothing to do — the usual case, including
/// a database that does not exist yet.
pub fn migrate_if_needed(path: &Path) -> Result<Option<MigrationReport>> {
    if !path.exists() {
        return Ok(None);
    }
    if !needs_migration(path) {
        return Ok(None);
    }

    info!(
        db = %path.display(),
        "State database is in redb's v2 format — migrating before startup"
    );

    let working = path.with_extension("migrating");
    let backup = backup_path(path);

    // A leftover from an interrupted previous attempt is not a valid database.
    if working.exists() {
        warn!(path = %working.display(), "Removing a partial migration from an earlier run");
        std::fs::remove_file(&working).ok();
    }
    if backup.exists() {
        anyhow::bail!(
            "{} already exists — a previous migration left it behind. Move it aside \
             (or delete it if the current database is good) and start again.",
            backup.display()
        );
    }

    let report = copy_tables(path, &working, backup.clone())
        .inspect_err(|_| {
            // Never leave a half-written file where a later run could mistake
            // it for a real database.
            std::fs::remove_file(&working).ok();
        })
        .context("copying tables into the new-format database")?;

    // Both the new file and the directory entry must be durable before the
    // original is moved: a crash between the two must not lose both.
    fsync_file(&working)?;
    std::fs::rename(path, &backup)
        .with_context(|| format!("moving the old database to {}", backup.display()))?;
    std::fs::rename(&working, path).with_context(|| {
        format!(
            "moving the migrated database into place — the original is safe at {}",
            backup.display()
        )
    })?;
    fsync_dir(path)?;

    info!(
        tables = report.tables.len(),
        rows = report.total_rows(),
        backup = %backup.display(),
        "State database migrated to the current redb format"
    );
    Ok(Some(report))
}

/// True when the file is a redb database this build cannot open but redb 2 can.
///
/// Deliberately narrow: it opens with redb 2 and only reports `true` if that
/// succeeds. A file that neither version can read is left alone for the normal
/// open path to fail on, with redb's own error, rather than being "migrated"
/// into something worse.
fn needs_migration(path: &Path) -> bool {
    if redb::Database::open(path).is_ok() {
        return false;
    }
    redb2::Database::open(path).is_ok()
}

fn backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "state.redb".into());
    path.with_file_name(format!("{name}.v2-backup"))
}

/// Copy every known table from a redb 2 database into a new one.
///
/// A table that isn't present is skipped, not an error: they are created
/// lazily, so a database from an older core legitimately lacks some.
fn copy_tables(src: &Path, dst: &Path, backup: PathBuf) -> Result<MigrationReport> {
    use redb2::ReadableTable as _;

    let old = redb2::Database::open(src).context("opening the existing database with redb 2")?;
    let new = redb::Database::create(dst).context("creating the new-format database")?;

    let read = old.begin_read()?;
    let write = new.begin_write()?;
    let mut tables = Vec::new();

    for name in STR_TABLES {
        let def_old: redb2::TableDefinition<&str, &str> = redb2::TableDefinition::new(name);
        let Ok(t_old) = read.open_table(def_old) else {
            continue; // table absent in this database
        };
        let def_new: redb::TableDefinition<&str, &str> = redb::TableDefinition::new(name);
        let mut t_new = write.open_table(def_new)?;
        let mut rows = 0usize;
        for entry in t_old.iter()? {
            let (k, v) = entry?;
            t_new.insert(k.value(), v.value())?;
            rows += 1;
        }
        drop(t_new);
        tables.push(((*name).to_string(), rows));
    }

    for name in BYTES_TABLES {
        let def_old: redb2::TableDefinition<&str, &[u8]> = redb2::TableDefinition::new(name);
        let Ok(t_old) = read.open_table(def_old) else {
            continue;
        };
        let def_new: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new(name);
        let mut t_new = write.open_table(def_new)?;
        let mut rows = 0usize;
        for entry in t_old.iter()? {
            let (k, v) = entry?;
            t_new.insert(k.value(), v.value())?;
            rows += 1;
        }
        drop(t_new);
        tables.push(((*name).to_string(), rows));
    }

    write.commit().context("committing the migrated tables")?;
    Ok(MigrationReport { tables, backup })
}

fn fsync_file(path: &Path) -> Result<()> {
    let f = std::fs::File::open(path)
        .with_context(|| format!("reopening {} to flush it", path.display()))?;
    f.sync_all()
        .with_context(|| format!("flushing {}", path.display()))
}

/// Flush the *directory* so the renames themselves survive a power cut.
fn fsync_dir(path: &Path) -> Result<()> {
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    let f = std::fs::File::open(dir)
        .with_context(|| format!("opening {} to flush it", dir.display()))?;
    f.sync_all()
        .with_context(|| format!("flushing directory {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::ReadableDatabase as _;

    /// Build a database in redb 2's format holding a row in every table this
    /// server uses, so the migration is exercised against the real shape
    /// rather than one table.
    fn write_v2_database(path: &Path) {
        let db = redb2::Database::create(path).unwrap();
        let w = db.begin_write().unwrap();
        for name in STR_TABLES {
            let def: redb2::TableDefinition<&str, &str> = redb2::TableDefinition::new(name);
            let mut t = w.open_table(def).unwrap();
            t.insert(
                format!("{name}-key-1").as_str(),
                format!("{name}-value-1").as_str(),
            )
            .unwrap();
            t.insert(format!("{name}-key-2").as_str(), r#"{"nested":"json &<>"}"#)
                .unwrap();
        }
        for name in BYTES_TABLES {
            let def: redb2::TableDefinition<&str, &[u8]> = redb2::TableDefinition::new(name);
            let mut t = w.open_table(def).unwrap();
            t.insert("schema-1", [0u8, 159, 146, 150].as_slice())
                .unwrap();
        }
        w.commit().unwrap();
    }

    #[test]
    fn redb4_cannot_open_a_v2_file_which_is_why_this_module_exists() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.redb");
        write_v2_database(&db);

        let err = redb::Database::open(&db).expect_err("redb 4 must refuse a v2 file");
        assert!(
            err.to_string().contains("version"),
            "unexpected refusal: {err}"
        );
        assert!(needs_migration(&db));
    }

    #[test]
    fn every_row_survives_the_migration() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.redb");
        write_v2_database(&db);

        let report = migrate_if_needed(&db).unwrap().expect("should migrate");
        assert_eq!(report.tables.len(), STR_TABLES.len() + BYTES_TABLES.len());
        assert_eq!(
            report.total_rows(),
            STR_TABLES.len() * 2 + BYTES_TABLES.len()
        );

        let new = redb::Database::open(&db).expect("migrated file must open with redb 4");
        let r = new.begin_read().unwrap();
        for name in STR_TABLES {
            let def: redb::TableDefinition<&str, &str> = redb::TableDefinition::new(name);
            let t = r.open_table(def).unwrap();
            assert_eq!(
                t.get(format!("{name}-key-1").as_str())
                    .unwrap()
                    .unwrap()
                    .value(),
                format!("{name}-value-1"),
                "{name}"
            );
            // Values are JSON blobs in production; make sure nothing re-encodes them.
            assert_eq!(
                t.get(format!("{name}-key-2").as_str())
                    .unwrap()
                    .unwrap()
                    .value(),
                r#"{"nested":"json &<>"}"#,
                "{name}"
            );
        }
        // Non-UTF-8 bytes must come through untouched.
        let def: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("device_schemas");
        let t = r.open_table(def).unwrap();
        assert_eq!(
            t.get("schema-1").unwrap().unwrap().value(),
            [0u8, 159, 146, 150]
        );
    }

    #[test]
    fn the_original_is_kept_and_still_readable_by_redb2() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.redb");
        write_v2_database(&db);

        let report = migrate_if_needed(&db).unwrap().unwrap();
        assert!(
            report.backup.exists(),
            "the pre-migration file must be kept"
        );

        // Not merely present — still a working v2 database, so an operator can
        // roll back to the previous core and carry on.
        let old = redb2::Database::open(&report.backup).unwrap();
        let r = old.begin_read().unwrap();
        let def: redb2::TableDefinition<&str, &str> = redb2::TableDefinition::new("devices");
        let t = r.open_table(def).unwrap();
        assert_eq!(
            t.get("devices-key-1").unwrap().unwrap().value(),
            "devices-value-1"
        );
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.redb");
        write_v2_database(&db);

        assert!(migrate_if_needed(&db).unwrap().is_some());
        // Second call: the file is already current, so nothing happens — and
        // in particular the backup is not overwritten with the migrated file.
        assert!(migrate_if_needed(&db).unwrap().is_none());
    }

    #[test]
    fn a_current_format_database_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.redb");
        {
            let d = redb::Database::create(&db).unwrap();
            let w = d.begin_write().unwrap();
            let def: redb::TableDefinition<&str, &str> = redb::TableDefinition::new("devices");
            w.open_table(def).unwrap();
            w.commit().unwrap();
        }
        assert!(migrate_if_needed(&db).unwrap().is_none());
        assert!(!backup_path(&db).exists(), "must not have written a backup");
    }

    #[test]
    fn a_missing_database_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(migrate_if_needed(&dir.path().join("nothing-here.redb"))
            .unwrap()
            .is_none());
    }

    /// A database with only some tables — an older core that never created the
    /// rest. Absent tables are skipped, not fatal.
    #[test]
    fn a_partial_database_migrates_what_it_has() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.redb");
        {
            let d = redb2::Database::create(&db).unwrap();
            let w = d.begin_write().unwrap();
            let def: redb2::TableDefinition<&str, &str> = redb2::TableDefinition::new("devices");
            let mut t = w.open_table(def).unwrap();
            t.insert("only", "table").unwrap();
            drop(t);
            w.commit().unwrap();
        }
        let report = migrate_if_needed(&db).unwrap().unwrap();
        assert_eq!(report.tables, vec![("devices".to_string(), 1)]);
    }

    /// An existing backup means a previous migration already ran and left it.
    /// Overwriting it would destroy the only copy of the original.
    #[test]
    fn an_existing_backup_stops_the_migration() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.redb");
        write_v2_database(&db);
        std::fs::write(backup_path(&db), b"an earlier backup").unwrap();

        let err = migrate_if_needed(&db).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        // And the database is untouched, so the operator can still roll back.
        assert!(redb2::Database::open(&db).is_ok());
    }
}
