//! Calendar store — loads `.ics` calendar files from a directory, expands
//! recurring events, and provides a hot-reloadable handle used by the scheduler.
//!
//! # Directory layout
//!
//! ```text
//! config/calendars/
//!   us_holidays.ics          ← loaded automatically
//!   us_holidays.meta.json    ← companion sidecar (source URL, refresh config)
//!   personal.ics
//! ```
//!
//! # Recurrence handling
//!
//! `FREQ=YEARLY` rules (the most common pattern in holiday calendars) are
//! expanded over the configured window (default 400 days).  All other RRULE
//! frequencies use only the base occurrence if it falls in the window — a
//! warning is emitted so operators know which calendars need attention.
//!
//! # URL fetch
//!
//! `fetch_and_save` downloads an `.ics` from a URL, saves it to the calendar
//! directory alongside a `.meta.json` sidecar (source URL, fetch timestamp,
//! optional auto-refresh interval), then parses and returns the entry.
//! On next startup (or hot-reload) the file is loaded from disk; no repeated
//! network access is needed.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Utc};
use icalendar::parser as ical_parser;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

// ── Public types ─────────────────────────────────────────────────────────────

/// Shared, hot-reloadable collection of loaded calendars.
pub type CalendarHandle = Arc<RwLock<Vec<CalendarEntry>>>;

/// A single calendar event occurrence (RRULE already expanded).
#[derive(Debug, Clone, Serialize)]
pub struct CalEvent {
    pub uid: String,
    pub summary: String,
    /// Absolute UTC start time of this occurrence.
    pub start: DateTime<Utc>,
    /// Absolute UTC end time of this occurrence.
    /// For all-day events without DTEND, defaults to start + 24h.
    /// For timed events without DTEND or DURATION, defaults to start (zero-duration).
    pub end: DateTime<Utc>,
    /// `true` when the original DTSTART had no time component (VALUE=DATE).
    pub is_all_day: bool,
}

/// A loaded calendar file with its expanded event list.
#[derive(Debug, Clone, Serialize)]
pub struct CalendarEntry {
    /// Stem of the `.ics` filename (e.g. `"us_holidays"`).
    pub id: String,
    #[serde(skip)]
    pub path: PathBuf,
    /// Where the file was fetched from, if fetched via the API.
    pub source_url: Option<String>,
    pub fetched_at: Option<DateTime<Utc>>,
    /// All events within the expansion window, sorted by start time.
    pub events: Vec<CalEvent>,
    pub loaded_at: DateTime<Utc>,
}

impl CalendarEntry {
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    pub fn upcoming_count(&self) -> usize {
        let now = Utc::now();
        self.events.iter().filter(|e| e.start >= now).count()
    }
}

/// Companion sidecar stored alongside each fetched `.ics` file.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct CalendarMeta {
    pub source_url: Option<String>,
    pub fetched_at: Option<DateTime<Utc>>,
    /// When set, the API will auto-refresh this calendar after this many hours.
    pub refresh_hours: Option<u64>,
}

// ── Directory loading ─────────────────────────────────────────────────────────

/// Scan `dir` for `*.ics` files and return a parsed `CalendarEntry` for each.
///
/// Non-fatal: files that fail to parse are warned and skipped.
pub fn load_dir(dir: &Path, expansion_days: u32) -> Result<Vec<CalendarEntry>> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "Calendar dir not readable; skipping");
            return Ok(vec![]);
        }
    };

    let mut entries = Vec::new();

    for dir_entry in read_dir.flatten() {
        let path = dir_entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("ics") {
            continue;
        }

        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Failed to read calendar file");
                continue;
            }
        };

        // Load companion meta sidecar if present (best-effort).
        let meta_path = path.with_extension("meta.json");
        let meta: CalendarMeta = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        match parse_ics(&id, &path, &content, expansion_days) {
            Ok(mut entry) => {
                entry.source_url = meta.source_url;
                entry.fetched_at = meta.fetched_at;
                info!(
                    id       = %id,
                    events   = entry.events.len(),
                    upcoming = entry.upcoming_count(),
                    "Calendar loaded"
                );
                entries.push(entry);
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Failed to parse calendar file");
            }
        }
    }

    Ok(entries)
}

