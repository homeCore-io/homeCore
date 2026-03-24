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

use crate::EventBus;
use chrono::{Datelike, Local, NaiveTime, Offset, Timelike};
use hc_types::event::Event;
use hc_types::rule::{Rule, SunEventType, Trigger};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

pub struct Scheduler {
    bus:                    EventBus,
    latitude:               f64,
    longitude:              f64,
    /// Shared rule set — reads the live handle each tick so hot-reloaded
    /// time-based rules take effect immediately without a restart.
    rules:                  Arc<RwLock<Vec<Rule>>>,
    /// How many minutes back from now to search for missed triggers on startup.
    /// Set to 0 to disable catch-up entirely.
    catchup_window_minutes: u32,
}

impl Scheduler {
    pub fn new(
        bus:                    EventBus,
        latitude:               f64,
        longitude:              f64,
        rules:                  Arc<RwLock<Vec<Rule>>>,
        catchup_window_minutes: u32,
    ) -> Self {
        Self { bus, latitude, longitude, rules, catchup_window_minutes }
    }

    /// Drive the scheduler loop.  Ticks once per minute and fires any rules
    /// whose `TimeOfDay` or `SunEvent` trigger matches the current time.
    pub async fn run(self) {
        info!(
            lat                    = self.latitude,
            lon                    = self.longitude,
            catchup_window_minutes = self.catchup_window_minutes,
            "Scheduler started"
        );

        // ── Startup catch-up ────────────────────────────────────────────────
        if self.catchup_window_minutes > 0 {
            self.fire_catchup().await;
        }

        // ── Main polling loop ───────────────────────────────────────────────
        loop {
            let now = Local::now();
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
                        Trigger::SunEvent { event, offset_minutes } => {
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
                        _ => false,
                    };

                    if fires {
                        debug!(rule_id = %rule.id, "Scheduler firing time trigger");
                        // Emit a synthetic Custom event that the engine interprets as
                        // a manual fire for this specific rule.
                        let _ = self.bus.publish(Event::Custom {
                            timestamp: chrono::Utc::now(),
                            event_type: "scheduler_tick".into(),
                            payload: serde_json::json!({ "rule_id": rule.id }),
                        });
                    }
                }
            } // read lock released here

