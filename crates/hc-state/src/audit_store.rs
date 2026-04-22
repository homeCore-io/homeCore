//! SQLite-backed append-only audit log.
//!
//! Lives in `audit.db` (separate from `history.db`) so retention policies
//! and archival lifecycles can differ. SQL gives cheap filtering + pagination
//! queries, which is painful in a KV store like redb.
//!
//! Schema is intentionally flat — every event is self-contained. No joins
//! at query time.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;
use uuid::Uuid;

/// Actor type for an audit entry — matches the Actor enum on the auth side
/// but serialises as a stable string column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditActorType {
    User,
    ApiKey,
    LocalAdmin,
    IpWhitelist,
    System,
    Anonymous,
}

impl AuditActorType {
    fn as_str(&self) -> &'static str {
        match self {
            AuditActorType::User => "user",
            AuditActorType::ApiKey => "api_key",
            AuditActorType::LocalAdmin => "local_admin",
            AuditActorType::IpWhitelist => "ip_whitelist",
            AuditActorType::System => "system",
            AuditActorType::Anonymous => "anonymous",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "user" => AuditActorType::User,
            "api_key" => AuditActorType::ApiKey,
            "local_admin" => AuditActorType::LocalAdmin,
            "ip_whitelist" => AuditActorType::IpWhitelist,
            "system" => AuditActorType::System,
            "anonymous" => AuditActorType::Anonymous,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditResult {
    Success,
    Denied,
    Error,
}

impl AuditResult {
    fn as_str(&self) -> &'static str {
        match self {
            AuditResult::Success => "success",
            AuditResult::Denied => "denied",
            AuditResult::Error => "error",
        }
    }
    fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "success" => AuditResult::Success,
            "denied" => AuditResult::Denied,
            "error" => AuditResult::Error,
            _ => return None,
        })
    }
}

/// One row in the audit log — both input (to record) and output (from query).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    #[serde(default)]
    pub id: Option<i64>, // set by the DB on insert
    pub ts: DateTime<Utc>,
    pub actor_type: AuditActorType,
    #[serde(default)]
    pub actor_id: Option<Uuid>,
    pub actor_label: String,
    pub event_type: String,
    #[serde(default)]
    pub scope_used: Option<String>,
    #[serde(default)]
    pub target_kind: Option<String>,
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    pub result: AuditResult,
    #[serde(default)]
    pub detail: serde_json::Value,
}

impl AuditEntry {
    /// Convenience builder for common case: success event with actor + event type.
    pub fn success(
        actor_type: AuditActorType,
        actor_id: Option<Uuid>,
        actor_label: impl Into<String>,
        event_type: impl Into<String>,
    ) -> Self {
        Self {
            id: None,
            ts: Utc::now(),
            actor_type,
            actor_id,
            actor_label: actor_label.into(),
            event_type: event_type.into(),
            scope_used: None,
            target_kind: None,
            target_id: None,
            correlation_id: None,
            ip: None,
            user_agent: None,
            result: AuditResult::Success,
            detail: serde_json::Value::Null,
        }
    }

    pub fn with_target(mut self, kind: impl Into<String>, id: impl Into<String>) -> Self {
        self.target_kind = Some(kind.into());
        self.target_id = Some(id.into());
        self
    }

    pub fn with_result(mut self, result: AuditResult) -> Self {
        self.result = result;
        self
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = detail;
        self
    }

    pub fn with_ip(mut self, ip: impl Into<String>) -> Self {
        self.ip = Some(ip.into());
        self
    }
}

/// Filters for `AuditStore::query`. All fields are optional; the conjunction
/// of the non-None ones selects rows. Results are ordered by `ts DESC`.
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    pub actor_id: Option<Uuid>,
    pub actor_type: Option<AuditActorType>,
    pub event_type: Option<String>,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    pub result: Option<AuditResult>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: u32,
    pub offset: u32,
}

pub struct AuditStore {
    conn: Mutex<Connection>,
}