// ── ICS parsing ───────────────────────────────────────────────────────────────

/// Parse a single ICS string into a `CalendarEntry` with RRULE expanded.
///
/// Uses `icalendar::parser` (the low-level RFC 5545 parser) which handles line
/// folding and returns zero-copy types that borrow from the unfolded string.
/// All interesting data is converted to owned types before this function returns.
fn parse_ics(id: &str, path: &Path, content: &str, expansion_days: u32) -> Result<CalendarEntry> {
    // Unfold RFC 5545 continuation lines first.
    let unfolded = ical_parser::unfold(content);

    let parsed = ical_parser::read_calendar(&unfolded)
        .map_err(|e| anyhow!("ICS parse error in {}: {}", path.display(), e))?;

    let now = Utc::now();
    let window_end = now + chrono::Duration::days(expansion_days as i64);
    let mut events = Vec::new();

    for component in &parsed.components {
        if component.name.as_str() != "VEVENT" {
            continue;
        }

        let uid = component
            .find_prop("UID")
            .map(|p| p.val.as_str().to_owned())
            .unwrap_or_default();

        let summary = component
            .find_prop("SUMMARY")
            .map(|p| p.val.as_str().to_owned())
            .unwrap_or_else(|| "(no title)".to_owned());

        let Some(dtstart_prop) = component.find_prop("DTSTART") else {
            continue;
        };

        let dtstart_val = dtstart_prop.val.as_str();

        // VALUE=DATE parameter means it is an all-day event.
        let is_date_only = dtstart_prop.params.iter().any(|p| {
            p.key.as_str().eq_ignore_ascii_case("VALUE")
                && match &p.val {
                    Some(v) => v.as_str().eq_ignore_ascii_case("DATE"),
                    None => false,
                }
        }) || dtstart_val.len() == 8; // bare YYYYMMDD implies date-only

        let (start_dt, is_all_day) = if is_date_only {
            match NaiveDate::parse_from_str(dtstart_val, "%Y%m%d") {
                Ok(d) => match d.and_hms_opt(0, 0, 0) {
                    Some(ndt) => (ndt.and_utc(), true),
                    None => continue,
                },
                Err(_) => continue,
            }
        } else {
            // Strip trailing 'Z' and parse as naive, then treat as UTC.
            let s = dtstart_val.trim_end_matches('Z');
            match NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%S") {
                Ok(naive) => (naive.and_utc(), false),
                Err(_) => {
                    debug!(summary = %summary, dtstart = dtstart_val, "Could not parse DTSTART; skipping event");
                    continue;
                }
            }
        };

        // ── Compute event duration (DTEND or DURATION) ────────────────────
        let event_duration = if let Some(dtend_prop) = component.find_prop("DTEND") {
            let dtend_val = dtend_prop.val.as_str();
            let end_is_date = dtend_prop.params.iter().any(|p| {
                p.key.as_str().eq_ignore_ascii_case("VALUE")
                    && match &p.val {
                        Some(v) => v.as_str().eq_ignore_ascii_case("DATE"),
                        None => false,
                    }
            }) || dtend_val.len() == 8;
            if end_is_date {
                NaiveDate::parse_from_str(dtend_val, "%Y%m%d")
                    .ok()
                    .and_then(|d| d.and_hms_opt(0, 0, 0))
                    .map(|ndt| ndt.and_utc() - start_dt)
            } else {
                let s = dtend_val.trim_end_matches('Z');
                NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%S")
                    .ok()
                    .map(|ndt| ndt.and_utc() - start_dt)
            }
        } else if let Some(dur_prop) = component.find_prop("DURATION") {
            parse_ics_duration(dur_prop.val.as_str())
        } else {
            None
        };

        // Default: all-day → 24h, timed → zero-duration.
        let duration = event_duration.unwrap_or_else(|| {
            if is_all_day {
                chrono::Duration::hours(24)
            } else {
                chrono::Duration::zero()
            }
        });

        // Collect RRULE value (may appear as RRULE property or not at all).
        let rrule_val: Option<String> = component
            .find_prop("RRULE")
            .map(|p| p.val.as_str().to_owned());

        if let Some(rrule) = rrule_val {
            let occurrences = expand_rrule(start_dt, &rrule, &now, &window_end, &summary);
            for occ in occurrences {
                events.push(CalEvent {
                    uid: uid.clone(),
                    summary: summary.clone(),
                    start: occ,
                    end: occ + duration,
                    is_all_day,
                });
            }
        } else {
            // Non-recurring: include only if within the expansion window.
            if start_dt >= now && start_dt <= window_end {
                events.push(CalEvent {
                    uid,
                    summary,
                    start: start_dt,
                    end: start_dt + duration,
                    is_all_day,
                });
            }
        }
    }

    events.sort_by_key(|e| e.start);

    Ok(CalendarEntry {
        id: id.to_string(),
        path: path.to_path_buf(),
        source_url: None,
        fetched_at: None,
        events,
        loaded_at: now,
    })
}

