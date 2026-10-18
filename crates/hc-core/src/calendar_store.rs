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
//! Recurring events are expanded over the configured window (default 400
//! days) by the `rrule` crate: every frequency — daily, weekly, every other
//! week, monthly (by date or "second Tuesday"), yearly — with INTERVAL,
//! BYDAY, BYMONTHDAY, BYSETPOS, COUNT and UNTIL, plus RDATE and EXDATE, and
//! single occurrences moved or cancelled (RECURRENCE-ID). Times are read in
//! their own zone (TZID), a floating time or a date in the house's zone, so a
//! weekly 07:00 stays 07:00 across a change of clocks. An occurrence still
//! under way when the calendar is read is kept.
//!
//! # URL fetch
//!
//! `fetch_and_save` downloads an `.ics` from a URL, saves it to the calendar
//! directory alongside a `.meta.json` sidecar (source URL, fetch timestamp,
//! optional auto-refresh interval), then parses and returns the entry.
//! On next startup (or hot-reload) the file is loaded from disk; no repeated
//! network access is needed.

use anyhow::{anyhow, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
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
    /// How often a fetched calendar is fetched again, if it is.
    pub refresh_hours: Option<u64>,
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
                entry.refresh_hours = meta.refresh_hours;
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

/// When an iCalendar date or date-time property happens.
#[derive(Debug, Clone, Copy)]
struct When {
    at: DateTime<Utc>,
    /// `VALUE=DATE` — a day, not a moment.
    date_only: bool,
    /// The zone the value was written in: its `TZID`, UTC for a trailing `Z`,
    /// and the house's own zone for a floating time or a date. Recurrence
    /// is worked out in this zone, so a weekly 07:00 stays 07:00 across a
    /// change to or from summer time.
    tz: chrono_tz::Tz,
}

fn param<'a>(prop: &'a ical_parser::Property<'a>, key: &str) -> Option<&'a str> {
    prop.params
        .iter()
        .find(|p| p.key.as_str().eq_ignore_ascii_case(key))
        .and_then(|p| p.val.as_ref())
        .map(|v| v.as_str().trim_matches('"'))
}

/// Every value of a date or date-time property: one for DTSTART, possibly
/// several, comma-separated, for EXDATE and RDATE.
fn whens_of(prop: &ical_parser::Property, house: chrono_tz::Tz) -> Vec<When> {
    let declared_date = param(prop, "VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE"));
    let zone = param(prop, "TZID").and_then(|z| z.parse::<chrono_tz::Tz>().ok());
    prop.val
        .as_str()
        .split(',')
        .filter_map(|raw| {
            let raw = raw.trim();
            if declared_date || raw.len() == 8 {
                let d = NaiveDate::parse_from_str(raw, "%Y%m%d").ok()?;
                // A day, as the house counts days.
                let at = local_instant(house, d.and_hms_opt(0, 0, 0)?)?;
                return Some(When {
                    at,
                    date_only: true,
                    tz: house,
                });
            }
            if let Some(utc) = raw.strip_suffix('Z') {
                let naive = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
                return Some(When {
                    at: naive.and_utc(),
                    date_only: false,
                    tz: chrono_tz::UTC,
                });
            }
            let naive = NaiveDateTime::parse_from_str(raw, "%Y%m%dT%H%M%S").ok()?;
            // A named zone homeCore doesn't know (Outlook's "Eastern Standard
            // Time", say) is read as the house's: nearer than UTC.
            let tz = zone.unwrap_or(house);
            Some(When {
                at: local_instant(tz, naive)?,
                date_only: false,
                tz,
            })
        })
        .collect()
}

/// A wall-clock time in a zone, as an instant. A time that a clock change
/// skips is taken an hour on; one it repeats is taken the first time.
fn local_instant(tz: chrono_tz::Tz, naive: NaiveDateTime) -> Option<DateTime<Utc>> {
    use chrono::TimeZone;
    tz.from_local_datetime(&naive)
        .earliest()
        .or_else(|| {
            tz.from_local_datetime(&(naive + chrono::Duration::hours(1)))
                .earliest()
        })
        .map(|dt| dt.with_timezone(&Utc))
}

fn prop_when(component: &ical_parser::Component, name: &str, house: chrono_tz::Tz) -> Option<When> {
    component
        .find_prop(name)
        .and_then(|p| whens_of(p, house).into_iter().next())
}

