//! A camera's siren: `on: true` starts it, `on: false` stops it, and it
//! stops itself after `siren_max_secs` so it can't be left sounding when
//! whoever started it has gone.
//!
//! Reolink's HTTP API (`AudioAlarmPlay`) has two ways to sound it: "manul"
//! with `manual_switch` 1 or 0 (sound until stopped, stop), and "times" (a
//! few plays of about five seconds each, then quiet by itself). `on` uses the
//! first; the `sound` action the second. Neither can be read back over HTTP:
//! what the plugin publishes is what it asked for and was accepted, and where
//! the camera pushes its siren's status (Baichuan, see `push`), that is
//! published as it comes. reolink_aio (MIT) was the reference for the
//! firmware's quirks: a manual stop doesn't stop a "times" play unless a
//! manual play is started first, and some firmware refuses a manual play,
//! where a "times" play is used instead.

use crate::api;
use crate::host::Host;
use anyhow::{bail, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{info, warn};

/// How long one play of a "times" sounding lasts, about.
pub const PLAY: Duration = Duration::from_secs(5);

/// Bounds on `siren_max_secs`, and the `sound` action's plays.
pub const MAX_SECS: (u64, u64) = (5, 600);
pub const TIMES: (u32, u32) = (1, 10);

/// Tries at stopping a siren on its own before saying it may still sound.
const STOP_TRIES: u32 = 3;
const STOP_RETRY: Duration = Duration::from_secs(5);

/// A siren's `on`, to publish: with the command it answers, if it does.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub device: String,
    pub on: bool,
    pub command: Option<Value>,
}

/// What a command asks of a siren.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    On,
    Off,
    Times(u32),
}

/// Read a command: `{"on": …}` or `{"action": "sound", "times": n}`.
pub fn want(payload: &Value, default_times: u32) -> Result<Option<Want>> {
    match payload.get("action").and_then(Value::as_str) {
        Some("sound") => {
            let n = payload["times"]
                .as_u64()
                .map_or(default_times, |n| n.min(u32::MAX as u64) as u32);
            Ok(Some(Want::Times(n.clamp(TIMES.0, TIMES.1))))
        }
        Some(other) => bail!("unknown action: {other}"),
        None => Ok(payload.get("on").and_then(Value::as_bool).map(|on| {
            if on {
                Want::On
            } else {
                Want::Off
            }
        })),
    }
}

/// A siren sounding, as far as the plugin knows.
struct Run {
    /// Which start this is: a timer acts only on its own.
    id: u64,
    host: Arc<Host>,
    channel: u32,
    /// A "times" play's end, when it goes quiet by itself.
    ends: Option<Instant>,
    /// Started here (and so stopped here); else heard from the camera.
    ours: bool,
}

pub struct Sirens {
    runs: Mutex<HashMap<String, Run>>,
    next: AtomicU64,
    tx: mpsc::UnboundedSender<Report>,
    /// The longest a siren sounds before the plugin stops it.
    pub max: Duration,
    /// Plays for `on` where the manual mode is refused.
    pub times: u32,
    /// One play's length.
    pub play: Duration,
    /// Between tries at stopping it on its own.
    pub retry: Duration,
}

impl Sirens {
    pub fn new(max_secs: u64, times: u32) -> (Arc<Self>, mpsc::UnboundedReceiver<Report>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let s = Self {
            runs: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            tx,
            max: Duration::from_secs(max_secs.clamp(MAX_SECS.0, MAX_SECS.1)),
            times: times.clamp(TIMES.0, TIMES.1),
            play: PLAY,
            retry: STOP_RETRY,
        };
        (Arc::new(s), rx)
    }

    /// Whether a siren is sounding, as far as the plugin knows.
    pub fn sounding(&self, device: &str) -> bool {
        self.runs.lock().expect("sirens").contains_key(device)
    }

    fn report(&self, device: &str, on: bool, command: Option<&Value>) {
        let _ = self.tx.send(Report {
            device: device.to_string(),
            on,
            command: command.cloned(),
        });
    }