// ── RRULE expansion ───────────────────────────────────────────────────────────

/// Expand a recurrence rule into concrete `DateTime<Utc>` occurrences that
/// fall within `[now, window_end]`.
///
/// Only `FREQ=YEARLY` is expanded.  All other values produce a single occurrence
/// at `dtstart` if it falls in the window, with a warning logged.
fn expand_rrule(
    dtstart: DateTime<Utc>,
    rrule: &str,
    now: &DateTime<Utc>,
    window_end: &DateTime<Utc>,
    summary: &str,
) -> Vec<DateTime<Utc>> {
    let mut out = Vec::new();

    // Parse RRULE key=value pairs.
    let is_yearly = rrule
        .split(';')
        .any(|p| p.trim().eq_ignore_ascii_case("FREQ=YEARLY"));

    if !is_yearly {
        debug!(
            summary,
            rrule, "Non-YEARLY RRULE not fully expanded; including base occurrence only"
        );
        if dtstart >= *now && dtstart <= *window_end {
            out.push(dtstart);
        }
        return out;
    }

    // Parse optional UNTIL= and COUNT= limits.
    let until: Option<DateTime<Utc>> = rrule
        .split(';')
        .find(|p| p.to_ascii_uppercase().starts_with("UNTIL="))
        .and_then(|p| p.splitn(2, '=').nth(1))
        .and_then(parse_ical_dt);

    let count_limit: Option<u32> = rrule
        .split(';')
        .find(|p| p.to_ascii_uppercase().starts_with("COUNT="))
        .and_then(|p| p.splitn(2, '=').nth(1))
        .and_then(|s| s.parse().ok());

    let effective_end = match until {
        Some(u) => u.min(*window_end),
        None => *window_end,
    };

    let base = dtstart.date_naive();
    let time = dtstart.time();
    let mut year = base.year();
    let mut fired: u32 = 0;
    // Safety cap: never generate more than 20 occurrences or go more than
    // 20 years beyond the base year (handles pathological inputs).
    let year_cap = base.year() + 20;

    loop {
        if year > year_cap {
            break;
        }

        let candidate_date = match base.with_year(year) {
            Some(d) => d,
            None => {
                // Feb 29 in a non-leap year — skip.
                year += 1;
                continue;
            }
        };

        let candidate = NaiveDateTime::new(candidate_date, time).and_utc();

        if candidate > effective_end {
            break;
        }
        if let Some(max) = count_limit {
            if fired >= max {
                break;
            }
        }

        if candidate >= *now {
            out.push(candidate);
        }

        fired += 1;
        year += 1;
    }

    out
}