/// Parse a single ICS string into a `CalendarEntry` with RRULE expanded.
///
/// Uses `icalendar::parser` (the low-level RFC 5545 parser) which handles line
/// folding and returns zero-copy types that borrow from the unfolded string.
/// All interesting data is converted to owned types before this function returns.
fn parse_ics(id: &str, path: &Path, content: &str, expansion_days: u32) -> Result<CalendarEntry> {
    parse_ics_at(
        id,
        path,
        content,
        expansion_days,
        Utc::now(),
        hc_time::configured_tz(),
    )
}

fn parse_ics_at(
    id: &str,
    path: &Path,
    content: &str,
    expansion_days: u32,
    now: DateTime<Utc>,
    house: chrono_tz::Tz,
) -> Result<CalendarEntry> {
    // Unfold RFC 5545 continuation lines first.
    let unfolded = ical_parser::unfold(content);

    let parsed = ical_parser::read_calendar(&unfolded)
        .map_err(|e| anyhow!("ICS parse error in {}: {}", path.display(), e))?;

    let window_end = now + chrono::Duration::days(expansion_days as i64);
    let mut events = Vec::new();

    // One occurrence of a series moved or cancelled is its own VEVENT, with
    // the series' UID and a RECURRENCE-ID naming the occurrence it replaces.
    // That occurrence is left out of the series; the replacement (unless it
    // is cancelled) is an event in its own right below.
    let mut replaced: std::collections::HashMap<String, Vec<DateTime<Utc>>> =
        std::collections::HashMap::new();
    for component in &parsed.components {
        if component.name.as_str() != "VEVENT" {
            continue;
        }
        if let (Some(uid), Some(rid)) = (
            component.find_prop("UID"),
            prop_when(component, "RECURRENCE-ID", house),
        ) {
            replaced
                .entry(uid.val.as_str().to_owned())
                .or_default()
                .push(rid.at);
        }
    }

    for component in &parsed.components {
        if component.name.as_str() != "VEVENT" {
            continue;
        }
        if component
            .find_prop("STATUS")
            .is_some_and(|p| p.val.as_str().eq_ignore_ascii_case("CANCELLED"))
        {
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

        let Some(start) = prop_when(component, "DTSTART", house) else {
            debug!(summary = %summary, "No usable DTSTART; skipping event");
            continue;
        };
        let is_all_day = start.date_only;

        // ── How long each occurrence lasts (DTEND or DURATION) ────────────
        let duration = prop_when(component, "DTEND", house)
            .map(|end| end.at - start.at)
            .or_else(|| {
                component
                    .find_prop("DURATION")
                    .and_then(|p| parse_ics_duration(p.val.as_str()))
            })
            .filter(|d| *d >= chrono::Duration::zero())
            // Default: a day for an all-day event, a moment otherwise.
            .unwrap_or_else(|| {
                if is_all_day {
                    chrono::Duration::hours(24)
                } else {
                    chrono::Duration::zero()
                }
            });

        // An occurrence still under way counts: an all-day event is active
        // all day, not just at its first instant.
        let from = now - duration;
        let starts: Vec<DateTime<Utc>> = match component.find_prop("RRULE") {
            Some(rrule) => {
                let mut extra = Vec::new();
                let mut skip = Vec::new();
                for p in &component.properties {
                    let name = p.name.as_str();
                    if name.eq_ignore_ascii_case("RDATE") {
                        extra.extend(whens_of(p, house).into_iter().map(|w| w.at));
                    } else if name.eq_ignore_ascii_case("EXDATE") {
                        skip.extend(whens_of(p, house).into_iter().map(|w| w.at));
                    }
                }
                skip.extend(replaced.get(&uid).into_iter().flatten().copied());
                expand_rrule(
                    start,
                    rrule.val.as_str(),
                    &extra,
                    &skip,
                    from,
                    window_end,
                    &summary,
                )
            }
            None if start.at > from && start.at <= window_end => vec![start.at],
            None => Vec::new(),
        };

        for at in starts {
            events.push(CalEvent {
                uid: uid.clone(),
                summary: summary.clone(),
                start: at,
                end: at + duration,
                is_all_day,
            });
        }
    }

    events.sort_by_key(|e| e.start);

    Ok(CalendarEntry {
        id: id.to_string(),
        path: path.to_path_buf(),
        source_url: None,
        fetched_at: None,
        refresh_hours: None,
        events,
        loaded_at: now,
    })
}

// ── RRULE expansion ───────────────────────────────────────────────────────────

/// Most occurrences one series may produce in the window — a minutely rule
/// over 400 days would otherwise be half a million.
const MAX_OCCURRENCES: u16 = 2000;

/// Every occurrence of a recurrence rule starting in `(from, window_end]`:
/// any frequency (daily, weekly, fortnightly, monthly, yearly…) with its
/// INTERVAL, BYDAY, BYMONTHDAY, BYSETPOS, COUNT and UNTIL (RFC 5545, via the
/// `rrule` crate), plus RDATE, less EXDATE and replaced occurrences. Worked
/// out in the zone the event was written in.
fn expand_rrule(
    start: When,
    rrule: &str,
    extra: &[DateTime<Utc>],
    skip: &[DateTime<Utc>],
    from: DateTime<Utc>,
    window_end: DateTime<Utc>,
    summary: &str,
) -> Vec<DateTime<Utc>> {
    let tz = rrule::Tz::Tz(start.tz);
    let dt_start = start.at.with_timezone(&tz);

    // UNTIL is taken out and set as an instant: a date-only UNTIL runs to the
    // end of that day, and the crate wants it in UTC when the start has a zone.
    let mut until = None;
    let rest: Vec<&str> = rrule
        .split(';')
        .filter(|part| {
            let Some((k, v)) = part.split_once('=') else {
                return true;
            };
            if !k.trim().eq_ignore_ascii_case("UNTIL") {
                return true;
            }
            until = parse_until(v.trim(), start);
            false
        })
        .collect();

    let parsed = rest.join(";").parse::<rrule::RRule<rrule::Unvalidated>>();
    let rule = match parsed {
        Ok(r) => r,
        Err(e) => {
            warn!(summary, rrule, error = %e, "Could not read a recurrence rule; using its first occurrence only");
            return [start.at]
                .into_iter()
                .chain(extra.iter().copied())
                .filter(|t| *t > from && *t <= window_end)
                .collect();
        }
    };
    let rule = match until {
        // An UNTIL before the start leaves no occurrences but the start.
        Some(u) if u < start.at => rule.count(1),
        Some(u) => rule.until(u.with_timezone(&rrule::Tz::UTC)),
        None => rule,
    };
    let mut set = match rule.build(dt_start) {
        Ok(set) => set,
        Err(e) => {
            warn!(summary, rrule, error = %e, "Recurrence rule refused; using its first occurrence only");
            return [start.at]
                .into_iter()
                .filter(|t| *t > from && *t <= window_end)
                .collect();
        }
    };
    for t in extra {
        set = set.rdate(t.with_timezone(&tz));
    }
    for t in skip {
        set = set.exdate(t.with_timezone(&tz));
    }
    let result = set
        .after(from.with_timezone(&tz))
        .before(window_end.with_timezone(&tz))
        .all(MAX_OCCURRENCES);
    if result.limited {
        warn!(
            summary,
            rrule, "Recurrence produced too many occurrences; keeping the first {MAX_OCCURRENCES}"
        );
    }
    result
        .dates
        .into_iter()
        .map(|d| d.with_timezone(&Utc))
        .filter(|t| *t > from && *t <= window_end)
        .collect()
}

/// An RRULE's UNTIL as an instant. A date means the whole of that day.
fn parse_until(v: &str, start: When) -> Option<DateTime<Utc>> {
    if v.len() == 8 {
        let d = NaiveDate::parse_from_str(v, "%Y%m%d").ok()?;
        return local_instant(start.tz, d.and_hms_opt(23, 59, 59)?);
    }
    if let Some(utc) = v.strip_suffix('Z') {
        return NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S")
            .ok()
            .map(|n| n.and_utc());
    }
    NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S")
        .ok()
        .and_then(|n| local_instant(start.tz, n))
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
            .next_back()
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
    // The live entry must say what the sidecar says: a client that reads the
    // list and fetches again with what it read would otherwise send `null`
    // back and stop the refresh it was only asking to run now.
    entry.refresh_hours = meta.refresh_hours;
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
            if !changes_content(&event.kind) {
                return;
            }
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

/// Whether a file event means a calendar may have changed. Reading the files
/// is itself an event (inotify reports opens, reads and closes), so reacting
/// to every event made each reload set off the next, five times a second, for
/// as long as homeCore ran.
fn changes_content(kind: &notify::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode, ModifyKind};
    use notify::EventKind;
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        // Closing a file that was open for writing is the end of a write.
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) | EventKind::Any | EventKind::Other => false,
    }
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

