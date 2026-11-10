//! One address — a camera on its own, an NVR or a Home Hub — kept signed in,
//! its cameras registered, and watched.
//!
//! On (re)connecting it reads what the address is, its channels, and what
//! each camera can do; registers each camera with its spotlight and siren;
//! adds their streams to go2rtc; then asks every `poll_secs` what they see
//! (one request for all of an NVR's cameras). A detection that starts gets a
//! snapshot; a doorbell press is published as a button press. When the
//! address stops answering, its cameras go offline and an alert says so; it
//! is tried again with a growing wait.

use crate::api::{self, Detections, Features, Reolink};
use crate::camera;
use crate::config::{HostEntry, ReolinkSection};
use crate::go2rtc::Go2rtc;
use anyhow::{anyhow, Result};
use plugin_sdk_rs::types::PluginAlert;
use plugin_sdk_rs::DevicePublisher;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// How long a camera behind an NVR takes to show a spotlight change: read
/// sooner, and the NVR answers with the state from before it.
const LED_SETTLES: Duration = Duration::from_secs(2);

/// A snapshot at most this often per camera.
const SNAP_EVERY: Duration = Duration::from_secs(10);

/// What a device id is: which address, which channel, which part of it.
#[derive(Clone)]
pub struct Target {
    pub host: Arc<Host>,
    pub channel: u32,
    pub camera: String,
    pub kind: Kind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Camera,
    Spotlight,
    Siren,
}

/// What the plugin shares between its addresses and its command loop.
pub struct Ctx {
    pub publisher: DevicePublisher,
    pub cfg: ReolinkSection,
    pub files_dir: PathBuf,
    pub go2rtc: Option<Go2rtc>,
    pub targets: Mutex<HashMap<String, Target>>,
    /// The plugin's alerts now, by topic and key.
    pub alerts: Mutex<HashMap<(String, String), PluginAlert>>,
    /// Each address's last word, for the status action.
    pub status: Mutex<HashMap<String, Value>>,
    /// How many addresses are configured, and each one's devices once it has answered.
    pub hosts: usize,
    pub live: Mutex<HashMap<String, std::collections::HashSet<String>>>,
}

impl Ctx {
    pub async fn alert(&self, topic: &str, key: &str, reason: Option<String>) {
        let list: Vec<PluginAlert> = {
            let mut a = self.alerts.lock().expect("alerts");
            let k = (topic.to_string(), key.to_string());
            let changed = match &reason {
                Some(r) => {
                    let mut al = PluginAlert::new(topic, r.clone());
                    al.key = Some(key.to_string());
                    a.insert(k, al.clone()) != Some(al)
                }
                None => a.remove(&k).is_some(),
            };
            if !changed {
                return;
            }
            a.values().cloned().collect()
        };
        if let Err(e) = self.publisher.set_plugin_alerts(&list).await {
            warn!(error = %e, "alerts not published");
        }
    }

    /// An address answered with these devices. Once every address has, what
    /// homeCore holds that none of them has (a camera taken off an NVR, an
    /// NVR's empty slots) is retired; never sooner, or a camera behind an
    /// address that hadn't answered yet would go too.
    pub async fn answered(&self, host: &str, devices: std::collections::HashSet<String>) {
        let all = {
            let mut live = self.live.lock().expect("live");
            live.insert(host.to_string(), devices);
            (live.len() >= self.hosts).then(|| live.values().flatten().cloned().collect())
        };
        if let Some(all) = all {
            match self.publisher.reconcile_devices(all).await {
                Ok(r) => {
                    for id in &r.stale_unregistered {
                        info!(device_id = %id, "retired: no longer there");
                    }
                }
                Err(e) => warn!(error = %e, "reconcile failed"),
            }
        }
    }

    fn set_status(&self, host: &str, v: Value) {
        self.status
            .lock()
            .expect("status")
            .insert(host.to_string(), v);
    }
}