/// Parse an RFC 5545 DURATION value (e.g. `PT1H30M`, `P1D`, `P1DT2H`).
fn parse_ics_duration(s: &str) -> Option<chrono::Duration> {
    let s = s.trim();
    if !s.starts_with('P') {
        return None;
    }
    let s = &s[1..]; // strip leading 'P'
    let mut days: i64 = 0;
    let mut hours: i64 = 0;
    let mut minutes: i64 = 0;
    let mut seconds: i64 = 0;
    let mut in_time = false;
    let mut num_buf = String::new();
    for ch in s.chars() {
        match ch {
            'T' => in_time = true,
            '0'..='9' => num_buf.push(ch),
            'D' if !in_time => {
                days = num_buf.parse().unwrap_or(0);
                num_buf.clear();
            }
            'W' if !in_time => {
                days = num_buf.parse::<i64>().unwrap_or(0) * 7;
                num_buf.clear();
            }
            'H' if in_time => {
                hours = num_buf.parse().unwrap_or(0);
                num_buf.clear();
            }
            'M' if in_time => {
                minutes = num_buf.parse().unwrap_or(0);
                num_buf.clear();
            }
            'S' if in_time => {
                seconds = num_buf.parse().unwrap_or(0);
                num_buf.clear();
            }
            _ => {}
        }
    }
    Some(
        chrono::Duration::days(days)
            + chrono::Duration::hours(hours)
            + chrono::Duration::minutes(minutes)
            + chrono::Duration::seconds(seconds),
    )
}

/// Parse an iCal date/datetime string to `DateTime<Utc>`.
///
/// Accepted formats: `YYYYMMDD`, `YYYYMMDDTHHmmSS`, `YYYYMMDDTHHmmSSZ`.
fn parse_ical_dt(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim_end_matches('Z');
    if s.len() == 8 {
        NaiveDate::parse_from_str(s, "%Y%m%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|dt| dt.and_utc())
    } else if s.len() == 15 {
        NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%S")
            .ok()
            .map(|dt| dt.and_utc())
    } else {
        None
    }
}

// ── URL fetch ─────────────────────────────────────────────────────────────────

/// Fetch an ICS file from `url`, save it to `{dir}/{name}.ics`, write a
/// companion `{dir}/{name}.meta.json`, then parse and return the entry.
///
/// `name` is derived from the URL path stem when `None`.
pub async fn fetch_and_save(
    url: &str,
    name: Option<&str>,
    dir: &Path,
    expansion_days: u32,
    refresh_hours: Option<u64>,
) -> Result<CalendarEntry> {
    // Derive a safe filename from the URL if not provided.
    let derived_name;
    let cal_name = if let Some(n) = name {
        n
    } else {
        derived_name = url
            .split('/')
            .last()
            .and_then(|s| s.split('?').next())
            .map(|s| s.trim_end_matches(".ics"))
            .unwrap_or("calendar")
            .to_string();
        derived_name.as_str()
    };

    // Sanitise: keep only alphanumerics, hyphens, underscores.
    let cal_name: String = cal_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("HomeCore/1.0 (ical-fetcher)")
        .build()?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| anyhow!("HTTP request failed for {}: {}", url, e))?;

    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {} fetching {}", resp.status(), url));
    }

    let content = resp
        .text()
        .await
        .map_err(|e| anyhow!("Failed to read response body: {}", e))?;

    if !content.contains("BEGIN:VCALENDAR") {
        return Err(anyhow!(
            "URL {} does not appear to return a valid ICS file (missing BEGIN:VCALENDAR)",
            url
        ));
    }

    let ics_path = dir.join(format!("{cal_name}.ics"));
    std::fs::write(&ics_path, &content)
        .map_err(|e| anyhow!("Failed to write {}: {}", ics_path.display(), e))?;

    let meta = CalendarMeta {
        source_url: Some(url.to_string()),
        fetched_at: Some(Utc::now()),
        refresh_hours,
    };
    let meta_path = dir.join(format!("{cal_name}.meta.json"));
    std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)
        .map_err(|e| anyhow!("Failed to write meta sidecar: {}", e))?;

    info!(
        name = %cal_name,
        url,
        path = %ics_path.display(),
        "Calendar fetched and saved"
    );

    let mut entry = parse_ics(&cal_name, &ics_path, &content, expansion_days)?;
    entry.source_url = meta.source_url;
    entry.fetched_at = meta.fetched_at;
    Ok(entry)
}

