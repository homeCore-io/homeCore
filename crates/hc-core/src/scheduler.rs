//! Scheduler — fires time-based triggers onto the event bus.
//!
//! Watches all enabled `TimeOfDay` and `SunEvent` rules and emits a synthetic
//! event at the right moment so the rule engine can evaluate them normally.
//!
//! Solar event support (sunrise/sunset) is computed locally from lat/lon
//! without any cloud dependency.
//!
//! # Catch-up on restart
//!
//! On startup the scheduler performs a one-time catch-up pass before entering
//! the main loop.  Any rule whose trigger time falls within the configured
//! `catchup_window_minutes` window (i.e. `(now - window, now]` in local time)
//! is fired immediately.  This handles the common case of a process restart
//! shortly after a solar or time-of-day trigger was due.

use crate::calendar_store::CalendarHandle;
use crate::EventBus;
use chrono::{Datelike, NaiveTime, Offset, Timelike};
use cron::Schedule;
use dashmap::DashMap;
use hc_types::event::Event;
use hc_types::rule::{PeriodicUnit, Rule, SunEventType, Trigger};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

pub struct Scheduler {
    pub_bus: EventBus,
    latitude: f64,
    longitude: f64,
    /// Shared rule set — reads the live handle each tick so hot-reloaded
    /// time-based rules take effect immediately without a restart.
    rules: Arc<RwLock<Vec<Rule>>>,
    /// How many minutes back from now to search for missed triggers on startup.
    /// Set to 0 to disable catch-up entirely.
    catchup_window_minutes: u32,
    /// Tracks when each Periodic rule last fired so we can compare elapsed time.
    pub(crate) last_periodic_fire: Arc<DashMap<Uuid, Instant>>,
    /// Optional calendar store for `CalendarEvent` triggers.
    calendar: Option<CalendarHandle>,
}

impl Scheduler {
    pub fn new(
        pub_bus: EventBus,
        latitude: f64,
        longitude: f64,
        rules: Arc<RwLock<Vec<Rule>>>,
        catchup_window_minutes: u32,
    ) -> Self {
        Self {
            pub_bus,
            latitude,
            longitude,
            rules,
            catchup_window_minutes,
            last_periodic_fire: Arc::new(DashMap::new()),
            calendar: None,
        }
    }

    /// Attach a calendar handle so `CalendarEvent` triggers are evaluated.
    pub fn with_calendar(mut self, handle: CalendarHandle) -> Self {
        self.calendar = Some(handle);
        self
    }

