//! The runtime's own page, on port 8790: paste the key, then see how it is doing.
//!
//! Before a key, the page asks for one and tests it (homeCore, then the broker)
//! before saving. After, it shows the connection and the plugins homeCore placed
//! here, and takes a new key only in the two cases that need one: the login
//! was reset (same runtime, new secret), or the runtime was removed from its
//! homeCore (the old key no longer works). A runtime that is still in use is
//! never repointed at another homeCore from this page, since that homeCore
//! would then decide what code runs here.
//!
//! No login on the page itself: it shows nothing secret, and the key it takes
//! has to be one homeCore made.

use crate::check;
use crate::core_client::CoreClient;
use crate::key_store;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use hc_api_types::plugin_runtimes::{ConnectionKey, HelloRequest};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    NotConnected,
    Connecting,
    Connected,
    Problem,
}

/// What the page shows. Never holds the secret.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    pub state: Phase,
    pub message: Option<String>,
    pub name: Option<String>,
    pub url: Option<String>,
    pub broker: Option<String>,
    pub username: Option<String>,
    pub kind: String,
    pub abi: String,
    pub arch: String,
    pub key_from_env: bool,
    pub last_heard: Option<chrono::DateTime<chrono::Utc>>,
    pub plugins: Vec<PluginRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginRow {
    pub plugin_id: String,
    pub version: String,
    pub since: chrono::DateTime<chrono::Utc>,
    pub looping: bool,
}

/// Everything the page and the main loop share.
pub struct Shared {
    pub status: Mutex<Status>,
    /// The key in use, as the main loop sees it.
    pub current: Mutex<Option<ConnectionKey>>,
    /// A new key from the page, already tested and saved.
    pub new_key: watch::Sender<Option<ConnectionKey>>,
    /// Hosted plugins crash-looping now, for the page's rows.
    pub looping: Mutex<HashSet<String>>,
    pub data_dir: PathBuf,
    pub hello: HelloRequest,
    pub broker_pin: Option<String>,
    pub http: reqwest::Client,
}

impl Shared {
    pub fn set(&self, f: impl FnOnce(&mut Status)) {
        if let Ok(mut s) = self.status.lock() {
            f(&mut s);
        }
    }
}

/// The broker host to dial for a key: the operator's pin, else the key's.
pub fn broker_host(key: &ConnectionKey, pin: &Option<String>) -> String {
    pin.clone().unwrap_or_else(|| key.broker_host.clone())
}

/// Test a key end to end. On success returns the runtime's name in homeCore.
pub async fn test_key(shared: &Shared, key: &ConnectionKey) -> anyhow::Result<String> {
    let caps = &shared.hello.capabilities;
    if key.kind != caps.kind || key.abi != caps.abi {
        anyhow::bail!(
            "this key is for a {} runtime ({}), and this container hosts {} ({})",
            key.kind,
            key.abi,
            caps.kind,
            caps.abi
        );
    }
    let client = CoreClient::new(shared.http.clone(), &key.url, &key.runtime_id, &key.secret);
    let hello = client.hello(&shared.hello).await?;
    let host = broker_host(key, &shared.broker_pin);
    check::broker_login(&host, key.broker_port, &key.username, &key.secret).await?;
    Ok(hello.name)
}

#[derive(Deserialize)]
struct KeyBody {
    key: String,
}

async fn status(State(shared): State<Arc<Shared>>) -> impl IntoResponse {
    let s = shared.status.lock().map(|s| s.clone()).unwrap_or_default();
    Json(s)
}