// ── File watcher ──────────────────────────────────────────────────────────────

/// Start a `notify` watcher on `dir`.  When any `.ics` or `.meta.json` file
/// changes, the whole directory is reloaded after a 200 ms debounce and the
/// handle is atomically replaced.
///
/// The returned `RecommendedWatcher` must be kept alive for the lifetime of
/// the process.
pub fn watch(
    dir: PathBuf,
    handle: CalendarHandle,
    expansion_days: u32,
) -> Result<RecommendedWatcher> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(16);

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let relevant = event.paths.iter().any(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("ics") | Some("json")
                )
            });
            if relevant {
                let _ = tx.blocking_send(());
            }
        }
    })?;

    watcher.watch(&dir, RecursiveMode::NonRecursive)?;

    tokio::spawn(async move {
        let debounce = Duration::from_millis(200);
        while rx.recv().await.is_some() {
            tokio::time::sleep(debounce).await;
            while rx.try_recv().is_ok() {} // drain coalesced events

            let dir2 = dir.clone();
            match tokio::task::spawn_blocking(move || load_dir(&dir2, expansion_days)).await {
                Ok(Ok(entries)) => {
                    let count = entries.len();
                    *handle.write().await = entries;
                    info!(calendars = count, "Calendar directory reloaded");
                }
                Ok(Err(e)) => warn!(error = %e, "Calendar reload failed"),
                Err(e) => warn!(error = %e, "Calendar reload task panicked"),
            }
        }
    });

    Ok(watcher)
}

// ── Auto-refresh ──────────────────────────────────────────────────────────────

/// Spawn a background task that periodically re-fetches calendars whose
/// `refresh_hours` meta field is set and whose last fetch is older than that
/// interval.  Runs an initial check at startup, then every 15 minutes.
pub fn spawn_auto_refresh(dir: PathBuf, handle: CalendarHandle, expansion_days: u32) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15 * 60));
        loop {
            interval.tick().await;
            refresh_stale_calendars(&dir, &handle, expansion_days).await;
        }
    });
}

async fn refresh_stale_calendars(dir: &Path, handle: &CalendarHandle, expansion_days: u32) {
    // Collect entries that need refreshing (read lock released before fetch).
    let to_refresh: Vec<(String, String, Option<u64>)> = {
        let entries = handle.read().await;
        entries
            .iter()
            .filter_map(|e| {
                let url = e.source_url.as_ref()?;
                let meta_path = dir.join(format!("{}.meta.json", e.id));
                let meta: CalendarMeta = std::fs::read_to_string(&meta_path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                let refresh_h = meta.refresh_hours?;
                let fetched = meta.fetched_at?;
                let age_h = (Utc::now() - fetched).num_hours();
                if age_h >= refresh_h as i64 {
                    Some((e.id.clone(), url.clone(), meta.refresh_hours))
                } else {
                    None
                }
            })
            .collect()
    };

    for (id, url, refresh_hours) in to_refresh {
        info!(id, url = %url, "Auto-refreshing calendar");
        match fetch_and_save(&url, Some(&id), dir, expansion_days, refresh_hours).await {
            Ok(new_entry) => {
                let mut entries = handle.write().await;
                if let Some(slot) = entries.iter_mut().find(|e| e.id == id) {
                    *slot = new_entry;
                } else {
                    entries.push(new_entry);
                }
                info!(id, "Calendar auto-refresh complete");
            }
            Err(e) => warn!(id, error = %e, "Calendar auto-refresh failed"),
        }
    }
}