            // Sleep until the start of the next minute.
            let secs_until_next_minute = 60 - now.second() as u64;
            tokio::time::sleep(Duration::from_secs(secs_until_next_minute)).await;
        }
    }

    /// Catch-up pass: fire any time-based rules whose trigger window was missed
    /// while the process was down.
    ///
    /// Checks `(now - catchup_window_minutes, now]` in local wall-clock time.
    /// Both `SunEvent` and `TimeOfDay` triggers are considered.
    async fn fire_catchup(&self) {
        let now = Local::now();
        // Strip seconds and nanoseconds from both endpoints so in_catchup_window
        // comparisons are at minute granularity (matches how trigger times are stored).
        let now_time = NaiveTime::from_hms_opt(now.hour(), now.minute(), 0)
            .unwrap_or_else(|| now.time());
        let window_start_dt = now - chrono::Duration::minutes(self.catchup_window_minutes as i64);
        let window_start_naive = NaiveTime::from_hms_opt(window_start_dt.hour(), window_start_dt.minute(), 0)
            .unwrap_or_else(|| window_start_dt.time());
        let today = now.date_naive();
        let weekday = now.weekday();

        let rules = self.rules.read().await;
        let mut fired: u32 = 0;

        for rule in rules.iter() {
            if !rule.enabled {
                continue;
            }

            let fires = match &rule.trigger {
                Trigger::SunEvent { event, offset_minutes } => {
                    match solar_event_time(self.latitude, self.longitude, today, *event, *offset_minutes) {
                        Some(sun_time) => in_catchup_window(sun_time, window_start_naive, now_time),
                        None => false,
                    }
                }
                Trigger::TimeOfDay { time, days } => {
                    let day_ok = days.is_empty() || days.contains(&weekday);
                    day_ok && in_catchup_window(*time, window_start_naive, now_time)
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
                if let Err(e) = self.bus.publish(Event::Custom {
                    timestamp:  chrono::Utc::now(),
                    event_type: "scheduler_tick".into(),
                    payload:    serde_json::json!({ "rule_id": rule.id }),
                }) {
                    warn!(rule_id = %rule.id, error = %e, "Scheduler catch-up: failed to publish event");
                }
            }
        }

        if fired > 0 {
            info!(
                count                  = fired,
                window_minutes         = self.catchup_window_minutes,
                "Scheduler catch-up complete"
            );
        } else {
            debug!(window_minutes = self.catchup_window_minutes, "Scheduler catch-up: no missed triggers found");
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns true if `trigger` (seconds zeroed) falls within `(window_start, now]`,
/// handling the case where the window crosses midnight.
fn in_catchup_window(trigger: NaiveTime, window_start: NaiveTime, now: NaiveTime) -> bool {
    // Callers pass window_start and now already stripped to minute precision.
    // Strip trigger to minute precision too for a consistent comparison.
    let t = NaiveTime::from_hms_opt(trigger.hour(), trigger.minute(), 0)
        .unwrap_or(trigger);
    if window_start <= now {
        // Normal case: window is contained within a single calendar day.
        t >= window_start && t <= now
    } else {
        // Window crosses midnight (e.g. window_start = 23:55, now = 00:10).
        t >= window_start || t <= now
    }
}

/// Returns true if `trigger_time` and `current_time` share the same hour and minute.
///
/// Intentionally ignores seconds and sub-second precision: the scheduler ticks
/// once per minute, so a minute-level comparison is both correct and sufficient.
/// Using `==` after `with_second(0)` would fail because `Local::now().time()` carries
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
    let decl = 0.006918
        - 0.399912 * (2.0 * std::f64::consts::PI * day_of_year / 365.0).cos()
        + 0.070257 * (2.0 * std::f64::consts::PI * day_of_year / 365.0).sin()
        - 0.006758 * (4.0 * std::f64::consts::PI * day_of_year / 365.0).cos()
        + 0.000907 * (4.0 * std::f64::consts::PI * day_of_year / 365.0).sin();

    // Hour angle for the target zenith.
    let zenith_deg: f64 = match event {
        SunEventType::Sunrise | SunEventType::Sunset => 90.833,
        SunEventType::SolarNoon => {
            // Solar noon is just 12:00 adjusted for longitude (UTC result).
            let noon_min = 720.0 - 4.0 * lon - equation_of_time(day_of_year);
            let utc_offset_min = Local::now().offset().fix().local_minus_utc() as f64 / 60.0;
            let total_min = (noon_min + offset_minutes as f64 + utc_offset_min).rem_euclid(1440.0);
            let h = (total_min / 60.0).floor() as u32;
            let m = (total_min % 60.0).abs() as u32;
            return NaiveTime::from_hms_opt(h % 24, m, 0);
        }
        SunEventType::CivilDawn | SunEventType::CivilDusk => 96.0,
    };

    let lat_rad = lat.to_radians();
    let cos_hour_angle = (zenith_deg.to_radians().cos() - decl.sin() * lat_rad.sin())
        / (decl.cos() * lat_rad.cos());

    // No event this day (e.g. polar summer/winter).
    if cos_hour_angle < -1.0 || cos_hour_angle > 1.0 {
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
    // compared directly against Local::now().time() in the scheduler.
    let utc_offset_min = Local::now().offset().fix().local_minus_utc() as f64 / 60.0;
    let total_minutes = (event_minutes + offset_minutes as f64 + utc_offset_min).rem_euclid(1440.0);
    let h = (total_minutes / 60.0) as u32;
    let m = (total_minutes % 60.0) as u32;
    NaiveTime::from_hms_opt(h, m, 0)
}

/// NOAA equation of time in minutes.
fn equation_of_time(day_of_year: f64) -> f64 {
    let b = 2.0 * std::f64::consts::PI * (day_of_year - 81.0) / 364.0;
    9.87 * (2.0 * b).sin() - 7.53 * b.cos() - 1.5 * b.sin()
}