/// A camera as the plugin keeps it.
#[derive(Clone, Debug)]
pub struct Cam {
    pub id: String,
    pub channel: u32,
    pub name: String,
    pub model: String,
    pub online: bool,
    pub features: Features,
    pub presets: Vec<(u32, String)>,
    pub seen: Detections,
    pub last_snap: Option<Instant>,
}

impl Cam {
    /// Its devices in homeCore: itself, and a spotlight and siren only if it
    /// has them. Marking one it hasn't would make an empty device of it.
    pub fn device_ids(&self) -> Vec<String> {
        let mut ids = vec![self.id.clone()];
        if self.features.spotlight {
            ids.push(camera::spotlight_id(&self.id));
        }
        if self.features.siren {
            ids.push(camera::siren_id(&self.id));
        }
        ids
    }
}

pub struct Host {
    pub entry: HostEntry,
    pub api: Reolink,
    /// Events are arriving by push now: asking can be rare.
    pub pushing: std::sync::atomic::AtomicBool,
    pub cams: tokio::sync::Mutex<Vec<Cam>>,
    pub hub: Mutex<bool>,
}

impl Host {
    pub fn new(entry: HostEntry) -> Result<Arc<Self>> {
        let api = Reolink::new(
            &entry.address,
            entry.https,
            &entry.username,
            &entry.password,
        )?;
        Ok(Arc::new(Self {
            entry,
            api,
            pushing: std::sync::atomic::AtomicBool::new(false),
            cams: tokio::sync::Mutex::new(Vec::new()),
            hub: Mutex::new(false),
        }))
    }

    fn label(&self) -> &str {
        if self.entry.name.is_empty() {
            &self.entry.id
        } else {
            &self.entry.name
        }
    }
}

/// After a refused sign-in, this long before the next: Reolink locks an
/// account for a while after five refusals, and a quick retry would lock it,
/// for everything else that signs in with it too.
pub const AFTER_REFUSAL: Duration = Duration::from_secs(600);

/// Keep one address signed in and watched, for as long as the plugin runs.
pub async fn run(ctx: Arc<Ctx>, host: Arc<Host>) {
    if ctx.cfg.push {
        tokio::spawn(push_loop(ctx.clone(), host.clone()));
    }
    let mut wait = Duration::from_secs(5);
    loop {
        match connect(&ctx, &host).await {
            Ok(()) => {
                ctx.alert("host_unreachable", &host.entry.id, None).await;
                ctx.alert("login_refused", &host.entry.id, None).await;
                wait = Duration::from_secs(5);
                if let Err(e) = watch(&ctx, &host).await {
                    warn!(host = %host.entry.id, error = %e, "stopped answering");
                    went_offline(&ctx, &host, &e).await;
                }
            }
            Err(e) => {
                warn!(host = %host.entry.id, error = %e, "can't connect");
                went_offline(&ctx, &host, &e).await;
                if e.downcast_ref::<api::LoginRefused>().is_some() {
                    // A refusal isn't fixed by trying again: wait for a new password.
                    tokio::time::sleep(AFTER_REFUSAL).await;
                    continue;
                }
            }
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(120));
    }
}

async fn went_offline(ctx: &Ctx, host: &Host, e: &anyhow::Error) {
    let refused = e.downcast_ref::<api::LoginRefused>().is_some();
    let (topic, reason) = if refused {
        (
            "login_refused",
            format!(
                "{} refused the sign-in: check the user name and password. Trying again in 10 minutes, or when the settings change.",
                host.label()
            ),
        )
    } else {
        (
            "host_unreachable",
            format!("{} isn't answering.", host.label()),
        )
    };
    ctx.alert(topic, &host.entry.id, Some(reason.clone())).await;
    ctx.set_status(&host.entry.id, json!({ "ok": false, "why": reason }));
    for c in host.cams.lock().await.iter() {
        for id in c.device_ids() {
            let _ = ctx.publisher.set_available(&id, false).await;
        }
    }
}