    /// Carry out a command for a siren.
    pub async fn command(
        self: &Arc<Self>,
        host: &Arc<Host>,
        channel: u32,
        device: &str,
        payload: &Value,
    ) -> Result<()> {
        let Some(w) = want(payload, self.times)? else {
            return Ok(());
        };
        match w {
            Want::On => {
                let ends = match host.api.one(api::siren_manual(channel, true)).await {
                    Ok(_) => None,
                    Err(e) => {
                        warn!(device_id = %device, error = %e, "manual siren refused; sounding it a few times instead");
                        host.api.one(api::siren_times(channel, self.times)).await?;
                        Some(Instant::now() + self.play * self.times)
                    }
                };
                self.started(host, channel, device, ends, payload);
                Ok(())
            }
            Want::Times(n) => {
                host.api.one(api::siren_times(channel, n)).await?;
                self.started(
                    host,
                    channel,
                    device,
                    Some(Instant::now() + self.play * n),
                    payload,
                );
                Ok(())
            }
            Want::Off => self.stop(host, channel, device, Some(payload)).await,
        }
    }

    /// It's sounding: say so, and set its end.
    fn started(
        self: &Arc<Self>,
        host: &Arc<Host>,
        channel: u32,
        device: &str,
        ends: Option<Instant>,
        payload: &Value,
    ) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.runs.lock().expect("sirens").insert(
            device.to_string(),
            Run {
                id,
                host: host.clone(),
                channel,
                ends,
                ours: true,
            },
        );
        self.report(device, true, Some(payload));
        let cap = Instant::now() + self.max;
        let (at, by_itself) = match ends {
            Some(e) if e <= cap => (e, true),
            _ => (cap, false),
        };
        let me = self.clone();
        let device = device.to_string();
        tokio::spawn(async move {
            tokio::time::sleep_until(at).await;
            if by_itself {
                // A "times" play is over: quiet, unless it was started again since.
                if me.take_if(&device, id) {
                    me.report(&device, false, None);
                }
            } else {
                me.auto_stop(&device, id).await;
            }
        });
    }

    /// Remove the run if it's still this one.
    fn take_if(&self, device: &str, id: u64) -> bool {
        let mut runs = self.runs.lock().expect("sirens");
        if runs.get(device).is_some_and(|r| r.id == id) {
            runs.remove(device);
            true
        } else {
            false
        }
    }

    /// Stop it: a "times" play needs a manual one started first, or the
    /// manual stop is ignored.
    pub async fn stop(
        &self,
        host: &Host,
        channel: u32,
        device: &str,
        command: Option<&Value>,
    ) -> Result<()> {
        let playing_times = self
            .runs
            .lock()
            .expect("sirens")
            .get(device)
            .and_then(|r| r.ends)
            .is_some_and(|e| Instant::now() < e);
        if playing_times {
            let _ = host.api.one(api::siren_manual(channel, true)).await;
        }
        host.api.one(api::siren_manual(channel, false)).await?;
        self.runs.lock().expect("sirens").remove(device);
        self.report(device, false, command);
        Ok(())
    }

    /// Sounded `max` and still on: stop it, trying again a few times.
    async fn auto_stop(&self, device: &str, id: u64) {
        for attempt in 1..=STOP_TRIES {
            let (host, channel) = {
                let runs = self.runs.lock().expect("sirens");
                match runs.get(device) {
                    Some(r) if r.id == id => (r.host.clone(), r.channel),
                    // Stopped, or started again (with its own end), meanwhile.
                    _ => return,
                }
            };
            match host.api.one(api::siren_manual(channel, false)).await {
                Ok(_) => {
                    if self.take_if(device, id) {
                        info!(device_id = %device, secs = self.max.as_secs(), "siren stopped after its longest");
                        self.report(device, false, None);
                    }
                    return;
                }
                Err(e) => {
                    warn!(device_id = %device, error = %e, attempt, "siren didn't stop");
                    if attempt < STOP_TRIES {
                        tokio::time::sleep(self.retry).await;
                    }
                }
            }
        }
        warn!(device_id = %device, "siren may still be sounding: it refused to stop");
    }

    /// The camera says whether its siren is sounding (pushed).
    pub fn heard(&self, host: &Arc<Host>, channel: u32, device: &str, on: bool) {
        let changed = {
            let mut runs = self.runs.lock().expect("sirens");
            match (on, runs.contains_key(device)) {
                (true, false) => {
                    let id = self.next.fetch_add(1, Ordering::Relaxed);
                    runs.insert(
                        device.to_string(),
                        Run {
                            id,
                            host: host.clone(),
                            channel,
                            ends: None,
                            ours: false,
                        },
                    );
                    true
                }
                // Quiet: whatever end was set for it no longer applies.
                (false, true) => runs.remove(device).is_some(),
                _ => false,
            }
        };
        if changed {
            self.report(device, on, None);
        }
    }

    /// The plugin is stopping: stop the sirens it started, so none outlives it.
    pub async fn stop_all(&self) {
        let ours: Vec<(String, Arc<Host>, u32)> = self
            .runs
            .lock()
            .expect("sirens")
            .iter()
            .filter(|(_, r)| r.ours)
            .map(|(d, r)| (d.clone(), r.host.clone(), r.channel))
            .collect();
        for (device, host, channel) in ours {
            if let Err(e) = self.stop(&host, channel, &device, None).await {
                warn!(device_id = %device, error = %e, "siren not stopped on the way out");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeReolink;
    use serde_json::json;

    fn host_for(fake: &FakeReolink) -> Arc<Host> {
        let entry = serde_json::from_value(json!({
            "id": "t", "address": fake.addr, "password": "pw", "https": false
        }))
        .unwrap();
        Host::new(entry).unwrap()
    }

    fn quick(max_ms: u64) -> (Arc<Sirens>, mpsc::UnboundedReceiver<Report>) {
        let (s, rx) = Sirens::new(60, 3);
        let mut s = Arc::try_unwrap(s).ok().unwrap();
        s.max = Duration::from_millis(max_ms);
        s.play = Duration::from_millis(20);
        s.retry = Duration::from_millis(10);
        (Arc::new(s), rx)
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<Report>) -> Report {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a report")
            .expect("open")
    }

    #[test]
    fn reads_on_off_and_sound() {
        assert_eq!(want(&json!({ "on": true }), 3).unwrap(), Some(Want::On));
        assert_eq!(want(&json!({ "on": false }), 3).unwrap(), Some(Want::Off));
        assert_eq!(
            want(&json!({ "action": "sound" }), 3).unwrap(),
            Some(Want::Times(3))
        );
        assert_eq!(
            want(&json!({ "action": "sound", "times": 500 }), 3).unwrap(),
            Some(Want::Times(10))
        );
        assert_eq!(want(&json!({ "brightness_pct": 5 }), 3).unwrap(), None);
        assert!(want(&json!({ "action": "dance" }), 3).is_err());
    }

    #[test]
    fn the_longest_is_bounded() {
        assert_eq!(Sirens::new(0, 3).0.max, Duration::from_secs(5));
        assert_eq!(Sirens::new(86_400, 3).0.max, Duration::from_secs(600));
        assert_eq!(Sirens::new(60, 0).0.times, 1);
    }

    #[tokio::test]
    async fn on_starts_it_manually_and_off_stops_it() {
        let fake = FakeReolink::start().await;
        let host = host_for(&fake);
        let (s, mut rx) = quick(60_000);
        let on = json!({ "on": true });
        s.command(&host, 1, "cam_siren", &on).await.unwrap();
        assert_eq!(
            next(&mut rx).await,
            Report {
                device: "cam_siren".into(),
                on: true,
                command: Some(on)
            }
        );
        assert!(s.sounding("cam_siren"));
        assert!(fake.sounding(1));

        let off = json!({ "on": false });
        s.command(&host, 1, "cam_siren", &off).await.unwrap();
        let r = next(&mut rx).await;
        assert!(!r.on && r.command == Some(off));
        assert!(!s.sounding("cam_siren"));
        assert!(!fake.sounding(1));
        let sent = fake.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["alarm_mode"], "manul");
        assert_eq!(sent[0]["manual_switch"], 1);
        assert_eq!(sent[1]["manual_switch"], 0);
    }

    #[tokio::test]
    async fn off_when_quiet_still_tells_the_camera() {
        // After a restart the plugin can't know: stopping is always sent.
        let fake = FakeReolink::start().await;
        let host = host_for(&fake);
        let (s, mut rx) = quick(60_000);
        s.command(&host, 0, "cam_siren", &json!({ "on": false }))
            .await
            .unwrap();
        assert!(!next(&mut rx).await.on);
        assert_eq!(fake.sent()[0]["manual_switch"], 0);
    }

    #[tokio::test]
    async fn it_stops_itself_after_the_longest() {
        let fake = FakeReolink::start().await;
        let host = host_for(&fake);
        let (s, mut rx) = quick(100);
        s.command(&host, 1, "cam_siren", &json!({ "on": true }))
            .await
            .unwrap();
        assert!(next(&mut rx).await.on);
        let r = next(&mut rx).await;
        assert_eq!((r.on, r.command), (false, None));
        assert!(!fake.sounding(1));
        assert!(!s.sounding("cam_siren"));
    }

    #[tokio::test]
    async fn a_new_start_gets_a_new_end() {
        let fake = FakeReolink::start().await;
        let host = host_for(&fake);
        let (s, mut rx) = quick(300);
        s.command(&host, 1, "cam_siren", &json!({ "on": true }))
            .await
            .unwrap();
        next(&mut rx).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        s.command(&host, 1, "cam_siren", &json!({ "on": true }))
            .await
            .unwrap();
        next(&mut rx).await;
        // The first start's end has passed: still on.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(s.sounding("cam_siren") && fake.sounding(1));
        let r = next(&mut rx).await;
        assert!(!r.on);
        assert!(!fake.sounding(1));
    }

    #[tokio::test]
    async fn a_refused_stop_is_tried_again_and_on_stays_true() {
        let fake = FakeReolink::start().await;
        let host = host_for(&fake);
        let (s, mut rx) = quick(50);
        s.command(&host, 1, "cam_siren", &json!({ "on": true }))
            .await
            .unwrap();
        next(&mut rx).await;
        fake.refuse_all(true);
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Three tries, all refused: it may still sound, so it reads on.
        assert!(s.sounding("cam_siren"));
        assert!(rx.try_recv().is_err());
        // A stop asked for by hand, once the camera takes it again.
        fake.refuse_all(false);
        s.command(&host, 1, "cam_siren", &json!({ "on": false }))
            .await
            .unwrap();
        assert!(!next(&mut rx).await.on);
        assert!(!fake.sounding(1));
    }

    #[tokio::test]
    async fn times_goes_quiet_by_itself_and_its_stop_starts_manual_first() {
        let fake = FakeReolink::start().await;
        let host = host_for(&fake);
        let (s, mut rx) = quick(60_000);
        s.command(
            &host,
            1,
            "cam_siren",
            &json!({ "action": "sound", "times": 2 }),
        )
        .await
        .unwrap();
        assert!(next(&mut rx).await.on);
        assert_eq!(fake.sent()[0]["alarm_mode"], "times");
        assert_eq!(fake.sent()[0]["times"], 2);
        let r = next(&mut rx).await;
        assert_eq!((r.on, r.command), (false, None));
        // No stop was sent: it ended by itself.
        assert_eq!(fake.sent().len(), 1);

        // A long play this time, stopped while it sounds.
        let (s, mut rx) = quick(60_000);
        let mut s = Arc::try_unwrap(s).ok().unwrap();
        s.play = Duration::from_secs(30);
        let s = Arc::new(s);
        s.command(
            &host,
            1,
            "cam_siren",
            &json!({ "action": "sound", "times": 2 }),
        )
        .await
        .unwrap();
        next(&mut rx).await;
        s.command(&host, 1, "cam_siren", &json!({ "on": false }))
            .await
            .unwrap();
        assert!(!next(&mut rx).await.on);
        let sent = fake.sent();
        assert_eq!(sent.len(), 4);
        assert_eq!(
            (
                sent[2]["manual_switch"].clone(),
                sent[3]["manual_switch"].clone()
            ),
            (json!(1), json!(0))
        );
    }

    #[tokio::test]
    async fn a_refused_manual_play_sounds_a_few_times_instead() {
        let fake = FakeReolink::start().await;
        fake.refuse_manual_on(true);
        let host = host_for(&fake);
        let (s, mut rx) = quick(60_000);
        s.command(&host, 1, "cam_siren", &json!({ "on": true }))
            .await
            .unwrap();
        assert!(next(&mut rx).await.on);
        assert_eq!(fake.sent().last().unwrap()["alarm_mode"], "times");
        assert_eq!(fake.sent().last().unwrap()["times"], 3);
        assert!(!next(&mut rx).await.on);
    }

    #[tokio::test]
    async fn a_refused_start_reads_off() {
        let fake = FakeReolink::start().await;
        fake.refuse_all(true);
        let host = host_for(&fake);
        let (s, mut rx) = quick(60_000);
        assert!(s
            .command(&host, 1, "cam_siren", &json!({ "on": true }))
            .await
            .is_err());
        assert!(!s.sounding("cam_siren"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn what_the_camera_pushes_is_published_once() {
        let fake = FakeReolink::start().await;
        let host = host_for(&fake);
        let (s, mut rx) = quick(60_000);
        s.heard(&host, 1, "cam_siren", true);
        s.heard(&host, 1, "cam_siren", true);
        assert!(next(&mut rx).await.on);
        s.heard(&host, 1, "cam_siren", false);
        assert!(!next(&mut rx).await.on);
        assert!(rx.try_recv().is_err());
        // Heard, not started here: never stopped on the way out.
        s.heard(&host, 1, "cam_siren", true);
        next(&mut rx).await;
        s.stop_all().await;
        assert!(fake.sent().is_empty());
    }

    #[tokio::test]
    async fn stopping_the_plugin_stops_the_sirens_it_started() {
        let fake = FakeReolink::start().await;
        let host = host_for(&fake);
        let (s, mut rx) = quick(60_000);
        s.command(&host, 1, "cam_siren", &json!({ "on": true }))
            .await
            .unwrap();
        next(&mut rx).await;
        s.stop_all().await;
        assert!(!next(&mut rx).await.on);
        assert!(!fake.sounding(1));
    }

    /// Against the Node stand-in (`standin/standin.mjs`, an NVR), started as
    /// `PORT=18980 PUSH_PORT=18981 SIREN_PLAY_MS=300 node standin.mjs`:
    ///
    /// `REOLINK_STANDIN=127.0.0.1:18980 REOLINK_STANDIN_PUSH=18981 cargo test -p hc-reolink -- --ignored standin`
    ///
    /// Only ever a stand-in on this machine: anything else is refused.
    #[tokio::test]
    #[ignore = "needs the Node stand-in running"]
    async fn the_standin_sounds_and_stops_and_pushes_it() {
        let addr = std::env::var("REOLINK_STANDIN").expect("REOLINK_STANDIN=127.0.0.1:<port>");
        assert!(
            addr.starts_with("127.0.0.1:"),
            "only a stand-in on 127.0.0.1, never a camera"
        );
        let entry = serde_json::from_value(json!({
            "id": "standin", "address": addr, "password": "standin-pass", "https": false
        }))
        .unwrap();
        let host = Host::new(entry).unwrap();
        let read = |addr: String| async move {
            let v: Value = reqwest::get(format!("http://{addr}/standin/siren"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            v[1].clone()
        };
        let mut push = match std::env::var("REOLINK_STANDIN_PUSH") {
            Ok(port) => Some(
                crate::push::Push::connect(
                    "127.0.0.1",
                    port.parse().unwrap(),
                    "admin",
                    "standin-pass",
                )
                .await
                .unwrap(),
            ),
            Err(_) => None,
        };
        let (s, mut rx) = quick(60_000);

        s.command(&host, 1, "standin_siren", &json!({ "on": true }))
            .await
            .unwrap();
        assert!(next(&mut rx).await.on);
        assert_eq!(read(addr.clone()).await["on"], true);
        assert_eq!(read(addr.clone()).await["mode"], "manul");
        if let Some(p) = push.as_mut() {
            assert_eq!(
                p.next().await.unwrap(),
                crate::push::Pushed::Sirens(vec![(1, true)])
            );
        }

        s.command(&host, 1, "standin_siren", &json!({ "on": false }))
            .await
            .unwrap();
        assert!(!next(&mut rx).await.on);
        assert_eq!(read(addr.clone()).await["on"], false);
        if let Some(p) = push.as_mut() {
            assert_eq!(
                p.next().await.unwrap(),
                crate::push::Pushed::Sirens(vec![(1, false)])
            );
        }

        // A "times" play, stopped while it sounds: manual first, as firmware needs.
        let (s, mut rx) = quick(60_000);
        let mut long = Arc::try_unwrap(s).ok().unwrap();
        long.play = Duration::from_secs(30);
        let s = Arc::new(long);
        s.command(
            &host,
            1,
            "standin_siren",
            &json!({ "action": "sound", "times": 50 }),
        )
        .await
        .unwrap();
        assert!(next(&mut rx).await.on);
        assert_eq!(read(addr.clone()).await["mode"], "times");
        s.command(&host, 1, "standin_siren", &json!({ "on": false }))
            .await
            .unwrap();
        assert!(!next(&mut rx).await.on);
        assert_eq!(read(addr.clone()).await["on"], false);
        host.api.logout().await;
    }
}