impl AuditStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating audit DB dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening audit DB {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS audit_events (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                ts             TEXT NOT NULL,
                actor_type     TEXT NOT NULL,
                actor_id       TEXT,
                actor_label    TEXT NOT NULL,
                event_type     TEXT NOT NULL,
                scope_used     TEXT,
                target_kind    TEXT,
                target_id      TEXT,
                correlation_id TEXT,
                ip             TEXT,
                user_agent     TEXT,
                result         TEXT NOT NULL,
                detail         TEXT NOT NULL DEFAULT '{}'
            );
            CREATE INDEX IF NOT EXISTS idx_audit_ts          ON audit_events(ts);
            CREATE INDEX IF NOT EXISTS idx_audit_actor_id    ON audit_events(actor_id);
            CREATE INDEX IF NOT EXISTS idx_audit_event_type  ON audit_events(event_type);
            CREATE INDEX IF NOT EXISTS idx_audit_target      ON audit_events(target_kind, target_id);
            "#,
        )
        .context("creating audit_events schema")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn record(&self, entry: &AuditEntry) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"INSERT INTO audit_events (
                ts, actor_type, actor_id, actor_label,
                event_type, scope_used, target_kind, target_id,
                correlation_id, ip, user_agent, result, detail
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"#,
            params![
                entry.ts.to_rfc3339(),
                entry.actor_type.as_str(),
                entry.actor_id.map(|u| u.to_string()),
                entry.actor_label,
                entry.event_type,
                entry.scope_used,
                entry.target_kind,
                entry.target_id,
                entry.correlation_id,
                entry.ip,
                entry.user_agent,
                entry.result.as_str(),
                entry.detail.to_string(),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn query(&self, q: &AuditQuery) -> Result<Vec<AuditEntry>> {
        let mut sql = String::from(
            "SELECT id, ts, actor_type, actor_id, actor_label, event_type, scope_used, \
             target_kind, target_id, correlation_id, ip, user_agent, result, detail \
             FROM audit_events WHERE 1=1",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(a) = q.actor_id {
            sql.push_str(&format!(" AND actor_id = ?{}", args.len() + 1));
            args.push(Box::new(a.to_string()));
        }
        if let Some(a) = q.actor_type {
            sql.push_str(&format!(" AND actor_type = ?{}", args.len() + 1));
            args.push(Box::new(a.as_str().to_string()));
        }
        if let Some(e) = &q.event_type {
            sql.push_str(&format!(" AND event_type = ?{}", args.len() + 1));
            args.push(Box::new(e.clone()));
        }
        if let Some(k) = &q.target_kind {
            sql.push_str(&format!(" AND target_kind = ?{}", args.len() + 1));
            args.push(Box::new(k.clone()));
        }
        if let Some(id) = &q.target_id {
            sql.push_str(&format!(" AND target_id = ?{}", args.len() + 1));
            args.push(Box::new(id.clone()));
        }
        if let Some(r) = q.result {
            sql.push_str(&format!(" AND result = ?{}", args.len() + 1));
            args.push(Box::new(r.as_str().to_string()));
        }
        if let Some(f) = q.from {
            sql.push_str(&format!(" AND ts >= ?{}", args.len() + 1));
            args.push(Box::new(f.to_rfc3339()));
        }
        if let Some(t) = q.to {
            sql.push_str(&format!(" AND ts <= ?{}", args.len() + 1));
            args.push(Box::new(t.to_rfc3339()));
        }

        sql.push_str(" ORDER BY ts DESC, id DESC");
        sql.push_str(&format!(
            " LIMIT ?{} OFFSET ?{}",
            args.len() + 1,
            args.len() + 2
        ));
        let limit = q.limit.min(1000).max(1);
        args.push(Box::new(limit));
        args.push(Box::new(q.offset));

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), row_to_entry)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Delete rows older than `cutoff`. Returns the number of rows removed.
    pub fn prune_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM audit_events WHERE ts < ?1",
            params![cutoff.to_rfc3339()],
        )?;
        Ok(n)
    }

    pub fn count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_events", [], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        Ok(n)
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditEntry> {
    let id: i64 = row.get(0)?;
    let ts_str: String = row.get(1)?;
    let actor_type_str: String = row.get(2)?;
    let actor_id_str: Option<String> = row.get(3)?;
    let actor_label: String = row.get(4)?;
    let event_type: String = row.get(5)?;
    let scope_used: Option<String> = row.get(6)?;
    let target_kind: Option<String> = row.get(7)?;
    let target_id: Option<String> = row.get(8)?;
    let correlation_id: Option<String> = row.get(9)?;
    let ip: Option<String> = row.get(10)?;
    let user_agent: Option<String> = row.get(11)?;
    let result_str: String = row.get(12)?;
    let detail_str: String = row.get(13)?;

    let ts = DateTime::parse_from_rfc3339(&ts_str)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e)))?;
    let actor_type = AuditActorType::from_str(&actor_type_str).unwrap_or(AuditActorType::Anonymous);
    let actor_id = actor_id_str.and_then(|s| Uuid::parse_str(&s).ok());
    let result = AuditResult::from_str(&result_str).unwrap_or(AuditResult::Error);
    let detail: serde_json::Value =
        serde_json::from_str(&detail_str).unwrap_or(serde_json::Value::Null);

    Ok(AuditEntry {
        id: Some(id),
        ts,
        actor_type,
        actor_id,
        actor_label,
        event_type,
        scope_used,
        target_kind,
        target_id,
        correlation_id,
        ip,
        user_agent,
        result,
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh() -> (TempDir, AuditStore) {
        let dir = TempDir::new().unwrap();
        let store = AuditStore::open(&dir.path().join("audit.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn record_and_query() {
        let (_d, s) = fresh();
        let e = AuditEntry::success(
            AuditActorType::User,
            Some(Uuid::new_v4()),
            "alice",
            "auth.login",
        );
        s.record(&e).unwrap();
        let q = AuditQuery {
            limit: 10,
            ..Default::default()
        };
        let rows = s.query(&q).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_type, "auth.login");
    }

    #[test]
    fn filter_by_actor_id() {
        let (_d, s) = fresh();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        s.record(&AuditEntry::success(AuditActorType::User, Some(a), "alice", "x"))
            .unwrap();
        s.record(&AuditEntry::success(AuditActorType::User, Some(b), "bob", "y"))
            .unwrap();
        let rows = s
            .query(&AuditQuery {
                actor_id: Some(a),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].actor_label, "alice");
    }

    #[test]
    fn filter_by_event_and_result() {
        let (_d, s) = fresh();
        s.record(&AuditEntry::success(AuditActorType::User, None, "x", "auth.login"))
            .unwrap();
        s.record(
            &AuditEntry::success(AuditActorType::User, None, "x", "auth.failed")
                .with_result(AuditResult::Denied),
        )
        .unwrap();
        let rows = s
            .query(&AuditQuery {
                event_type: Some("auth.failed".into()),
                result: Some(AuditResult::Denied),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn filter_by_target() {
        let (_d, s) = fresh();
        s.record(
            &AuditEntry::success(AuditActorType::User, None, "x", "rule.update")
                .with_target("rule", "abc"),
        )
        .unwrap();
        s.record(
            &AuditEntry::success(AuditActorType::User, None, "x", "rule.update")
                .with_target("rule", "xyz"),
        )
        .unwrap();
        let rows = s
            .query(&AuditQuery {
                target_kind: Some("rule".into()),
                target_id: Some("abc".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn prune_before_removes_old_rows() {
        let (_d, s) = fresh();
        let mut old =
            AuditEntry::success(AuditActorType::User, None, "x", "ancient");
        old.ts = Utc::now() - chrono::Duration::days(10);
        s.record(&old).unwrap();
        s.record(&AuditEntry::success(AuditActorType::User, None, "x", "recent"))
            .unwrap();

        let cutoff = Utc::now() - chrono::Duration::days(5);
        let n = s.prune_before(cutoff).unwrap();
        assert_eq!(n, 1);
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn results_ordered_ts_desc() {
        let (_d, s) = fresh();
        let t0 = Utc::now() - chrono::Duration::minutes(10);
        let t1 = Utc::now();
        let mut e0 =
            AuditEntry::success(AuditActorType::User, None, "x", "first");
        e0.ts = t0;
        let mut e1 = AuditEntry::success(AuditActorType::User, None, "x", "second");
        e1.ts = t1;
        s.record(&e0).unwrap();
        s.record(&e1).unwrap();
        let rows = s
            .query(&AuditQuery {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows[0].event_type, "second");
        assert_eq!(rows[1].event_type, "first");
    }

    #[test]
    fn limit_and_offset() {
        let (_d, s) = fresh();
        for i in 0..5 {
            s.record(&AuditEntry::success(AuditActorType::User, None, "x", format!("ev{i}")))
                .unwrap();
        }
        let page1 = s
            .query(&AuditQuery {
                limit: 2,
                offset: 0,
                ..Default::default()
            })
            .unwrap();
        let page2 = s
            .query(&AuditQuery {
                limit: 2,
                offset: 2,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page2.len(), 2);
        // No overlap between pages.
        assert_ne!(page1[0].id, page2[0].id);
    }
}