#[cfg(test)]
mod watch_tests {
    use super::changes_content;
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
    };
    use notify::EventKind;

    #[test]
    fn reading_the_calendars_is_not_a_change_to_them() {
        assert!(!changes_content(&EventKind::Access(AccessKind::Open(
            AccessMode::Read
        ))));
        assert!(!changes_content(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(!changes_content(&EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::AccessTime
        ))));
        assert!(changes_content(&EventKind::Create(CreateKind::File)));
        assert!(changes_content(&EventKind::Modify(ModifyKind::Data(
            DataChange::Content
        ))));
        assert!(changes_content(&EventKind::Remove(RemoveKind::File)));
        assert!(changes_content(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
    }
}

#[cfg(test)]
mod recurrence_tests {
    use super::*;
    use chrono::TimeZone;

    const NY: chrono_tz::Tz = chrono_tz::America::New_York;

    /// "Now" is Saturday 17 October 2026, 10:00 in New York.
    fn now() -> DateTime<Utc> {
        NY.with_ymd_and_hms(2026, 10, 17, 10, 0, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn events(vevents: &str) -> Vec<CalEvent> {
        let ics = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{vevents}END:VCALENDAR\r\n");
        parse_ics_at("t", Path::new("t.ics"), &ics, 400, now(), NY)
            .unwrap()
            .events
    }

    /// Starts as New York wall-clock strings, for reading the assertions.
    fn local(evs: &[CalEvent], n: usize) -> Vec<String> {
        evs.iter()
            .take(n)
            .map(|e| {
                e.start
                    .with_timezone(&NY)
                    .format("%a %d %b %H:%M")
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_weekly_bin_day_comes_round_every_week_as_a_day() {
        let e = events(
            "BEGIN:VEVENT\r\nUID:bins\r\nSUMMARY:Bins\r\nDTSTART;VALUE=DATE:20261020\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\n",
        );
        assert_eq!(
            local(&e, 3),
            ["Tue 20 Oct 00:00", "Tue 27 Oct 00:00", "Tue 03 Nov 00:00"]
        );
        assert!(e.iter().all(|x| x.is_all_day));
        assert!(e.len() >= 57, "about a year of weeks, got {}", e.len());
        assert_eq!(e[0].end - e[0].start, chrono::Duration::hours(24));
    }

    #[test]
    fn daily_with_a_count_and_every_other_week_on_a_day() {
        let e = events(
            "BEGIN:VEVENT\r\nUID:d\r\nSUMMARY:Pills\r\nDTSTART;TZID=America/New_York:20261018T080000\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEND:VEVENT\r\n",
        );
        assert_eq!(
            local(&e, 9),
            ["Sun 18 Oct 08:00", "Mon 19 Oct 08:00", "Tue 20 Oct 08:00"]
        );

        let e = events(
            "BEGIN:VEVENT\r\nUID:r\r\nSUMMARY:Recycling\r\nDTSTART;VALUE=DATE:20261013\r\nRRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=TU\r\nEND:VEVENT\r\n",
        );
        assert_eq!(
            local(&e, 3),
            ["Tue 27 Oct 00:00", "Tue 10 Nov 00:00", "Tue 24 Nov 00:00"]
        );
    }

    #[test]
    fn the_second_tuesday_of_each_month() {
        let e = events(
            "BEGIN:VEVENT\r\nUID:m\r\nSUMMARY:Council\r\nDTSTART;TZID=America/New_York:20260908T190000\r\nRRULE:FREQ=MONTHLY;BYDAY=2TU\r\nEND:VEVENT\r\n",
        );
        assert_eq!(
            local(&e, 3),
            ["Tue 10 Nov 19:00", "Tue 08 Dec 19:00", "Tue 12 Jan 19:00"]
        );
    }

    #[test]
    fn seven_oclock_stays_seven_oclock_across_the_change_of_clocks() {
        let e = events(
            "BEGIN:VEVENT\r\nUID:w\r\nSUMMARY:Gym\r\nDTSTART;TZID=America/New_York:20261026T070000\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\n",
        );
        // US clocks go back on 1 November 2026.
        assert_eq!(local(&e, 2), ["Mon 26 Oct 07:00", "Mon 02 Nov 07:00"]);
        assert_eq!(e[0].start.format("%H:%M").to_string(), "11:00");
        assert_eq!(e[1].start.format("%H:%M").to_string(), "12:00");
    }

    #[test]
    fn excluded_moved_and_cancelled_occurrences() {
        let e = events(concat!(
            "BEGIN:VEVENT\r\nUID:s\r\nSUMMARY:Lesson\r\nDTSTART;TZID=America/New_York:20261019T160000\r\n",
            "RRULE:FREQ=WEEKLY;COUNT=4\r\nEXDATE;TZID=America/New_York:20261026T160000\r\nEND:VEVENT\r\n",
            // The 2 November lesson moved to the Wednesday.
            "BEGIN:VEVENT\r\nUID:s\r\nSUMMARY:Lesson (moved)\r\nRECURRENCE-ID;TZID=America/New_York:20261102T160000\r\n",
            "DTSTART;TZID=America/New_York:20261104T170000\r\nEND:VEVENT\r\n",
            // The 9 November lesson cancelled.
            "BEGIN:VEVENT\r\nUID:s\r\nSUMMARY:Lesson\r\nSTATUS:CANCELLED\r\nRECURRENCE-ID;TZID=America/New_York:20261109T160000\r\n",
            "DTSTART;TZID=America/New_York:20261109T160000\r\nEND:VEVENT\r\n",
        ));
        assert_eq!(local(&e, 9), ["Mon 19 Oct 16:00", "Wed 04 Nov 17:00"]);
        assert_eq!(e[1].summary, "Lesson (moved)");
    }

    #[test]
    fn an_event_under_way_is_kept_and_a_finished_one_is_not() {
        // Today's all-day event started at midnight; it's 10:00 now.
        let e = events(concat!(
            "BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20261017\r\nEND:VEVENT\r\n",
            "BEGIN:VEVENT\r\nUID:b\r\nSUMMARY:Breakfast\r\nDTSTART:20261017T110000Z\r\nDTEND:20261017T120000Z\r\nEND:VEVENT\r\n",
        ));
        assert_eq!(e.len(), 1, "{e:?}");
        assert_eq!(e[0].summary, "Holiday");
        assert!(e[0].start <= now() && now() < e[0].end);
    }

    #[test]
    fn a_date_only_until_includes_that_day() {
        let e = events(
            "BEGIN:VEVENT\r\nUID:u\r\nSUMMARY:Term\r\nDTSTART;VALUE=DATE:20261019\r\nRRULE:FREQ=DAILY;UNTIL=20261021\r\nEND:VEVENT\r\n",
        );
        assert_eq!(
            local(&e, 9),
            ["Mon 19 Oct 00:00", "Tue 20 Oct 00:00", "Wed 21 Oct 00:00"]
        );
    }

    #[test]
    fn yearly_holidays_still_come_round() {
        let e = events(
            "BEGIN:VEVENT\r\nUID:x\r\nSUMMARY:Christmas\r\nDTSTART;VALUE=DATE:20001225\r\nRRULE:FREQ=YEARLY\r\nEND:VEVENT\r\n",
        );
        assert_eq!(local(&e, 9), ["Fri 25 Dec 00:00"]);
    }

    #[test]
    fn a_rule_that_cannot_be_read_keeps_its_first_occurrence() {
        let e = events(
            "BEGIN:VEVENT\r\nUID:z\r\nSUMMARY:Odd\r\nDTSTART:20261020T120000Z\r\nRRULE:FREQ=SOMETIMES\r\nEND:VEVENT\r\n",
        );
        assert_eq!(e.len(), 1);
    }
}

#[cfg(test)]
mod meta_tests {
    use super::load_dir;

    #[test]
    fn a_loaded_calendar_says_how_often_it_is_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let ics = "BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VEVENT\nUID:a\nDTSTART:20991019T080000Z\nDTEND:20991019T090000Z\nSUMMARY:x\nEND:VEVENT\nEND:VCALENDAR\n";
        std::fs::write(dir.path().join("school.ics"), ics).unwrap();
        std::fs::write(
            dir.path().join("school.meta.json"),
            r#"{"source_url":"http://x/school.ics","fetched_at":null,"refresh_hours":6}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("bins.ics"), ics).unwrap();

        let all = load_dir(dir.path(), 30).unwrap();
        let school = all.iter().find(|c| c.id == "school").unwrap();
        assert_eq!(school.refresh_hours, Some(6));
        assert_eq!(school.source_url.as_deref(), Some("http://x/school.ics"));
        let bins = all.iter().find(|c| c.id == "bins").unwrap();
        assert_eq!(bins.refresh_hours, None);
    }
}
