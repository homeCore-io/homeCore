//! Rotations: a shared list of commands for one kind of device, played one at
//! a time — WLED presets at sunset, a different playlist in the bathroom each
//! morning — so what plays changes on its own.
//!
//! A rule says *when* (its trigger); a rotation says *what*. It moves on each
//! time it is used, or once an hour / day / week, or on a cron schedule, and
//! within a period every use plays the same choice. Where it is (its state) is
//! kept by core, so a restart doesn't move it on.
//!
//! [`pick`] is the whole of the choosing, with the randomness and the "does
//! the device still offer this" check passed in, so it can be tested alone.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rotation {
    pub id: Uuid,
    pub name: String,
    /// The device it is made for. A rule step may play it on another device
    /// of the same kind instead.
    pub device_id: String,
    pub choices: Vec<RotationChoice>,
    #[serde(default)]
    pub order: RotationOrder,
    #[serde(default)]
    pub advance: RotationAdvance,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RotationChoice {
    /// What to call it; the command's own words when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The device command, as `PATCH /devices/{id}/state` takes it:
    /// `{"action": "play_playlist", "playlist": "Chill"}`,
    /// `{"action": "apply_preset", "preset_id": 7}`.
    #[cfg_attr(feature = "schema", schemars(with = "serde_json::Value"))]
    pub command: serde_json::Value,
    /// When it may play; always when absent. Outside it, the choice is
    /// passed over (not counted missing): calm presets after 10 pm,
    /// day music and night music.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_when: Option<ChoiceWhen>,
}

/// When a choice may play. Every part given must hold.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceWhen {
    /// One of these modes is on (mode device ids, e.g. `mode_night`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modes: Vec<String>,
    /// Between these times of day, local; `22:00`–`06:00` runs past
    /// midnight. Both ends count, as for the `TimeWindow` condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub between: Option<TimeRange>,
    /// On these days.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<chrono::Weekday>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRange {
    pub start: chrono::NaiveTime,
    pub end: chrono::NaiveTime,
}

/// The moment a rotation plays in: what a choice's `only_when` is asked
/// about.
pub struct Moment<'a> {
    pub time: chrono::NaiveTime,
    pub day: chrono::Weekday,
    pub mode_on: &'a dyn Fn(&str) -> bool,
}

impl ChoiceWhen {
    pub fn holds(&self, m: &Moment) -> bool {
        let mode = self.modes.is_empty() || self.modes.iter().any(|id| (m.mode_on)(id));
        let time = self.between.is_none_or(|r| {
            if r.start <= r.end {
                m.time >= r.start && m.time <= r.end
            } else {
                m.time >= r.start || m.time <= r.end
            }
        });
        let day = self.days.is_empty() || self.days.contains(&m.day);
        mode && time && day
    }
}

impl RotationChoice {
    /// Whether it may play at `m`.
    pub fn allowed(&self, m: &Moment) -> bool {
        self.only_when.as_ref().is_none_or(|w| w.holds(m))
    }
}