    /// Drive the scheduler loop.  Ticks once per minute and fires any rules
    /// whose `TimeOfDay`, `SunEvent`, or `Cron` trigger matches the current time.
    ///
    /// Stops cleanly when `shutdown` receives `true`.
    pub async fn run(self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        info!(
            lat = self.latitude,
            lon = self.longitude,
            catchup_window_minutes = self.catchup_window_minutes,
            "Scheduler started"
        );

        // ── Startup catch-up ────────────────────────────────────────────────
        if self.catchup_window_minutes > 0 {
            self.fire_catchup().await;
        }

        // ── Main polling loop ───────────────────────────────────────────────
        loop {
            let now = hc_time::now_local();
            let current_time = now.time().with_second(0).unwrap_or(now.time());
            let current_day = now.weekday();

            debug!(current_time = %current_time, "Scheduler tick");

            // Hold the read lock only for the duration of the tick evaluation.
            {
                let rules = self.rules.read().await;
                for rule in rules.iter() {
                    if !rule.enabled {
                        continue;
                    }
                    let fires = match &rule.trigger {
                        Trigger::TimeOfDay { time, days } => {
                            let time_match = times_match(*time, current_time);
                            let day_match = days.is_empty() || days.contains(&current_day);
                            time_match && day_match
                        }
                        Trigger::SunEvent {
                            event,
                            offset_minutes,
                        } => {
                            if let Some(sun_time) = solar_event_time(
                                self.latitude,
                                self.longitude,
                                now.date_naive(),
                                *event,
                                *offset_minutes,
                            ) {
                                debug!(
                                    rule_name    = %rule.name,
                                    event        = ?event,
                                    sun_time     = %sun_time,
                                    current_time = %current_time,
                                    matches      = times_match(sun_time, current_time),
                                    "Scheduler: SunEvent check"
                                );
                                times_match(sun_time, current_time)
                            } else {
                                debug!(rule_name = %rule.name, event = ?event, "Scheduler: SunEvent returned None (polar?)");
                                false
                            }
                        }
                        Trigger::Cron { expression } => {
                            cron_fires_now(expression, &now, &rule.name)
                        }
                        Trigger::Periodic { every_n, unit } => {
                            let period_secs = periodic_to_secs(*every_n, unit);
                            match self.last_periodic_fire.get(&rule.id) {
                                Some(last) => last.elapsed().as_secs() >= period_secs,
                                None => true, // never fired — fire immediately
                            }
                        }
                        _ => false,
                    };

                    if fires {
                        if matches!(&rule.trigger, Trigger::Periodic { .. }) {
                            self.last_periodic_fire.insert(rule.id, Instant::now());
                        }
                        debug!(rule_id = %rule.id, "Scheduler firing time trigger");
                        // Emit a synthetic Custom event that the engine interprets as
                        // a manual fire for this specific rule.
                        let _ = self.pub_bus.publish(Event::Custom {
                            timestamp: chrono::Utc::now(),
                            event_type: "scheduler_tick".into(),
                            payload: serde_json::json!({ "rule_id": rule.id }),
                        });
                    }
                }

                // ── CalendarEvent triggers ────────────────────────────────────
                if let Some(cal_handle) = &self.calendar {
                    let calendars = cal_handle.read().await;
                    let utc_now = chrono::Utc::now();

                    for rule in rules.iter() {
                        if !rule.enabled {
                            continue;
                        }
                        let Trigger::CalendarEvent {
                            calendar_id,
                            title_contains,
                            offset_minutes,
                        } = &rule.trigger
                        else {
                            continue;
                        };

                        // The "target" event start time that would cause this rule
                        // to fire right now after applying offset_minutes.
                        let target = utc_now - chrono::Duration::minutes(*offset_minutes as i64);

                        let fired = calendars
                            .iter()
                            .filter(|cal| {
                                calendar_id
                                    .as_deref()
                                    .map(|id| id == cal.id)
                                    .unwrap_or(true)
                            })
                            .flat_map(|cal| cal.events.iter().map(move |ev| (cal, ev)))
                            .any(|(cal, ev)| {
                                // Event start must fall within the current minute window
                                // after offset adjustment.
                                let start = ev.start;
                                let in_window = start.date_naive() == target.date_naive()
                                    && start.hour() == target.hour()
                                    && start.minute() == target.minute();
                                if !in_window {
                                    return false;
                                }
                                if let Some(filter) = title_contains {
                                    if !ev.summary.to_lowercase().contains(&filter.to_lowercase()) {
                                        return false;
                                    }
                                }
                                debug!(
                                    rule_id   = %rule.id,
                                    cal_id    = %cal.id,
                                    summary   = %ev.summary,
                                    "Scheduler: CalendarEvent matched"
                                );
                                true
                            });

                        if fired {
                            let payload = {
                                // Find the matched event for the payload (first match).
                                let ev_info = calendars
                                    .iter()
                                    .filter(|cal| {
                                        calendar_id
                                            .as_deref()
                                            .map(|id| id == cal.id)
                                            .unwrap_or(true)
                                    })
                                    .flat_map(|cal| {
                                        cal.events.iter().map(move |ev| (cal.id.as_str(), ev))
                                    })
                                    .find(|(_, ev)| {
                                        let start = ev.start;
                                        let in_window = start.date_naive() == target.date_naive()
                                            && start.hour() == target.hour()
                                            && start.minute() == target.minute();
                                        if !in_window {
                                            return false;
                                        }
                                        if let Some(filter) = title_contains {
                                            return ev
                                                .summary
                                                .to_lowercase()
                                                .contains(&filter.to_lowercase());
                                        }
                                        true
                                    });
                                match ev_info {
                                    Some((cal_id, ev)) => serde_json::json!({
                                        "rule_id":       rule.id,
                                        "calendar_id":   cal_id,
                                        "event_summary": ev.summary,
                                        "event_start":   ev.start,
                                    }),
                                    None => serde_json::json!({ "rule_id": rule.id }),
                                }
                            };
                            debug!(rule_id = %rule.id, "Scheduler firing CalendarEvent trigger");
                            let _ = self.pub_bus.publish(Event::Custom {
                                timestamp: chrono::Utc::now(),
                                event_type: "scheduler_tick".into(),
                                payload,
                            });
                        }
                    }
                }
            } // read lock released here

            // Sleep until the start of the next minute, waking early on shutdown.
            let secs_until_next_minute = 60 - now.second() as u64;
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(secs_until_next_minute)) => {}
                changed = shutdown.changed() => {
                    match changed {
                        Ok(()) if *shutdown.borrow() => {
                            info!("Scheduler: shutdown signal received — stopping");
                            return;
                        }
                        Ok(()) => {} // Value changed but still false
                        Err(_) => {
                            // Sender dropped — no more shutdown signals.
                            // Sleep out the rest of the minute normally.
                            tokio::time::sleep(Duration::from_secs(secs_until_next_minute)).await;
                        }
                    }
                }
            }
        }
    }

    /// Catch-up pass: fire any time-based rules whose trigger window was missed
    /// while the process was down.
    ///
    /// Checks `(now - catchup_window_minutes, now]` in local wall-clock time.
    /// Both `SunEvent` and `TimeOfDay` triggers are considered.
    async fn fire_catchup(&self) {
        let now = hc_time::now_local();
        // Strip seconds and nanoseconds from both endpoints so in_catchup_window
        // comparisons are at minute granularity (matches how trigger times are stored).
        let now_time =
            NaiveTime::from_hms_opt(now.hour(), now.minute(), 0).unwrap_or_else(|| now.time());
        let window_start_dt = now - chrono::Duration::minutes(self.catchup_window_minutes as i64);
        let window_start_naive =
            NaiveTime::from_hms_opt(window_start_dt.hour(), window_start_dt.minute(), 0)
                .unwrap_or_else(|| window_start_dt.time());
        let today = now.date_naive();
        let weekday = now.weekday();
        // Keep DateTime versions for cron window check.
        let now_dt = now;
        let window_start_dt_full = window_start_dt;

        let rules = self.rules.read().await;
        let mut fired: u32 = 0;

        for rule in rules.iter() {
            if !rule.enabled {
                continue;
            }

            let fires = match &rule.trigger {
                Trigger::SunEvent {
                    event,
                    offset_minutes,
                } => {
                    match solar_event_time(
                        self.latitude,
                        self.longitude,
                        today,
                        *event,
                        *offset_minutes,
                    ) {
                        Some(sun_time) => in_catchup_window(sun_time, window_start_naive, now_time),
                        None => false,
                    }
                }
                Trigger::TimeOfDay { time, days } => {
                    let day_ok = days.is_empty() || days.contains(&weekday);
                    day_ok && in_catchup_window(*time, window_start_naive, now_time)
                }
                Trigger::Cron { expression } => {
                    cron_fired_in_window(expression, &window_start_dt_full, &now_dt, &rule.name)
                }
                // Periodic triggers always fire on startup — they have no "missed" window
                // to check because their period is relative to the last fire time (which
                // is unknown after a restart).  Seeding last_periodic_fire to now prevents
                // an immediate double-fire once the main loop starts.
                Trigger::Periodic { every_n, unit } => {
                    let period_secs = periodic_to_secs(*every_n, unit);
                    if self.catchup_window_minutes as u64 * 60 >= period_secs {
                        self.last_periodic_fire.insert(rule.id, Instant::now());
                        true
                    } else {
                        // Period longer than catchup window — seed to now so the main
                        // loop doesn't fire immediately; it will fire after one period.
                        self.last_periodic_fire.insert(rule.id, Instant::now());
                        false
                    }
                }
                _ => false,
            };

            if fires {
                fired += 1;
                info!(
                    rule_name              = %rule.name,
                    rule_id               = %rule.id,
                    catchup_window_minutes = self.catchup_window_minutes,
                    "Scheduler catch-up: firing missed time trigger"
                );
                if let Err(e) = self.pub_bus.publish(Event::Custom {
                    timestamp: chrono::Utc::now(),
                    event_type: "scheduler_tick".into(),
                    payload: serde_json::json!({ "rule_id": rule.id }),
                }) {
                    warn!(rule_id = %rule.id, error = %e, "Scheduler catch-up: failed to publish event");
                }
            }
        }

        if fired > 0 {
            info!(
                count = fired,
                window_minutes = self.catchup_window_minutes,
                "Scheduler catch-up complete"
            );
        } else {
            debug!(
                window_minutes = self.catchup_window_minutes,
                "Scheduler catch-up: no missed triggers found"
            );
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns true if `trigger` (seconds zeroed) falls within `(window_start, now]`,
/// handling the case where the window crosses midnight.
fn in_catchup_window(trigger: NaiveTime, window_start: NaiveTime, now: NaiveTime) -> bool {
    // Callers pass window_start and now already stripped to minute precision.
    // Strip trigger to minute precision too for a consistent comparison.
    let t = NaiveTime::from_hms_opt(trigger.hour(), trigger.minute(), 0).unwrap_or(trigger);
    if window_start <= now {
        // Normal case: window is contained within a single calendar day.
        t >= window_start && t <= now
    } else {
        // Window crosses midnight (e.g. window_start = 23:55, now = 00:10).
        t >= window_start || t <= now
    }
}

// ── Cron helpers ──────────────────────────────────────────────────────────────

/// Returns `true` if the cron expression fires within the current minute.
///
/// Parses the expression, then asks the schedule for its next occurrence after
/// one minute ago.  If that next occurrence falls within the current minute
/// (same hour + minute) the rule fires.
/// Did this expression fire in the last minute?
///
/// Delegates to [`cron_fired_in_window`] over `(now - 1min, now]`, which is
/// what the tick interval means — and having one implementation is the point.
///
/// This used to compare `times_match(next.time(), current_time)`: the *time* of
/// the next occurrence against the current time, with the date thrown away. For
/// `0 0 8 * * Mon-Fri` evaluated on a Saturday at 08:00, the next occurrence is
/// Monday 08:00, the hours and minutes match, and the rule fired — on a day it
/// excludes. Every day-of-week and day-of-month restriction was silently
/// inert: `Sat,Sun` fired daily, and `0 0 0 1 * *` fired every midnight rather
/// than on the first. Only the time-of-day part of a Cron trigger worked.
///
/// The catch-up path never had the bug, because it compared datetimes. Two
/// implementations of one question, and only one of them was right.
fn cron_fires_now(
    expression: &str,
    now: &chrono::DateTime<chrono_tz::Tz>,
    rule_name: &str,
) -> bool {
    let prev = *now - chrono::Duration::minutes(1);
    cron_fired_in_window(expression, &prev, now, rule_name)
}

/// Returns `true` if the cron expression fired at least once in `(window_start, now]`.
fn cron_fired_in_window(
    expression: &str,
    window_start: &chrono::DateTime<chrono_tz::Tz>,
    now: &chrono::DateTime<chrono_tz::Tz>,
    rule_name: &str,
) -> bool {
    match Schedule::from_str(expression) {
        Ok(schedule) => schedule
            .after(window_start)
            .next()
            .map(|next| next <= *now)
            .unwrap_or(false),
        Err(e) => {
            warn!(rule_name, expression, error = %e, "Scheduler catch-up: invalid cron expression — skipping");
            false
        }
    }
}

/// Returns true if `trigger_time` and `current_time` share the same hour and minute.
///
/// Intentionally ignores seconds and sub-second precision: the scheduler ticks
/// once per minute, so a minute-level comparison is both correct and sufficient.
/// Using `==` after `with_second(0)` would fail because `hc_time::now_local().time()` carries
/// non-zero nanoseconds that `with_second` preserves, while `solar_event_time` and
/// TOML-parsed times use `from_hms_opt` which produces zero nanoseconds.
fn times_match(trigger_time: NaiveTime, current_time: NaiveTime) -> bool {
    trigger_time.hour() == current_time.hour() && trigger_time.minute() == current_time.minute()
}

/// Compute the local time of a solar event using the sunrise equation.
///
/// Public to `crate` so `ModeManager` can reuse the calculation.
///
/// Returns `None` if the event doesn't occur on this day (polar regions).
pub(crate) fn solar_event_time(
    lat: f64,
    lon: f64,
    date: chrono::NaiveDate,
    event: SunEventType,
    offset_minutes: i32,
) -> Option<NaiveTime> {
    // Day of year.
    let day_of_year = date.ordinal() as f64;

    // Solar declination (radians).
    let decl = 0.006918 - 0.399912 * (2.0 * std::f64::consts::PI * day_of_year / 365.0).cos()
        + 0.070257 * (2.0 * std::f64::consts::PI * day_of_year / 365.0).sin()
        - 0.006758 * (4.0 * std::f64::consts::PI * day_of_year / 365.0).cos()
        + 0.000907 * (4.0 * std::f64::consts::PI * day_of_year / 365.0).sin();

    // Hour angle for the target zenith.
    let zenith_deg: f64 = match event {
        SunEventType::Sunrise | SunEventType::Sunset => 90.833,
        SunEventType::SolarNoon => {
            // Solar noon is just 12:00 adjusted for longitude (UTC result).
            let noon_min = 720.0 - 4.0 * lon - equation_of_time(day_of_year);
            let utc_offset_min =
                hc_time::now_local().offset().fix().local_minus_utc() as f64 / 60.0;
            let total_min = (noon_min + offset_minutes as f64 + utc_offset_min).rem_euclid(1440.0);
            let h = (total_min / 60.0).floor() as u32;
            let m = (total_min % 60.0).abs() as u32;
            return NaiveTime::from_hms_opt(h % 24, m, 0);
        }
        SunEventType::CivilDawn | SunEventType::CivilDusk => 96.0,
    };

    let lat_rad = lat.to_radians();
    let cos_hour_angle =
        (zenith_deg.to_radians().cos() - decl.sin() * lat_rad.sin()) / (decl.cos() * lat_rad.cos());

    // No event this day (e.g. polar summer/winter).
    if !(-1.0..=1.0).contains(&cos_hour_angle) {
        return None;
    }

    let hour_angle_deg = cos_hour_angle.acos().to_degrees();

    let eot = equation_of_time(day_of_year);

    let event_minutes = match event {
        SunEventType::Sunrise | SunEventType::CivilDawn => {
            720.0 - 4.0 * (lon + hour_angle_deg) - eot
        }
        SunEventType::Sunset | SunEventType::CivilDusk => {
            720.0 - 4.0 * (lon - hour_angle_deg) - eot
        }
        SunEventType::SolarNoon => unreachable!(),
    };

    // event_minutes is UTC; convert to local wall-clock time so it can be
    // compared directly against hc_time::now_local().time() in the scheduler.
    let utc_offset_min = hc_time::now_local().offset().fix().local_minus_utc() as f64 / 60.0;
    let total_minutes = (event_minutes + offset_minutes as f64 + utc_offset_min).rem_euclid(1440.0);
    let h = (total_minutes / 60.0) as u32;
    let m = (total_minutes % 60.0) as u32;
    NaiveTime::from_hms_opt(h, m, 0)
}

/// Converts a `Periodic` trigger's `every_n` + `unit` into a period in whole seconds.
fn periodic_to_secs(every_n: u32, unit: &PeriodicUnit) -> u64 {
    let n = every_n.max(1) as u64;
    match unit {
        PeriodicUnit::Minutes => n * 60,
        PeriodicUnit::Hours => n * 3600,
        PeriodicUnit::Days => n * 86400,
        PeriodicUnit::Weeks => n * 604800,
    }
}

/// NOAA equation of time in minutes.
fn equation_of_time(day_of_year: f64) -> f64 {
    let b = 2.0 * std::f64::consts::PI * (day_of_year - 81.0) / 364.0;
    9.87 * (2.0 * b).sin() - 7.53 * b.cos() - 1.5 * b.sin()
}

#[cfg(test)]
mod cron_tests {
    use super::*;
    use chrono::TimeZone;

    /// Cron triggers decide when a user's automations run, and this crate had
    /// no test for that at all. Every expression below is one the docs promise
    /// works (`docs/devNotes.md`, "Expression quick reference"), so these are a
    /// contract with the operator rather than a snapshot of a library.
    ///
    /// Note the six fields: `{sec} {min} {hour} {dom} {month} {dow}`. The
    /// second is required — it is what distinguishes Cron from TimeOfDay — and
    /// a five-field expression is rejected, which the last test pins.
    fn at(y: i32, m: u32, d: u32, hh: u32, mm: u32) -> chrono::DateTime<chrono_tz::Tz> {
        chrono_tz::America::New_York
            .with_ymd_and_hms(y, m, d, hh, mm, 0)
            .unwrap()
    }

    fn fires(expr: &str, when: chrono::DateTime<chrono_tz::Tz>) -> bool {
        cron_fires_now(expr, &when, "test-rule")
    }

    #[test]
    fn daily_at_a_fixed_time() {
        // 2026-08-04 is a Tuesday.
        assert!(fires("0 30 9 * * *", at(2026, 8, 4, 9, 30)));
        assert!(!fires("0 30 9 * * *", at(2026, 8, 4, 9, 31)));
        assert!(!fires("0 30 9 * * *", at(2026, 8, 4, 10, 30)));
    }

    #[test]
    fn step_values_fire_on_each_step() {
        for minute in [0, 15, 30, 45] {
            assert!(
                fires("0 */15 * * * *", at(2026, 8, 4, 13, minute)),
                "{minute}"
            );
        }
        for minute in [1, 14, 16, 44] {
            assert!(
                !fires("0 */15 * * * *", at(2026, 8, 4, 13, minute)),
                "{minute}"
            );
        }
    }

    #[test]
    fn named_day_ranges_respect_the_weekend() {
        // Mon-Fri at 08:00 — Tuesday fires, Saturday does not.
        assert!(fires("0 0 8 * * Mon-Fri", at(2026, 8, 4, 8, 0)));
        assert!(!fires("0 0 8 * * Mon-Fri", at(2026, 8, 8, 8, 0)));
    }

    #[test]
    fn day_lists_fire_only_on_listed_days() {
        // Sat,Sun at 10:00 — Saturday the 8th fires, Tuesday the 4th does not.
        assert!(fires("0 0 10 * * Sat,Sun", at(2026, 8, 8, 10, 0)));
        assert!(!fires("0 0 10 * * Sat,Sun", at(2026, 8, 4, 10, 0)));
    }

    #[test]
    fn hour_lists_fire_at_each_listed_hour() {
        assert!(fires("0 0 8,20 * * *", at(2026, 8, 4, 8, 0)));
        assert!(fires("0 0 8,20 * * *", at(2026, 8, 4, 20, 0)));
        assert!(!fires("0 0 8,20 * * *", at(2026, 8, 4, 14, 0)));
    }

    #[test]
    fn day_of_month_fires_only_on_that_day() {
        assert!(fires("0 0 0 1 * *", at(2026, 9, 1, 0, 0)));
        assert!(!fires("0 0 0 1 * *", at(2026, 9, 2, 0, 0)));
    }

    /// Catch-up after a restart: anything that should have fired inside the
    /// window fires immediately. Getting this wrong either double-fires
    /// automations or silently drops them across a restart.
    #[test]
    fn the_catch_up_window_sees_a_missed_firing() {
        let now = at(2026, 8, 4, 9, 40);
        let window_start = at(2026, 8, 4, 9, 25);
        assert!(cron_fired_in_window(
            "0 30 9 * * *",
            &window_start,
            &now,
            "t"
        ));

        // Nothing due in this window.
        let quiet_start = at(2026, 8, 4, 10, 5);
        let quiet_now = at(2026, 8, 4, 10, 20);
        assert!(!cron_fired_in_window(
            "0 30 9 * * *",
            &quiet_start,
            &quiet_now,
            "t"
        ));
    }

    /// An unparseable expression must be inert, never a panic and never a
    /// fire-every-tick — the rule is disabled and logged instead.
    #[test]
    fn a_broken_expression_never_fires() {
        assert!(!fires("not a cron expression", at(2026, 8, 4, 9, 30)));
        assert!(!fires("", at(2026, 8, 4, 9, 30)));
        // Five fields: valid in crontab(5), rejected here. The docs say the
        // second field is required, and this is what enforces it.
        assert!(!fires("30 9 * * *", at(2026, 8, 4, 9, 30)));
    }
}