async fn put_key(
    State(shared): State<Arc<Shared>>,
    Json(body): Json<KeyBody>,
) -> impl IntoResponse {
    let refuse = |code: StatusCode, msg: String| (code, Json(serde_json::json!({ "error": msg })));

    if shared
        .status
        .lock()
        .map(|s| s.key_from_env)
        .unwrap_or(false)
    {
        return refuse(
            StatusCode::CONFLICT,
            format!(
                "this runtime's key is set by {} where the container starts; change it there",
                key_store::ENV
            ),
        );
    }
    let key = match ConnectionKey::decode(&body.key) {
        Ok(k) => k,
        Err(e) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, e),
    };

    // A runtime still in use stays with its homeCore. Its own new key (Reset
    // login) is fine; another homeCore's key is taken only once the old one has
    // stopped working, which is what removing the runtime there does.
    let current = shared.current.lock().ok().and_then(|c| c.clone());
    if let Some(cur) = current.filter(|c| c.runtime_id != key.runtime_id) {
        let old = CoreClient::new(shared.http.clone(), &cur.url, &cur.runtime_id, &cur.secret);
        if old.hello(&shared.hello).await.is_ok() {
            return refuse(
                StatusCode::CONFLICT,
                format!(
                    "this runtime is still added in homeCore at {}. Remove it there first, then paste the new key",
                    cur.url
                ),
            );
        }
    }

    let name = match test_key(&shared, &key).await {
        Ok(n) => n,
        Err(e) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")),
    };
    if let Err(e) = key_store::save(&shared.data_dir, &key) {
        return refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    }
    tracing::info!(runtime_id = %key.runtime_id, url = %key.url, "a new key was saved from the page");
    let _ = shared.new_key.send(Some(key));
    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "name": name })),
    )
}

async fn index() -> impl IntoResponse {
    Html(PAGE)
}

pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/key", post(put_key))
        .with_state(shared)
}

pub async fn serve(shared: Arc<Shared>, port: u16) {
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            tracing::info!(%port, "runtime page listening");
            if let Err(e) = axum::serve(listener, router(shared)).await {
                tracing::error!(error = %e, "runtime page stopped");
            }
        }
        Err(e) => tracing::error!(%port, error = %e, "couldn't open the runtime page"),
    }
}

const PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>homeCore plugin runtime</title>
<style>
:root{--bg:#f3f0e8;--side:#eeeae0;--surface:#fff;--sheet:#fbfaf6;--line:#e2dccf;--card:#e9e3d6;--ink:#1e2126;--muted:#5d6470;--accent:#9a4a07;--accent-ink:#fff;--good:#1d6b48;--good-bg:#e3f1e9;--warm:#8a5a10;--warm-bg:#fbe3bf;--alert:#b42318;--alert-bg:#fce9e6}
@media (prefers-color-scheme: dark){:root{--bg:#0d1015;--side:#0a0d11;--surface:#151a21;--sheet:#11151b;--line:#2a303b;--card:#232934;--ink:#e8eaee;--muted:#9aa3b0;--accent:#f2b544;--accent-ink:#1a1306;--good:#7fd1a8;--good-bg:#12291f;--warm:#f2c27a;--warm-bg:#2b2112;--alert:#ff8a80;--alert-bg:#2e1614}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--ink);font-family:system-ui,-apple-system,"Segoe UI",sans-serif}
header{display:flex;align-items:center;gap:10px;padding:0 24px;height:60px;border-bottom:1px solid var(--line);background:var(--side)}
header b{font-size:16px}header span{color:var(--muted);font-size:14px}
main{max-width:720px;margin:0 auto;padding:40px 16px;display:flex;flex-direction:column;gap:18px}
h1{margin:0;font-size:28px;flex:1}
.row{display:flex;align-items:center;gap:12px;flex-wrap:wrap}
.pill{display:inline-flex;align-items:center;gap:6px;height:28px;padding:0 12px;border-radius:14px;font-weight:700;font-size:13px;background:var(--side);color:var(--muted)}
.pill i{width:8px;height:8px;border-radius:4px;background:currentColor}
.pill.connected{background:var(--good-bg);color:var(--good)}.pill.connecting{background:var(--warm-bg);color:var(--warm)}.pill.problem{background:var(--alert-bg);color:var(--alert)}
.card{border-radius:14px;background:var(--surface);border:1px solid var(--card);padding:20px;display:flex;flex-direction:column;gap:12px}
p{margin:0}.muted{color:var(--muted);font-size:14px}.small{font-size:13px}
label{display:flex;flex-direction:column;gap:8px;font-weight:700;font-size:14px}
textarea,input{width:100%;padding:12px;border-radius:11px;border:1px solid var(--line);background:var(--sheet);color:var(--ink);font:14px ui-monospace,"IBM Plex Mono",monospace;line-height:1.5}
textarea{resize:vertical;min-height:84px}
button{height:42px;padding:0 20px;border-radius:21px;border:none;background:var(--accent);color:var(--accent-ink);font:inherit;font-weight:700;cursor:pointer}
button[disabled]{opacity:.6;cursor:default}
.plain{background:none;color:var(--ink);padding:8px 0;height:auto;display:flex;flex-wrap:wrap;align-items:center;gap:4px 8px;font-weight:700;text-align:left}
.err{padding:10px 14px;border-radius:11px;background:var(--alert-bg);color:var(--alert);font-weight:600;font-size:14px}
.note{padding:10px 14px;border-radius:11px;background:var(--warm-bg);color:var(--warm);font-weight:600;font-size:14px}
dl{margin:0;display:grid;grid-template-columns:160px minmax(0,1fr);font-size:14px}
dt,dd{margin:0;padding:11px 0;border-top:1px solid var(--line)}dt{color:var(--muted);font-weight:600}
dt:first-of-type,dd:first-of-type{border-top:none}
dd.mono{font-family:ui-monospace,"IBM Plex Mono",monospace;overflow-wrap:anywhere}
ul{list-style:none;margin:0;padding:0}
li{display:flex;align-items:center;gap:12px;padding:12px 0;border-top:1px solid var(--line);flex-wrap:wrap}
li .dot{width:8px;height:8px;border-radius:4px;background:var(--good)}li.looping .dot{background:var(--alert)}
li b{flex:1;min-width:160px}li .v{font-family:ui-monospace,monospace;color:var(--muted);font-size:13px}
li .s{min-width:170px;font-size:13px;font-weight:600;color:var(--good)}li.looping .s{color:var(--alert)}
.hidden{display:none !important}
@media (max-width:560px){dl{grid-template-columns:1fr}dt{padding-bottom:0}dd{border-top:none;padding-top:4px}}
</style>
</head>
<body>
<header><b>homeCore plugin runtime</b><span id="what"></span></header>
<main>
  <div class="row"><h1 id="title">Connect to homeCore</h1><span id="pill" class="pill"><i></i><span id="pilltext">Not connected</span></span></div>
  <p id="problem" class="err hidden"></p>

  <section id="setup" class="hidden" style="display:flex;flex-direction:column;gap:18px">
    <p class="muted" style="font-size:15px">In homeCore, open Settings, Plugins, Runtimes and choose Add runtime. Copy the key it shows and paste it here.</p>
    <div class="card" id="keycard"></div>
    <p class="muted small">Once it's connected, this page only shows status. A new key is taken after Reset login in homeCore, or after removing this runtime there.</p>
  </section>

  <section id="connected" class="hidden" style="display:flex;flex-direction:column;gap:18px">
    <div class="card" style="padding:6px 20px"><dl id="conn"></dl></div>
    <div class="card" style="padding:14px 20px 6px;gap:0">
      <b style="padding-bottom:6px">Plugins homeCore placed here</b>
      <ul id="plugins"></ul>
      <p id="noplugins" class="muted small" style="padding:10px 0">None yet. Install a Python plugin from the Catalog in homeCore and it appears here.</p>
      <p class="muted small" style="padding:6px 0 10px">Add and remove plugins in homeCore. This page shows what's here.</p>
    </div>
    <div class="card" style="padding:4px 20px;background:var(--sheet);border-style:dashed">
      <button class="plain" id="newkeybtn" style="height:48px" aria-expanded="false"><b style="white-space:nowrap">Use a new key</b> <span class="muted small" style="font-weight:500">After Reset login in homeCore, or after removing this runtime there to move it</span></button>
      <div id="newkey" class="hidden" style="padding-bottom:16px;display:flex;flex-direction:column;gap:12px"></div>
    </div>
  </section>
</main>
<template id="keyform">
  <label>Connection key<textarea name="key" placeholder="hcrt1." spellcheck="false" autocomplete="off"></textarea></label>
  <p class="err hidden" data-err></p>
  <div class="row"><button type="button" data-go>Connect</button><span class="muted small">It tries homeCore and the broker before saving, and says which one it couldn't reach.</span></div>
</template>
<script>
const $ = (id) => document.getElementById(id);
let envNote = false;
function ago(t){ if(!t) return ''; const s=Math.max(0,Math.round((Date.now()-Date.parse(t))/1000)); if(s<60) return s+' seconds ago'; const m=Math.round(s/60); if(m<60) return m+(m==1?' minute':' minutes')+' ago'; const h=Math.round(m/60); return h+(h==1?' hour':' hours')+' ago'; }
function runFor(t){ const s=Math.max(0,Math.round((Date.now()-Date.parse(t))/1000)); if(s<90) return 'Running'; const m=Math.round(s/60); if(m<90) return 'Running for '+m+' minutes'; const h=Math.round(m/60); if(h<48) return 'Running for '+h+' hours'; return 'Running for '+Math.round(h/24)+' days'; }
function text(el, v){ el.textContent = v; return el; }
function mountForm(where){
  where.replaceChildren($('keyform').content.cloneNode(true));
  const ta = where.querySelector('textarea'), go = where.querySelector('[data-go]'), er = where.querySelector('[data-err]');
  go.onclick = async () => {
    go.disabled = true; text(go,'Checking…'); er.classList.add('hidden');
    try {
      const r = await fetch('api/key',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({key:ta.value})});
      const j = await r.json().catch(()=>({}));
      if(!r.ok){ text(er, j.error || ('The runtime answered '+r.status)); er.classList.remove('hidden'); }
      else { ta.value=''; refresh(); }
    } catch(e){ text(er,'The runtime did not answer. Is it still running?'); er.classList.remove('hidden'); }
    go.disabled = false; text(go,'Connect');
  };
}
$('newkeybtn').onclick = () => { const n=$('newkey'); const open=n.classList.toggle('hidden')===false; $('newkeybtn').setAttribute('aria-expanded', open); if(open && !n.firstChild) mountForm(n); };
async function refresh(){
  let s; try { s = await (await fetch('api/status')).json(); } catch(e){ return; }
  text($('what'), (s.kind==='python'?'Python 3.12':s.kind)+' · '+s.arch+' · this container');
  const labels = {not_connected:'Not connected',connecting:'Connecting',connected:'Connected',problem:'Not connected'};
  $('pill').className = 'pill '+s.state; text($('pilltext'), labels[s.state]||s.state);
  const p = $('problem'); if(s.message){ text(p, s.message); p.classList.remove('hidden'); } else p.classList.add('hidden');
  const haveKey = !!s.url;
  text($('title'), haveKey && s.name ? s.name : 'Connect to homeCore');
  $('setup').classList.toggle('hidden', haveKey);
  $('connected').classList.toggle('hidden', !haveKey);
  if(!haveKey){
    const kc = $('keycard');
    if(s.key_from_env){ if(!envNote){ kc.replaceChildren(); const n=document.createElement('p'); n.className='note'; n.textContent='This runtime takes its key from HC_RUNTIME_KEY where the container starts.'; kc.append(n); envNote=true; } }
    else if(!kc.querySelector('textarea')) mountForm(kc);
    return;
  }
  const dl = $('conn'); dl.replaceChildren();
  for (const [k,v,mono] of [['homeCore',s.url,1],['Broker',s.broker,1],['Runtime id',s.username,1],['Last heard from', s.last_heard?'homeCore, '+ago(s.last_heard):'Not yet',0]]){
    dl.append(text(document.createElement('dt'),k)); const dd=text(document.createElement('dd'),v||''); if(mono) dd.className='mono'; dl.append(dd);
  }
  const ul = $('plugins'); ul.replaceChildren();
  for (const pl of s.plugins){
    const li=document.createElement('li'); if(pl.looping) li.className='looping';
    const dot=document.createElement('span'); dot.className='dot'; dot.setAttribute('aria-hidden','true');
    li.append(dot, text(document.createElement('b'),pl.plugin_id), Object.assign(text(document.createElement('span'),pl.version),{className:'v'}), Object.assign(text(document.createElement('span'), pl.looping?'Keeps stopping; restarting':runFor(pl.since)),{className:'s'}));
    ul.append(li);
  }
  $('noplugins').classList.toggle('hidden', s.plugins.length>0);
  $('newkeybtn').parentElement.classList.toggle('hidden', s.key_from_env);
}
refresh(); setInterval(refresh, 3000);
</script>
</body>
</html>
"##;
