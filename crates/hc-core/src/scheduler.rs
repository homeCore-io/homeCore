//! Scheduler — fires time-based triggers onto the event bus.
//!
//! Watches all enabled `TimeOfDay` rules and emits a synthetic event at the
//! right moment so the rule engine can evaluate them normally.
//!
//! Solar event support (sunrise/sunset) is computed locally from lat/lon
//! without any cloud dependency.

use crate::EventBus;
use chrono::{Datelike, Local, NaiveTime, Timelike};
use hc_types::event::Event;
use hc_types::rule::{Rule, SunEventType, Trigger};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info};

pub struct Scheduler {
    bus: EventBus,
    latitude: f64,
    longitude: f64,
    /// Shared rule set — reads the live handle each tick so hot-reloaded
    /// time-based rules take effect immediately without a restart.
    rules: Arc<RwLock<Vec<Rule>>>,
}

impl Scheduler {
    pub fn new(
        bus: EventBus,
        latitude: f64,
        longitude: f64,
        rules: Arc<RwLock<Vec<Rule>>>,
    ) -> Self {
        Self { bus, latitude, longitude, rules }
    }

    /// Drive the scheduler loop.  Ticks once per minute and fires any rules
    /// whose `TimeOfDay` or `SunEvent` trigger matches the current time.
    pub async fn run(self) {
        info!(lat = self.latitude, lon = self.longitude, "Scheduler started");
        loop {
            let now = Local::now();
            let current_time = now.time().with_second(0).unwrap_or(now.time());
            let current_day = now.weekday();

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
                                times_match(sun_time, current_time)
                            } else {
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
}

/// Returns true if `trigger_time` matches `current_time` within a 1-minute window.
fn times_match(trigger_time: NaiveTime, current_time: NaiveTime) -> bool {
    let trigger_minute = trigger_time.with_second(0).unwrap_or(trigger_time);
    trigger_minute == current_time
}

/// Compute the local time of a solar event using the sunrise equation.
///
/// Returns `None` if the event doesn't occur on this day (polar regions).
fn solar_event_time(
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
            // Solar noon is just 12:00 adjusted for longitude.
            let noon_min = 720.0 - 4.0 * lon - equation_of_time(day_of_year);
            let total_min = noon_min + offset_minutes as f64;
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

    let total_minutes = (event_minutes + offset_minutes as f64).rem_euclid(1440.0);
    let h = (total_minutes / 60.0) as u32;
    let m = (total_minutes % 60.0) as u32;
    NaiveTime::from_hms_opt(h, m, 0)
}

/// NOAA equation of time in minutes.
fn equation_of_time(day_of_year: f64) -> f64 {
    let b = 2.0 * std::f64::consts::PI * (day_of_year - 81.0) / 364.0;
    9.87 * (2.0 * b).sin() - 7.53 * b.cos() - 1.5 * b.sin()
}
