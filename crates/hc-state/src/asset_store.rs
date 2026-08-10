//! Content-addressed storage for the files a dashboard points at.
//!
//! Wallpapers, floor plans, fonts, and the texture JPEGs inside a Sweet Home 3D
//! archive. Everything that stores an *address* today — a card's picture, a
//! page background, a skin's font, the image widget — stores a string and
//! resolves it in the browser. Core has had nowhere to put the file behind that
//! string, so all of them have been URL-only.
//!
//! **Bytes on disk, metadata in redb.** A 4MB photograph in a key-value store
//! is a 4MB read every time anything near it is touched. The blobs live under
//! `<parent-of-state_db_path>/assets/`, which is derived rather than configured
//! — the same trick `jwt_secret_file`, `INITIAL_ADMIN_PASSWORD` and `audit.db`
//! already use, and it keeps this off the 13 call sites of `StateStore::open`.
//! If assets ever need their own volume, that is the day to add the config key.
//!
//! **Content-addressed, so the id is the sha256 of the bytes.** Three things
//! fall out of that and all three matter:
//!
//! 1. **Writes are idempotent.** Re-importing the same archive writes nothing;
//!    re-uploading the same wallpaper does not duplicate it. The `.sh3d` case
//!    needs this — one import can carry the same oak texture in forty rooms.
//! 2. **The id is unguessable**, which is what makes serving reads without a
//!    token defensible. A browser sends no `Authorization` header on `<img>`,
//!    a CSS background or a font fetch, so an authenticated GET here would work
//!    only via the IP whitelist — that is, on the LAN, on port 8080, and 401
//!    everywhere else. A 256-bit address is the thing standing in for the
//!    token, so it must never be derived from anything a caller supplies.
//! 3. **The caller cannot choose it**, so no request can overwrite another
//!    asset's bytes by naming them.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const ASSETS: TableDefinition<&str, &str> = TableDefinition::new("assets");

/// What is known about a stored blob. The bytes are not in here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetRecord {
    /// Lowercase hex sha256 of the bytes. Also the filename and the URL.
    pub id: String,
    /// As declared at upload and echoed back on read. Never sniffed.
    pub content_type: String,
    pub size: u64,
    /// The name it had when it arrived, for the manager UI to show. Purely
    /// descriptive — it never touches the filesystem path.
    pub name: String,
    /// What it arrived with, so a floor plan's textures can be pruned together
    /// instead of one at a time. `None` for a single file someone chose.
    pub group: Option<String>,
    pub created: DateTime<Utc>,
}

pub struct AssetStore {
    db: Arc<Database>,
    dir: PathBuf,
}