/// Find out what the address is and what its cameras can do, and register them.
async fn connect(ctx: &Ctx, host: &Arc<Host>) -> Result<()> {
    let e = &host.entry;
    let dev = api::parse_dev_info(&host.api.one(api::cmd("GetDevInfo", json!({}))).await?);
    let own_name = if e.name.is_empty() {
        dev.name.clone()
    } else {
        e.name.clone()
    };
    let channels: Vec<api::ChannelInfo> = if dev.hub {
        api::parse_channels(
            &host
                .api
                .one(api::cmd("GetChannelstatus", json!({})))
                .await?,
        )
    } else {
        // A camera on its own is one camera, whatever channelNum says: a Duo
        // joins its lenses into one picture on channel 0.
        vec![api::ChannelInfo {
            channel: 0,
            name: own_name.clone(),
            online: true,
            model: dev.model.clone(),
            uid: String::new(),
        }]
    };
    let channels: Vec<api::ChannelInfo> = channels.into_iter().filter(|c| !c.empty()).collect();
    // Only an NVR or Hub is asked again which cameras it has.
    *host.hub.lock().expect("hub") = dev.hub;
    let ability = host
        .api
        .one(api::cmd(
            "GetAbility",
            json!({ "User": { "userName": e.username } }),
        ))
        .await
        .unwrap_or(Value::Null);

    let mut cams = Vec::new();
    for ch in channels {
        let id = camera::camera_id(&e.id, dev.hub.then_some(ch.channel));
        let mut f = api::parse_abilities(&api::ability_for(&ability, ch.channel));
        let (seen, sees) = read_events(host, ch.channel).await.unwrap_or_default();
        f.person = sees.person;
        f.vehicle = sees.vehicle;
        f.animal = sees.animal;
        f.package = sees.package;
        f.doorbell = sees.doorbell || ch.model.to_lowercase().contains("doorbell");
        let presets = if f.ptz {
            host.api
                .one(api::cmd("GetPtzPreset", json!({ "channel": ch.channel })))
                .await
                .map(|v| api::parse_presets(&v))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let name = if ch.name.is_empty() {
            format!("{} {}", host.label(), ch.channel + 1)
        } else {
            ch.name.clone()
        };
        // An NVR's list leaves out each camera's model: ask.
        let model = if !ch.model.is_empty() {
            ch.model.clone()
        } else if dev.hub {
            host.api
                .one(api::cmd("GetChnTypeInfo", json!({ "channel": ch.channel })))
                .await
                .map(|v| api::parse_type_info(&v))
                .unwrap_or_default()
        } else {
            dev.model.clone()
        };
        cams.push(Cam {
            id,
            channel: ch.channel,
            name,
            model,
            online: ch.online,
            features: f,
            presets,
            seen,
            last_snap: None,
        });
    }

    for c in &cams {
        register(ctx, host, c).await?;
    }
    info!(host = %e.id, model = %dev.model, cameras = cams.len(), "connected");
    ctx.set_status(
        &e.id,
        json!({ "ok": true, "model": dev.model, "firmware": dev.firmware, "hub": dev.hub,
                "cameras": cams.iter().map(|c| json!({ "id": c.id, "name": c.name, "channel": c.channel + 1, "online": c.online })).collect::<Vec<_>>() }),
    );
    // A camera has more than this account may use: say so, once, by address.
    let denied: Vec<&str> = {
        let mut d: Vec<&str> = cams
            .iter()
            .flat_map(|c| c.features.denied.iter().copied())
            .collect();
        d.sort();
        d.dedup();
        d
    };
    ctx.alert(
        "account_limited",
        &e.id,
        (!denied.is_empty()).then(|| {
            format!(
                "{} is signed in as {}, who may not use {}. Sign in with an account that may, to control them here.",
                host.label(),
                e.username,
                list(&denied)
            )
        }),
    )
    .await;
    let live: std::collections::HashSet<String> = cams.iter().flat_map(Cam::device_ids).collect();
    *host.cams.lock().await = cams;
    ctx.answered(&e.id, live).await;
    Ok(())
}

/// What a channel sees, and what it can detect: `GetEvents`, else the older pair.
async fn read_events(host: &Host, channel: u32) -> Result<(Detections, Features)> {
    match host
        .api
        .one(api::cmd("GetEvents", json!({ "channel": channel })))
        .await
    {
        Ok(v) => Ok(api::parse_events(&v)),
        Err(_) => {
            let r = host
                .api
                .call(&[
                    api::cmd("GetMdState", json!({ "channel": channel })),
                    api::cmd("GetAiState", json!({ "channel": channel })),
                ])
                .await?;
            let md = r
                .first()
                .and_then(|x| x.as_ref().ok())
                .cloned()
                .unwrap_or_default();
            let ai = r
                .get(1)
                .and_then(|x| x.as_ref().ok())
                .cloned()
                .unwrap_or_default();
            Ok(api::parse_md_ai(&md, &ai))
        }
    }
}

async fn register(ctx: &Ctx, host: &Arc<Host>, c: &Cam) -> Result<()> {
    let p = &ctx.publisher;
    p.register_device_full(&c.id, &c.name, Some("camera"), None, None)
        .await?;
    p.register_device_schema_json(&c.id, &camera::camera_schema(&c.features, &c.presets))
        .await?;
    p.subscribe_commands(&c.id).await?;
    let mut state = camera::detections_state(&c.seen, &c.features);
    state["model"] = json!(c.model);
    if let Some(g) = &ctx.go2rtc {
        let (sub, hd) = if ctx.cfg.go2rtc.add_streams {
            let (s, h) = camera::stream_names(&c.id);
            (Some(s), Some(h))
        } else {
            // Names someone set up in go2rtc, by the camera's place in the list.
            let named = |l: &Vec<String>| {
                l.get(c.channel as usize)
                    .filter(|n| !n.trim().is_empty())
                    .map(|n| n.trim().to_string())
            };
            (named(&host.entry.streams), named(&host.entry.streams_hd))
        };
        state["stream"] = json!(sub);
        state["stream_hd"] = json!(hd);
        if let (true, Some(sub), Some(hd)) = (ctx.cfg.go2rtc.add_streams, &sub, &hd) {
            let e = &host.entry;
            let (s_sub, s_main) =
                camera::rtsp_sources(&e.address, e.rtsp_port, &e.username, &e.password, c.channel);
            let ok = g.put(sub, &s_sub).await.and(g.put(hd, &s_main).await);
            ctx.alert(
                "go2rtc_unreachable",
                "",
                ok.as_ref()
                    .err()
                    .map(|e| format!("go2rtc didn't take the cameras' streams: {e}")),
            )
            .await;
        }
    }
    if c.features.doorbell {
        state["pushed"] = Value::Null;
    }
    // Its newest still from before a restart, so its card isn't blank until it sees something.
    if let Some((rel, ms)) = newest_snapshot(&ctx.files_dir.join("snap"), &c.id).await {
        state["snapshot"] = json!(rel);
        if let Some(at) = chrono::DateTime::from_timestamp_millis(ms) {
            state["snapshot_at"] = json!(at.to_rfc3339());
        }
    }
    if c.features.ir {
        if let Ok(v) = host
            .api
            .one(api::cmd("GetIrLights", json!({ "channel": c.channel })))
            .await
        {
            let auto = v["IrLights"]["state"].as_str() != Some("Off");
            state["ir_mode"] = json!(if auto { "auto" } else { "off" });
        }
    }
    p.publish_state(&c.id, &state).await?;
    p.set_available(&c.id, c.online).await?;
    let mut targets = vec![(c.id.clone(), Kind::Camera)];

    if c.features.spotlight {
        let id = camera::spotlight_id(&c.id);
        p.register_device_full(
            &id,
            &format!("{} spotlight", c.name),
            Some("light"),
            None,
            None,
        )
        .await?;
        p.register_device_schema(&id, &camera::spotlight_schema())
            .await?;
        p.subscribe_commands(&id).await?;
        let (on, bright) = host
            .api
            .one(api::cmd("GetWhiteLed", json!({ "channel": c.channel })))
            .await
            .map(|v| api::parse_white_led(&v))
            .unwrap_or((false, 100));
        p.publish_state(
            &id,
            &json!({ "on": on, "brightness_pct": if on { bright } else { 0 } }),
        )
        .await?;
        p.set_available(&id, c.online).await?;
        targets.push((id, Kind::Spotlight));
    }
    if c.features.siren {
        let id = camera::siren_id(&c.id);
        p.register_device_full(&id, &format!("{} siren", c.name), Some("siren"), None, None)
            .await?;
        p.register_device_schema_json(&id, &camera::siren_schema())
            .await?;
        p.subscribe_commands(&id).await?;
        p.publish_state(&id, &json!({ "on": false })).await?;
        p.set_available(&id, c.online).await?;
        targets.push((id, Kind::Siren));
    }
    let mut t = ctx.targets.lock().expect("targets");
    for (id, kind) in targets {
        t.insert(
            id,
            Target {
                host: host.clone(),
                channel: c.channel,
                camera: c.id.clone(),
                kind,
            },
        );
    }
    Ok(())
}

/// While push is up, asking is only a backstop.
const ASK_WHILE_PUSHING: Duration = Duration::from_secs(30);

/// Ask what each camera sees, every `poll_secs` (rarely while push is up),
/// until the address stops answering.
async fn watch(ctx: &Ctx, host: &Arc<Host>) -> Result<()> {
    let every = Duration::from_secs(ctx.cfg.poll_secs.max(1));
    let mut ticks: u64 = 0;
    let mut use_events = true;
    let mut since = Duration::ZERO;
    loop {
        tokio::time::sleep(every).await;
        since += every;
        if host.pushing.load(std::sync::atomic::Ordering::Relaxed) && since < ASK_WHILE_PUSHING {
            continue;
        }
        since = Duration::ZERO;
        ticks += 1;
        // An NVR's cameras come and go: look again now and then.
        if *host.hub.lock().expect("hub") && ticks.is_multiple_of(30) {
            let chans = api::parse_channels(
                &host
                    .api
                    .one(api::cmd("GetChannelstatus", json!({})))
                    .await?,
            );
            let mut cams = host.cams.lock().await;
            for c in cams.iter_mut() {
                if let Some(ch) = chans.iter().find(|x| x.channel == c.channel) {
                    if ch.online != c.online {
                        c.online = ch.online;
                        for id in c.device_ids() {
                            let _ = ctx.publisher.set_available(&id, ch.online).await;
                        }
                    }
                }
            }
        }
        let channels: Vec<u32> = host
            .cams
            .lock()
            .await
            .iter()
            .filter(|c| c.online)
            .map(|c| c.channel)
            .collect();
        if channels.is_empty() {
            continue;
        }
        let now: Vec<(u32, Detections)> = if use_events {
            let cmds: Vec<Value> = channels
                .iter()
                .map(|ch| api::cmd("GetEvents", json!({ "channel": ch })))
                .collect();
            let r = host.api.call(&cmds).await?;
            if r.iter().all(|x| x.is_err()) {
                // Older firmware: the pair from now on.
                use_events = false;
                continue;
            }
            channels
                .iter()
                .zip(r)
                .filter_map(|(ch, x)| x.ok().map(|v| (*ch, api::parse_events(&v).0)))
                .collect()
        } else {
            let mut out = Vec::new();
            for ch in &channels {
                out.push((*ch, read_events(host, *ch).await?.0));
            }
            out
        };
        for (ch, d) in now {
            seen(ctx, host, ch, d).await;
        }
        ctx.set_status(
            &host.entry.id,
            json!({ "ok": true, "watching": channels.len() }),
        );
    }
}

/// Events by push, for as long as the plugin runs: connect, sign in,
/// subscribe, and hand each to `seen` as asking does. Down, asking carries on
/// every `poll_secs` and an alert says so; it's tried again with a growing
/// wait, and ten minutes after a refused sign-in.
async fn push_loop(ctx: Arc<Ctx>, host: Arc<Host>) {
    use std::sync::atomic::Ordering;
    let e = &host.entry;
    let addr = e
        .address
        .split(':')
        .next()
        .unwrap_or(&e.address)
        .to_string();
    let mut wait = Duration::from_secs(5);
    loop {
        // Only once the address has answered and its cameras are known.
        if host.cams.lock().await.is_empty() {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        match crate::push::Push::connect(&addr, e.push_port, &e.username, &e.password).await {
            Ok(mut p) => {
                info!(host = %e.id, "events by push");
                host.pushing.store(true, Ordering::Relaxed);
                ctx.alert("push_down", &e.id, None).await;
                wait = Duration::from_secs(5);
                let why = loop {
                    match p.next().await {
                        Ok(list) => {
                            for s in list {
                                let d = Detections {
                                    motion: s.motion,
                                    person: s.person,
                                    vehicle: s.vehicle,
                                    animal: s.animal,
                                    package: s.package,
                                    visitor: s.visitor,
                                };
                                seen(&ctx, &host, s.channel, d).await;
                            }
                        }
                        Err(err) => break err,
                    }
                };
                host.pushing.store(false, Ordering::Relaxed);
                warn!(host = %e.id, error = %why, "push stopped");
            }
            Err(err) => {
                let refused = err.downcast_ref::<api::LoginRefused>().is_some();
                warn!(host = %e.id, error = %err, "no push");
                ctx.alert(
                    "push_down",
                    &e.id,
                    Some(format!(
                        "{} isn't pushing events ({err}), so they're asked for every {} s.",
                        host.label(),
                        ctx.cfg.poll_secs
                    )),
                )
                .await;
                if refused {
                    tokio::time::sleep(AFTER_REFUSAL).await;
                    continue;
                }
            }
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(300));
    }
}

/// Publish what changed for one camera; a new detection gets a snapshot.
async fn seen(ctx: &Ctx, host: &Arc<Host>, channel: u32, d: Detections) {
    let (cam, before, snap) = {
        let mut cams = host.cams.lock().await;
        let Some(c) = cams.iter_mut().find(|c| c.channel == channel) else {
            return;
        };
        if c.seen == d {
            return;
        }
        let before = c.seen;
        c.seen = d;
        let snap = d.began(&before) && c.last_snap.is_none_or(|t| t.elapsed() >= SNAP_EVERY);
        if snap {
            c.last_snap = Some(Instant::now());
        }
        (c.clone(), before, snap)
    };
    let p = &ctx.publisher;
    let _ = p
        .publish_state_partial(&cam.id, &camera::detections_state(&d, &cam.features))
        .await;
    // A press is a moment: set, then cleared, so the same button twice is two.
    if cam.features.doorbell && d.visitor && !before.visitor {
        let _ = p
            .publish_state_partial(&cam.id, &json!({ "pushed": 1 }))
            .await;
        let _ = p
            .publish_state_partial(&cam.id, &json!({ "pushed": null }))
            .await;
    }
    if snap {
        if let Err(e) = snapshot(ctx, host, &cam).await {
            warn!(camera = %cam.id, error = %e, "no snapshot");
        }
    }
}

/// Take a still, keep it by a new name, and drop the oldest past `snapshots_kept`.
pub async fn snapshot(ctx: &Ctx, host: &Host, cam: &Cam) -> Result<String> {
    let bytes = host.api.snap(cam.channel).await?;
    let at = chrono::Utc::now();
    let rel = format!("snap/{}-{}.jpg", cam.id, at.timestamp_millis());
    let dir = ctx.files_dir.join("snap");
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(ctx.files_dir.join(&rel), &bytes).await?;
    prune(&dir, &cam.id, ctx.cfg.snapshots_kept.max(1)).await;
    ctx.publisher
        .publish_state_partial(
            &cam.id,
            &json!({ "snapshot": rel, "snapshot_at": at.to_rfc3339() }),
        )
        .await?;
    Ok(rel)
}

/// A camera's snapshot files, by when they were taken.
async fn snapshots_of(dir: &std::path::Path, camera: &str) -> Vec<(i64, std::path::PathBuf)> {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return Vec::new();
    };
    let prefix = format!("{camera}-");
    let mut mine = Vec::new();
    while let Ok(Some(e)) = rd.next_entry().await {
        let n = e.file_name().to_string_lossy().to_string();
        if let Some(ms) = n
            .strip_prefix(&prefix)
            .and_then(|r| r.strip_suffix(".jpg"))
            .and_then(|r| r.parse::<i64>().ok())
        {
            mine.push((ms, e.path()));
        }
    }
    mine.sort();
    mine
}

/// The newest one, as the files folder names it.
async fn newest_snapshot(dir: &std::path::Path, camera: &str) -> Option<(String, i64)> {
    let (ms, _) = snapshots_of(dir, camera).await.pop()?;
    Some((format!("snap/{camera}-{ms}.jpg"), ms))
}

/// The camera's snapshots past the newest `keep`, removed.
async fn prune(dir: &std::path::Path, camera: &str, keep: usize) {
    let mine = snapshots_of(dir, camera).await;
    let extra = mine.len().saturating_sub(keep);
    for (_, path) in mine.into_iter().take(extra) {
        let _ = tokio::fs::remove_file(path).await;
    }
}

/// "a, b and c".
fn list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Carry out a command for one of this plugin's devices.
pub async fn command(ctx: &Ctx, device_id: &str, payload: &Value) -> Result<()> {
    let t = ctx
        .targets
        .lock()
        .expect("targets")
        .get(device_id)
        .cloned()
        .ok_or_else(|| anyhow!("not one of this plugin's devices"))?;
    let h = &t.host;
    let p = &ctx.publisher;
    let action = payload.get("action").and_then(Value::as_str);
    match t.kind {
        Kind::Spotlight => {
            let pct = payload.get("brightness_pct").and_then(Value::as_f64);
            let on = payload.get("on").and_then(Value::as_bool);
            let (want_on, level) = match (on, pct) {
                (_, Some(p)) => match camera::spotlight_level(p) {
                    Some(l) => (true, Some(l)),
                    None => (false, None),
                },
                (Some(o), None) => (o, None),
                (None, None) => return Ok(()),
            };
            h.api
                .one(api::set_spotlight(t.channel, want_on, level))
                .await?;
            // What was asked, now: an NVR answers a read straight after with
            // the camera's state from before the command.
            let asked = match (want_on, level) {
                (false, _) => json!({ "on": false, "brightness_pct": 0 }),
                (true, Some(l)) => json!({ "on": true, "brightness_pct": l }),
                // On at its own brightness: that stays as last read.
                (true, None) => json!({ "on": true }),
            };
            p.publish_state_partial_for_command(device_id, &asked, payload, "reolink")
                .await?;
            // Then what it is, once the camera has caught up.
            tokio::time::sleep(LED_SETTLES).await;
            if let Ok(v) = h
                .api
                .one(api::cmd("GetWhiteLed", json!({ "channel": t.channel })))
                .await
            {
                let (on, bright) = api::parse_white_led(&v);
                p.publish_state_partial(
                    device_id,
                    &json!({ "on": on, "brightness_pct": if on { bright } else { 0 } }),
                )
                .await?;
            }
        }
        Kind::Siren => {
            let times = match (action, payload.get("on").and_then(Value::as_bool)) {
                (Some("sound"), _) => payload["times"]
                    .as_u64()
                    .unwrap_or(ctx.cfg.siren_times as u64)
                    as u32,
                (None, Some(true)) => ctx.cfg.siren_times,
                (None, Some(false)) => return Ok(()),
                (Some(other), _) => anyhow::bail!("unknown action: {other}"),
                _ => return Ok(()),
            };
            h.api.one(api::siren(t.channel, times)).await?;
            p.publish_state_for_command(device_id, &json!({ "on": true }), payload, "reolink")
                .await?;
            // There's no reading a siren back: it's quiet again once it has sounded.
            tokio::time::sleep(Duration::from_secs(2 * times.max(1) as u64)).await;
            p.publish_state_partial(device_id, &json!({ "on": false }))
                .await?;
        }
        Kind::Camera => match action {
            Some("take_snapshot") => {
                let cam = cam_of(h, &t.camera).await?;
                snapshot(ctx, h, &cam).await?;
            }
            Some("ptz_move") => {
                let dir = payload["direction"].as_str().unwrap_or_default();
                let op = api::ptz_op(dir).ok_or_else(|| anyhow!("no such direction: {dir}"))?;
                let ms = payload["duration_ms"]
                    .as_u64()
                    .unwrap_or(500)
                    .clamp(100, 5000);
                h.api.one(api::ptz(t.channel, op, 32, None)).await?;
                tokio::time::sleep(Duration::from_millis(ms)).await;
                h.api.one(api::ptz(t.channel, "Stop", 32, None)).await?;
            }
            Some("goto_preset") => {
                let id = payload["preset"]
                    .as_u64()
                    .or_else(|| payload["preset"].as_str().and_then(|s| s.parse().ok()))
                    .ok_or_else(|| anyhow!("which preset?"))?;
                h.api
                    .one(api::ptz(t.channel, "ToPos", 32, Some(id as u32)))
                    .await?;
            }
            Some(other) => anyhow::bail!("unknown action: {other}"),
            None => {
                if let Some(mode) = payload.get("ir_mode").and_then(Value::as_str) {
                    h.api.one(api::set_ir(t.channel, mode != "off")).await?;
                    p.publish_state_for_command(
                        device_id,
                        &json!({ "ir_mode": if mode == "off" { "off" } else { "auto" } }),
                        payload,
                        "reolink",
                    )
                    .await?;
                }
            }
        },
    }
    Ok(())
}

async fn cam_of(h: &Host, id: &str) -> Result<Cam> {
    h.cams
        .lock()
        .await
        .iter()
        .find(|c| c.id == id)
        .cloned()
        .ok_or_else(|| anyhow!("no such camera"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_the_newest_still_of_a_camera_and_no_other() {
        let dir = tempfile::tempdir().unwrap();
        for n in [
            "reolink_a-5.jpg",
            "reolink_a-12.jpg",
            "reolink_a_1-99.jpg",
            "notes.txt",
        ] {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        assert_eq!(
            newest_snapshot(dir.path(), "reolink_a").await,
            Some(("snap/reolink_a-12.jpg".to_string(), 12))
        );
        assert_eq!(newest_snapshot(dir.path(), "reolink_b").await, None);
    }

    #[tokio::test]
    async fn keeps_only_the_newest_snapshots_of_each_camera() {
        let dir = tempfile::tempdir().unwrap();
        for ms in [1, 2, 3, 4] {
            std::fs::write(dir.path().join(format!("reolink_a-{ms}.jpg")), b"x").unwrap();
        }
        std::fs::write(dir.path().join("reolink_b-1.jpg"), b"x").unwrap();
        prune(dir.path(), "reolink_a", 2).await;
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec!["reolink_a-3.jpg", "reolink_a-4.jpg", "reolink_b-1.jpg"]
        );
    }
}