impl RotationChoice {
    /// Its label, or the first parameter of its command, or the action.
    pub fn name(&self) -> String {
        if let Some(l) = self.label.as_deref().filter(|l| !l.trim().is_empty()) {
            return l.to_string();
        }
        let Some(obj) = self.command.as_object() else {
            return self.command.to_string();
        };
        obj.iter()
            .find(|(k, _)| k.as_str() != "action")
            .map(|(_, v)| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string())
            })
            .or_else(|| {
                obj.get("action")
                    .and_then(|a| a.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| self.command.to_string())
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RotationOrder {
    /// First to last, then round again.
    #[default]
    InTurn,
    /// Each once, in a random order, before any again.
    Shuffle,
    /// Any, but not the same twice running.
    Random,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RotationAdvance {
    /// Every use plays the next.
    #[default]
    EachUse,
    /// The same choice for `n` hours, days or weeks (local time), then the
    /// next. Uses within the period play the same one.
    Every { n: u32, unit: PeriodUnit },
    /// A new choice each time this cron schedule comes round (local time,
    /// the scheduler's syntax); uses in between play the same one.
    Schedule { cron: String },
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeriodUnit {
    Hour,
    Day,
    Week,
}

/// Where a rotation is. Kept by core beside the rotation, and started afresh
/// when its choices change.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotationState {
    /// The choice playing now (an index into `choices`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<usize>,
    /// The period `current` was chosen for; a use in the same one plays it
    /// again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<String>,
    /// In turn: the next to play. Shuffle: how far through `order`.
    #[serde(default)]
    pub position: usize,
    /// Shuffle: this round's order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<usize>,
    /// Choices the device no longer offers (a playlist deleted, a preset
    /// gone), skipped until it does again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played: Option<chrono::DateTime<chrono::Utc>>,
    /// Moved on by hand ("skip this one"): the next play is `current`,
    /// whatever the pace. Without it, a rotation that moves on each use
    /// moved on again at the next play, and the skipped-to choice never
    /// played.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held: bool,
}

/// The period a moment falls in, for [`RotationAdvance::Every`]: the same
/// string for every moment of one period. `None` for the others; a schedule's
/// period is the time its cron last came round, which the caller works out.
pub fn period_of(advance: &RotationAdvance, local: chrono::NaiveDateTime) -> Option<String> {
    let RotationAdvance::Every { n, unit } = advance else {
        return None;
    };
    let n = i64::from((*n).max(1));
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 5)?; // a Monday
    let days = (local.date() - epoch).num_days();
    let (unit, count) = match unit {
        PeriodUnit::Hour => ("h", days * 24 + i64::from(chrono::Timelike::hour(&local))),
        PeriodUnit::Day => ("d", days),
        PeriodUnit::Week => ("w", days.div_euclid(7)),
    };
    Some(format!("{unit}{n}:{}", count.div_euclid(n)))
}

/// The choice to play, updating `state`.
///
/// - Within the period `current` was chosen for, and unless `force` (a
///   "move on"), it is `current` again.
/// - Otherwise the next by `order`, passing over any not `allowed` now
///   (a night-only choice by day), and skipping (and noting in
///   `state.missing`) any the device no longer `offered`.
///
/// `None` when there are no choices, or none is both allowed now and
/// offered.
/// `rand(n)` returns a number below `n`.
pub fn pick(
    rotation: &Rotation,
    state: &mut RotationState,
    period: Option<String>,
    force: bool,
    allowed: &dyn Fn(&RotationChoice) -> bool,
    offered: &dyn Fn(&RotationChoice) -> bool,
    rand: &mut dyn FnMut(usize) -> usize,
) -> Option<usize> {
    let n = rotation.choices.len();
    if n == 0 {
        return None;
    }
    // A state from before the choices changed size says nothing useful.
    if state.current.is_some_and(|c| c >= n) {
        state.current = None;
    }
    if state.order.len() != n || state.order.iter().any(|&i| i >= n) {
        state.order.clear();
    }
    // What the device lacks, as it is now: all of it, every pick, so the
    // list never holds a choice that's back or misses one that's gone.
    state.missing = (0..n).filter(|&i| !offered(&rotation.choices[i])).collect();

    let usable = |i: usize| allowed(&rotation.choices[i]) && offered(&rotation.choices[i]);

    // Held: within the period it was chosen for, or skipped to by hand.
    if !force && (state.held || (period.is_some() && state.period == period)) {
        if let Some(c) = state.current {
            // Still for now: night came, and a day-only choice gives way.
            if usable(c) {
                state.held = false;
                state.period = period;
                return Some(c);
            }
        }
    }

    let chosen = if rotation.order == RotationOrder::Random {
        // From those that can play now, not the same twice running unless
        // it's the only one. Drawing blind and skipping could miss the one
        // that can play, or never pick it when it played last.
        let mut ok = Vec::new();
        for i in (0..n).filter(|&i| usable(i)) {
            ok.push(i);
        }
        let fresh: Vec<usize> = ok
            .iter()
            .copied()
            .filter(|&i| Some(i) != state.current)
            .collect();
        let pool = if fresh.is_empty() { ok } else { fresh };
        (!pool.is_empty()).then(|| pool[rand(pool.len())])
    } else {
        // Each tried once at most: `missing` from an earlier pick says
        // nothing about now, and stopping on its count gave up before
        // trying choices the device lists again.
        let mut tried = std::collections::HashSet::new();
        let mut found = None;
        for _ in 0..(2 * n + 1) {
            let i = next_index(rotation.order, state, n, rand);
            tried.insert(i);
            if usable(i) {
                found = Some(i);
                break;
            }
            if tried.len() >= n {
                break;
            }
        }
        found
    };
    let i = chosen?;
    state.current = Some(i);
    state.period = period;
    state.held = force;
    Some(i)
}

fn next_index(
    order: RotationOrder,
    state: &mut RotationState,
    n: usize,
    rand: &mut dyn FnMut(usize) -> usize,
) -> usize {
    match order {
        RotationOrder::InTurn => {
            let i = state.position % n;
            state.position = (i + 1) % n;
            i
        }
        RotationOrder::Shuffle => {
            if state.order.is_empty() || state.position >= state.order.len() {
                let mut round: Vec<usize> = (0..n).collect();
                for i in (1..n).rev() {
                    round.swap(i, rand(i + 1));
                }
                // A new round doesn't open with what the last one ended on.
                if n > 1 && Some(round[0]) == state.current {
                    round.swap(0, 1 + rand(n - 1));
                }
                state.order = round;
                state.position = 0;
            }
            let i = state.order[state.position];
            state.position += 1;
            i
        }
        RotationOrder::Random => {
            let mut i = rand(n);
            if n > 1 && Some(i) == state.current {
                i = (i + 1 + rand(n - 1)) % n;
            }
            i
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rotation(n: usize, order: RotationOrder, advance: RotationAdvance) -> Rotation {
        Rotation {
            id: Uuid::nil(),
            name: "Bathroom music".into(),
            device_id: "sonos_bathroom".into(),
            choices: (0..n)
                .map(|i| RotationChoice {
                    label: None,
                    command: json!({ "action": "play_playlist", "playlist": format!("p{i}") }),
                    only_when: None,
                })
                .collect(),
            order,
            advance,
        }
    }

    fn all(_: &RotationChoice) -> bool {
        true
    }

    /// A predictable "random": counts up.
    fn counter() -> impl FnMut(usize) -> usize {
        let mut k = 7usize;
        move |n| {
            k = k.wrapping_mul(31).wrapping_add(11);
            k % n
        }
    }

    #[test]
    fn in_turn_goes_round() {
        let r = rotation(3, RotationOrder::InTurn, RotationAdvance::EachUse);
        let mut s = RotationState::default();
        let mut rand = counter();
        let got: Vec<_> = (0..5)
            .map(|_| pick(&r, &mut s, None, false, &all, &all, &mut rand).unwrap())
            .collect();
        assert_eq!(got, [0, 1, 2, 0, 1]);
    }

    #[test]
    fn shuffle_plays_each_once_a_round_and_never_twice_running() {
        let r = rotation(5, RotationOrder::Shuffle, RotationAdvance::EachUse);
        let mut s = RotationState::default();
        let mut rand = counter();
        let mut last = None;
        for _ in 0..4 {
            let mut round: Vec<_> = (0..5)
                .map(|_| {
                    let i = pick(&r, &mut s, None, false, &all, &all, &mut rand).unwrap();
                    assert_ne!(Some(i), last, "the same twice running");
                    last = Some(i);
                    i
                })
                .collect();
            round.sort_unstable();
            assert_eq!(round, [0, 1, 2, 3, 4]);
        }
    }

    #[test]
    fn random_never_repeats_straight_away() {
        let r = rotation(3, RotationOrder::Random, RotationAdvance::EachUse);
        let mut s = RotationState::default();
        let mut rand = counter();
        let mut last = None;
        for _ in 0..50 {
            let i = pick(&r, &mut s, None, false, &all, &all, &mut rand).unwrap();
            assert_ne!(Some(i), last);
            last = Some(i);
        }
    }

    #[test]
    fn within_a_period_the_same_choice_plays() {
        let every_day = RotationAdvance::Every {
            n: 1,
            unit: PeriodUnit::Day,
        };
        let r = rotation(3, RotationOrder::InTurn, every_day.clone());
        let mut s = RotationState::default();
        let mut rand = counter();
        let at = |d: u32, h: u32| {
            chrono::NaiveDate::from_ymd_opt(2026, 11, d)
                .unwrap()
                .and_hms_opt(h, 0, 0)
                .unwrap()
        };
        let mut play = |d, h, s: &mut RotationState| {
            pick(
                &r,
                s,
                period_of(&every_day, at(d, h)),
                false,
                &all,
                &all,
                &mut rand,
            )
            .unwrap()
        };
        assert_eq!(play(27, 7, &mut s), 0);
        assert_eq!(play(27, 13, &mut s), 0);
        assert_eq!(play(27, 23, &mut s), 0);
        assert_eq!(play(28, 6, &mut s), 1);
    }

    #[test]
    fn moving_on_by_hand_holds_for_the_rest_of_the_period() {
        let every_hour = RotationAdvance::Every {
            n: 1,
            unit: PeriodUnit::Hour,
        };
        let r = rotation(3, RotationOrder::InTurn, every_hour);
        let mut s = RotationState::default();
        let mut rand = counter();
        let p = Some("h1:1".to_string());
        assert_eq!(
            pick(&r, &mut s, p.clone(), false, &all, &all, &mut rand),
            Some(0)
        );
        assert_eq!(
            pick(&r, &mut s, p.clone(), true, &all, &all, &mut rand),
            Some(1)
        );
        assert_eq!(pick(&r, &mut s, p, false, &all, &all, &mut rand), Some(1));
    }

    #[test]
    fn a_choice_the_device_no_longer_offers_is_skipped_and_noted() {
        let r = rotation(3, RotationOrder::InTurn, RotationAdvance::EachUse);
        let mut s = RotationState::default();
        let mut rand = counter();
        let not_p1 = |c: &RotationChoice| c.command["playlist"] != "p1";
        assert_eq!(
            pick(&r, &mut s, None, false, &all, &not_p1, &mut rand),
            Some(0)
        );
        assert_eq!(
            pick(&r, &mut s, None, false, &all, &not_p1, &mut rand),
            Some(2)
        );
        assert_eq!(s.missing, [1]);
        // Offered again, it plays, and is no longer noted.
        assert_eq!(
            pick(&r, &mut s, None, false, &all, &all, &mut rand),
            Some(0)
        );
        assert_eq!(
            pick(&r, &mut s, None, false, &all, &all, &mut rand),
            Some(1)
        );
        assert!(s.missing.is_empty());
        // None offered: nothing plays.
        assert_eq!(
            pick(&r, &mut s, None, false, &all, &|_| false, &mut rand),
            None
        );
        assert_eq!(s.missing, [0, 1, 2]);
    }

    /// Missing noted by an earlier pick doesn't stop this one trying them.
    #[test]
    fn old_missing_doesnt_stop_trying_what_is_back() {
        let r = rotation(3, RotationOrder::InTurn, RotationAdvance::EachUse);
        let mut s = RotationState {
            missing: vec![0, 1],
            position: 2,
            ..Default::default()
        };
        let mut rand = counter();
        let not_p2 = |c: &RotationChoice| c.command["playlist"] != "p2";
        assert_eq!(
            pick(&r, &mut s, None, false, &all, &not_p2, &mut rand),
            Some(0)
        );
        assert_eq!(s.missing, [2]);
    }

    /// Random plays the only choice it can, even when it played last.
    #[test]
    fn random_plays_the_only_one_left_even_if_it_played_last() {
        let r = rotation(3, RotationOrder::Random, RotationAdvance::EachUse);
        let mut s = RotationState {
            current: Some(1),
            ..Default::default()
        };
        let mut rand = counter();
        let only_p1 = |c: &RotationChoice| c.command["playlist"] == "p1";
        for _ in 0..20 {
            assert_eq!(
                pick(&r, &mut s, None, false, &all, &only_p1, &mut rand),
                Some(1)
            );
        }
    }

    /// A skip is what the next play plays, even moving on each use.
    #[test]
    fn a_skip_is_what_plays_next() {
        let r = rotation(3, RotationOrder::InTurn, RotationAdvance::EachUse);
        let mut s = RotationState::default();
        let mut rand = counter();
        let mut play = |force: bool, s: &mut RotationState| {
            pick(&r, s, None, force, &all, &all, &mut rand).unwrap()
        };
        assert_eq!(play(false, &mut s), 0);
        assert_eq!(play(true, &mut s), 1);
        assert_eq!(play(false, &mut s), 1);
        assert_eq!(play(false, &mut s), 2);
    }

    #[test]
    fn periods_are_hours_days_and_weeks_of_local_time() {
        let t = |d: u32, h: u32| {
            chrono::NaiveDate::from_ymd_opt(2026, 11, d)
                .unwrap()
                .and_hms_opt(h, 30, 0)
                .unwrap()
        };
        let every = |n, unit| RotationAdvance::Every { n, unit };
        let hour = every(1, PeriodUnit::Hour);
        assert_eq!(period_of(&hour, t(27, 9)), period_of(&hour, t(27, 9)));
        assert_ne!(period_of(&hour, t(27, 9)), period_of(&hour, t(27, 10)));
        let two_hours = every(2, PeriodUnit::Hour);
        assert_ne!(
            period_of(&two_hours, t(27, 9)),
            period_of(&two_hours, t(27, 10))
        );
        assert_eq!(
            period_of(&two_hours, t(27, 10)),
            period_of(&two_hours, t(27, 11))
        );
        let week = every(1, PeriodUnit::Week);
        // 2026-11-23 is a Monday, the 29th a Sunday.
        assert_eq!(period_of(&week, t(23, 0)), period_of(&week, t(29, 23)));
        assert_ne!(period_of(&week, t(29, 23)), period_of(&week, t(30, 0)));
        assert_eq!(period_of(&RotationAdvance::EachUse, t(27, 9)), None);
    }

    fn moment(
        h: u32,
        day: chrono::Weekday,
        night: bool,
    ) -> (chrono::NaiveTime, chrono::Weekday, bool) {
        (
            chrono::NaiveTime::from_hms_opt(h, 0, 0).unwrap(),
            day,
            night,
        )
    }

    /// A choice only for some times, days or modes: night-only at night,
    /// both ends of a window past midnight, and weekends.
    #[test]
    fn a_choice_holds_only_when_its_parts_do() {
        use chrono::Weekday::{Mon, Sat};
        let w =
            |modes: &[&str], between: Option<(u32, u32)>, days: Vec<chrono::Weekday>| ChoiceWhen {
                modes: modes.iter().map(|m| m.to_string()).collect(),
                between: between.map(|(a, b)| TimeRange {
                    start: chrono::NaiveTime::from_hms_opt(a, 0, 0).unwrap(),
                    end: chrono::NaiveTime::from_hms_opt(b, 0, 0).unwrap(),
                }),
                days,
            };
        let holds =
            |when: &ChoiceWhen, (time, day, night): (chrono::NaiveTime, chrono::Weekday, bool)| {
                let mode_on = move |id: &str| id == "mode_night" && night;
                when.holds(&Moment {
                    time,
                    day,
                    mode_on: &mode_on,
                })
            };
        let at_night = w(&["mode_night"], None, vec![]);
        assert!(holds(&at_night, moment(23, Mon, true)));
        assert!(!holds(&at_night, moment(23, Mon, false)));
        let late = w(&[], Some((22, 6)), vec![]);
        assert!(holds(&late, moment(23, Mon, false)));
        assert!(holds(&late, moment(6, Mon, false)));
        assert!(!holds(&late, moment(12, Mon, false)));
        let weekends_late = w(&[], Some((22, 6)), vec![Sat, chrono::Weekday::Sun]);
        assert!(holds(&weekends_late, moment(23, Sat, false)));
        assert!(!holds(&weekends_late, moment(23, Mon, false)));
        assert!(holds(&ChoiceWhen::default(), moment(12, Mon, false)));
    }

    /// Outside its time a choice is passed over, not counted missing; and
    /// within a period, the choice playing gives way when its time ends.
    #[test]
    fn a_choice_not_for_now_is_passed_over_and_gives_way() {
        let every_day = RotationAdvance::Every {
            n: 1,
            unit: PeriodUnit::Day,
        };
        let r = rotation(3, RotationOrder::InTurn, every_day);
        let mut s = RotationState::default();
        let mut rand = counter();
        let day = Some("d1:1".to_string());
        let not_p0 = |c: &RotationChoice| c.command["playlist"] != "p0";
        assert_eq!(
            pick(&r, &mut s, day.clone(), false, &not_p0, &all, &mut rand),
            Some(1)
        );
        assert!(s.missing.is_empty());
        // Same day, but p1's time is over: the next for now plays instead.
        let not_p1 = |c: &RotationChoice| c.command["playlist"] != "p1";
        assert_eq!(
            pick(&r, &mut s, day.clone(), false, &not_p1, &all, &mut rand),
            Some(2)
        );
        // Nothing for now: nothing plays, and nothing is counted missing.
        assert_eq!(
            pick(&r, &mut s, day, false, &|_| false, &all, &mut rand),
            None
        );
        assert!(s.missing.is_empty());
    }

    #[test]
    fn only_when_reads_as_the_api_writes_it() {
        let c: RotationChoice = serde_json::from_value(json!({
            "command": { "action": "play_playlist", "playlist": "Sleep" },
            "only_when": {
                "modes": ["mode_night"],
                "between": { "start": "22:00:00", "end": "06:00:00" },
                "days": ["Sat", "Sun"]
            }
        }))
        .unwrap();
        let w = c.only_when.unwrap();
        assert_eq!(w.modes, ["mode_night"]);
        assert_eq!(w.days.len(), 2);
    }

    #[test]
    fn a_choice_is_named_by_its_label_or_its_command() {
        let c = |label: Option<&str>, command| RotationChoice {
            label: label.map(str::to_string),
            command,
            only_when: None,
        };
        assert_eq!(c(Some("Jazz"), json!({})).name(), "Jazz");
        assert_eq!(
            c(
                None,
                json!({ "action": "play_playlist", "playlist": "Chill" })
            )
            .name(),
            "Chill"
        );
        assert_eq!(
            c(None, json!({ "action": "apply_preset", "preset_id": 7 })).name(),
            "7"
        );
        assert_eq!(c(None, json!({ "action": "next" })).name(), "next");
    }

    #[test]
    fn it_reads_and_writes_as_the_api_shows_it() {
        let r: Rotation = serde_json::from_value(json!({
            "id": Uuid::nil(),
            "name": "Sunset lights",
            "device_id": "wled_porch",
            "choices": [{ "command": { "action": "apply_preset", "preset_id": 3 } }],
            "order": "shuffle",
            "advance": { "every": { "n": 1, "unit": "day" } }
        }))
        .unwrap();
        assert_eq!(r.order, RotationOrder::Shuffle);
        let schedule: RotationAdvance =
            serde_json::from_value(json!({ "schedule": { "cron": "0 0 */2 * * *" } })).unwrap();
        assert!(matches!(schedule, RotationAdvance::Schedule { .. }));
        assert_eq!(
            serde_json::to_value(RotationAdvance::EachUse).unwrap(),
            json!("each_use")
        );
    }
}