impl AssetStore {
    pub fn new(db: Arc<Database>, dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating asset directory {}", dir.display()))?;
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(ASSETS)?;
        }
        write_txn.commit()?;
        Ok(Self { db, dir })
    }

    /// Stores `bytes`, or returns the existing record if they are already here.
    ///
    /// Idempotent by content. A second upload of the same file keeps the first
    /// record — including its name and group — because the address is the
    /// bytes, and rewriting the metadata would mean the same id described
    /// itself differently depending on who uploaded it last.
    pub fn put(
        &self,
        bytes: &[u8],
        content_type: &str,
        name: &str,
        group: Option<&str>,
    ) -> Result<AssetRecord> {
        let id = hex::encode(Sha256::digest(bytes));

        if let Some(existing) = self.get_meta(&id)? {
            // Metadata without bytes means someone deleted the file underneath
            // us. Rewrite it rather than handing back a record that 404s.
            if self.path_for(&id)?.exists() {
                return Ok(existing);
            }
        }

        let path = self.path_for(&id)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating asset shard {}", parent.display()))?;
        }
        // Write beside the target and rename, so a crash mid-write cannot leave
        // a truncated file at an address that claims to be the hash of the
        // whole thing.
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("committing {}", path.display()))?;

        let record = AssetRecord {
            id: id.clone(),
            content_type: content_type.to_string(),
            size: bytes.len() as u64,
            name: name.to_string(),
            group: group.map(str::to_string),
            created: Utc::now(),
        };
        let json = serde_json::to_string(&record)?;
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(ASSETS)?;
            table.insert(id.as_str(), json.as_str())?;
        }
        write_txn.commit()?;
        Ok(record)
    }

    pub fn get_meta(&self, id: &str) -> Result<Option<AssetRecord>> {
        if validate_id(id).is_err() {
            return Ok(None);
        }
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ASSETS)?;
        let Some(value) = table.get(id)? else {
            return Ok(None);
        };
        let record: AssetRecord =
            serde_json::from_str(value.value()).context("decoding asset record")?;
        Ok(Some(record))
    }

    /// The bytes, or `None` if there is no such asset.
    pub fn read(&self, id: &str) -> Result<Option<Vec<u8>>> {
        if validate_id(id).is_err() {
            return Ok(None);
        }
        let path = self.path_for(id)?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Newest first, which is the order a manager UI wants.
    pub fn list(&self) -> Result<Vec<AssetRecord>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(ASSETS)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, value) = entry?;
            if let Ok(record) = serde_json::from_str::<AssetRecord>(value.value()) {
                out.push(record);
            }
        }
        out.sort_by_key(|r| std::cmp::Reverse(r.created));
        Ok(out)
    }

    /// Removes the metadata and the bytes. True if it was there.
    ///
    /// Nothing reference-counts: an asset a page still points at will 404 and
    /// the page will show its empty state, which is the same thing that happens
    /// today when a URL goes stale. Auto-deletion is the dangerous half, and
    /// it is deliberately absent.
    pub fn delete(&self, id: &str) -> Result<bool> {
        validate_id(id)?;
        let write_txn = self.db.begin_write()?;
        let existed = {
            let mut table = write_txn.open_table(ASSETS)?;
            let had = table.remove(id)?.is_some();
            had
        };
        write_txn.commit()?;

        let path = self.path_for(id)?;
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
        Ok(existed)
    }

    /// Removes everything that arrived together. Returns how many went.
    pub fn delete_group(&self, group: &str) -> Result<usize> {
        let ids: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|r| r.group.as_deref() == Some(group))
            .map(|r| r.id)
            .collect();
        let mut n = 0;
        for id in ids {
            if self.delete(&id)? {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Total bytes stored, for the manager UI to show before someone asks why
    /// the disk is full.
    pub fn total_bytes(&self) -> Result<u64> {
        Ok(self.list()?.iter().map(|r| r.size).sum())
    }

    /// `<dir>/ab/abcdef…` — sharded, because a house with a floor plan per room
    /// puts hundreds of files in one directory otherwise.
    ///
    /// Every path this store touches comes through here, and this is the only
    /// place an id becomes a path. That is deliberate: the id arrives from a
    /// URL, so the validation below is what stands between `GET /assets/..%2f..`
    /// and the filesystem.
    fn path_for(&self, id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        Ok(self.dir.join(&id[0..2]).join(id))
    }

    /// Visible for tests and for the backup job.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// An id is 64 lowercase hex characters and nothing else.
///
/// Not a nicety. The id is a path segment taken from the URL, so anything
/// permissive here is a directory traversal.
fn validate_id(id: &str) -> Result<()> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("not an asset id");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (AssetStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::create(dir.path().join("t.redb")).unwrap());
        let store = AssetStore::new(db, dir.path().join("assets")).unwrap();
        (store, dir)
    }

    #[test]
    fn the_id_is_the_hash_of_the_bytes() {
        let (s, _d) = store();
        let a = s.put(b"hello", "text/plain", "a.txt", None).unwrap();
        // sha256("hello"), so the address is checkable from outside.
        assert_eq!(
            a.id,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(s.read(&a.id).unwrap().unwrap(), b"hello");
    }

    #[test]
    fn the_same_bytes_twice_is_one_asset() {
        // The whole reason the .sh3d import can be dumb: hand core every
        // texture and let it work out which are new.
        let (s, _d) = store();
        let a = s
            .put(b"oak", "image/jpeg", "oak.jpg", Some("plan-1"))
            .unwrap();
        let b = s
            .put(b"oak", "image/jpeg", "different-name.jpg", Some("plan-2"))
            .unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(b.name, "oak.jpg", "the first record wins, not the last");
        assert_eq!(b.group.as_deref(), Some("plan-1"));
        assert_eq!(s.list().unwrap().len(), 1);
    }

    #[test]
    fn different_bytes_are_different_assets() {
        let (s, _d) = store();
        let a = s.put(b"oak", "image/jpeg", "a.jpg", None).unwrap();
        let b = s.put(b"tile", "image/jpeg", "b.jpg", None).unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(s.list().unwrap().len(), 2);
    }

    #[test]
    fn an_id_that_is_not_an_id_never_reaches_the_filesystem() {
        let (s, _d) = store();
        for bad in [
            "../../etc/passwd",
            "..%2f..%2fetc%2fpasswd",
            "/etc/passwd",
            "AAAA5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824cafe", // uppercase
            "2cf24dba",                                                         // too short
            "",
        ] {
            assert!(s.read(bad).unwrap().is_none(), "read accepted {bad:?}");
            assert!(s.get_meta(bad).unwrap().is_none(), "meta accepted {bad:?}");
            assert!(s.delete(bad).is_err(), "delete accepted {bad:?}");
            assert!(s.path_for(bad).is_err(), "path_for accepted {bad:?}");
        }
    }

    #[test]
    fn deleting_takes_the_bytes_with_it() {
        let (s, _d) = store();
        let a = s.put(b"gone", "image/png", "g.png", None).unwrap();
        assert!(s.delete(&a.id).unwrap());
        assert!(s.read(&a.id).unwrap().is_none());
        assert!(s.get_meta(&a.id).unwrap().is_none());
        assert!(!s.delete(&a.id).unwrap(), "deleting twice is not an error");
    }

    #[test]
    fn a_group_prunes_together() {
        // One floor plan import, one unrelated wallpaper.
        let (s, _d) = store();
        s.put(b"t1", "image/jpeg", "1.jpg", Some("plan-1")).unwrap();
        s.put(b"t2", "image/jpeg", "2.jpg", Some("plan-1")).unwrap();
        let keep = s.put(b"w", "image/png", "wall.png", None).unwrap();

        assert_eq!(s.delete_group("plan-1").unwrap(), 2);
        let left = s.list().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, keep.id);
    }

    #[test]
    fn metadata_without_bytes_repairs_itself() {
        // Someone cleaned out the directory by hand. The next upload of those
        // bytes should put the file back rather than trust the record.
        let (s, _d) = store();
        let a = s.put(b"repair", "image/png", "r.png", None).unwrap();
        std::fs::remove_file(s.path_for(&a.id).unwrap()).unwrap();
        assert!(s.read(&a.id).unwrap().is_none());

        let again = s.put(b"repair", "image/png", "r.png", None).unwrap();
        assert_eq!(again.id, a.id);
        assert_eq!(s.read(&a.id).unwrap().unwrap(), b"repair");
    }

    #[test]
    fn size_is_reported_and_totalled() {
        let (s, _d) = store();
        s.put(&[0u8; 100], "image/png", "a", None).unwrap();
        s.put(&[1u8; 250], "image/png", "b", None).unwrap();
        assert_eq!(s.total_bytes().unwrap(), 350);
    }
}
