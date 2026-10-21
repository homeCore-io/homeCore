//! What a Pico's button events become in homeCore's state.
//!
//! Caséta's LIP says two things about a Pico button: pressed (`~DEVICE,id,N,3`)
//! and released (`…,4`). No hold, no double-tap. From those this publishes:
//!
//! - `button_N`: `"press"`, then `"hold"` if it stays down past the hold
//!   threshold (timed here, as hc-lutron does), then `"release"` — the
//!   attribute automations already read, and why it must always come back to
//!   `"release"`: a press whose release was missed used to leave it at
//!   `"press"` for good, and the next press of that button changed nothing.
//! - `pushed` / `held` / `released`: the button's number, set and cleared at
//!   once, which is what homeCore's `ButtonEvent` trigger reads. Cleared so
//!   the same button pressed twice is two changes, two events.
//! - `last_button`, `last_button_name`: which was used last.
//!
//! Pure — it says what to publish and which hold timers to start or stop; the
//! bridge loop does both.

use std::collections::HashMap;

use serde_json::{json, Value};

/// One thing to do about an event.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// A partial state patch, in order.
    Publish(Value),
    /// Start this button's hold timer.
    StartHold(u32),
    /// Stop it: the button came up first.
    CancelHold(u32),
}

/// Where each button of one Pico is: down, held, or up (absent).
#[derive(Debug, Default)]
pub struct Buttons {
    down: HashMap<u32, Down>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Down {
    Pressed,
    Held,
}

impl Buttons {
    /// Forget every button: after a reconnect, nothing is down.
    pub fn reset(&mut self) {
        self.down.clear();
    }

    /// `~DEVICE,id,N,3`.
    pub fn press(&mut self, n: u32, name: &str) -> Vec<Effect> {
        let attr = format!("button_{n}");
        let mut out = vec![];
        // Still down from before — its release was lost. Put it up first, so
        // this press is a change to `button_N` like any other.
        if self.down.remove(&n).is_some() {
            out.push(Effect::CancelHold(n));
            out.push(Effect::Publish(json!({ &attr: "release" })));
        }
        self.down.insert(n, Down::Pressed);
        out.push(Effect::Publish(json!({
            &attr: "press",
            "pushed": n,
            "last_button": n,
            "last_button_name": name,
        })));
        out.push(Effect::Publish(json!({ "pushed": null })));
        out.push(Effect::StartHold(n));
        out
    }

    /// The hold timer ran out with the button still down.
    pub fn hold(&mut self, n: u32) -> Vec<Effect> {
        if self.down.get(&n) != Some(&Down::Pressed) {
            return vec![];
        }
        self.down.insert(n, Down::Held);
        vec![
            Effect::Publish(json!({ format!("button_{n}"): "hold", "held": n })),
            Effect::Publish(json!({ "held": null })),
        ]
    }

    /// `~DEVICE,id,N,4`.
    pub fn release(&mut self, n: u32) -> Vec<Effect> {
        let was_down = self.down.remove(&n).is_some();
        let mut out = vec![Effect::CancelHold(n)];
        let mut patch = json!({ "released": n });
        if was_down {
            patch[format!("button_{n}")] = Value::from("release");
        }
        out.push(Effect::Publish(patch));
        out.push(Effect::Publish(json!({ "released": null })));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patches(e: &[Effect]) -> Vec<Value> {
        e.iter()
            .filter_map(|e| match e {
                Effect::Publish(v) => Some(v.clone()),
                _ => None,
            })
            .collect()
    }

    /// The same button twice: two `pushed` changes (each cleared) and
    /// `button_2` back to release between them — two events, two firings.
    #[test]
    fn the_same_button_twice_is_two_events() {
        let mut b = Buttons::default();
        for _ in 0..2 {
            let e = b.press(2, "On");
            let p = patches(&e);
            assert_eq!(p[0]["pushed"], 2);
            assert_eq!(p[0]["button_2"], "press");
            assert_eq!(p[0]["last_button_name"], "On");
            assert!(p[1]["pushed"].is_null());
            assert!(e.contains(&Effect::StartHold(2)));
            let r = patches(&b.release(2));
            assert_eq!(r[0]["button_2"], "release");
            assert_eq!(r[0]["released"], 2);
            assert!(r[1]["released"].is_null());
        }
    }

    #[test]
    fn a_long_press_is_held_then_released() {
        let mut b = Buttons::default();
        b.press(5, "Raise");
        let h = patches(&b.hold(5));
        assert_eq!(h[0], json!({"button_5": "hold", "held": 5}));
        assert!(h[1]["held"].is_null());
        assert!(b.hold(5).is_empty(), "held once, not again");
        assert_eq!(patches(&b.release(5))[0]["button_5"], "release");
    }

    /// The hold timer of a button already up does nothing.
    #[test]
    fn a_short_press_is_never_held() {
        let mut b = Buttons::default();
        b.press(3, "Favorite");
        assert!(b.release(3).contains(&Effect::CancelHold(3)));
        assert!(b.hold(3).is_empty());
    }

    /// The release was lost: the next press first puts the button up, so
    /// `button_4` still changes to "press".
    #[test]
    fn a_lost_release_does_not_swallow_the_next_press() {
        let mut b = Buttons::default();
        b.press(4, "Off");
        let p = b.press(4, "Off");
        assert_eq!(p[0], Effect::CancelHold(4));
        assert_eq!(p[1], Effect::Publish(json!({"button_4": "release"})));
        assert_eq!(patches(&p)[1]["button_4"], "press");
    }

    /// A release with no press seen (the plugin started mid-press) still
    /// says released, and leaves `button_N` alone.
    #[test]
    fn a_stray_release_is_said_but_changes_no_button() {
        let mut b = Buttons::default();
        let p = patches(&b.release(6));
        assert_eq!(p[0], json!({"released": 6}));
    }

    #[test]
    fn reset_forgets_buttons_down() {
        let mut b = Buttons::default();
        b.press(2, "On");
        b.reset();
        assert_eq!(patches(&b.press(2, "On")).len(), 2, "no release first");
    }
}
