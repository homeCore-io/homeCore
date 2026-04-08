# HomeCore Admin — Leptos First-Class Design

**Status:** Design draft — iterate before implementation  
**Goal:** A production-quality admin UI that is an integral part of the homeCore
binary, not a client consuming the REST API.

---

## Guiding Principle

> The admin UI is not a consumer of homeCore. It *is* homeCore.

Server functions called **during SSR** are inline function calls — same process,
same async executor, direct `AdminContext` access, zero HTTP overhead. The server
renders complete HTML with live data on every navigation request.

**After WASM hydration**, server function calls from the WASM client are HTTP POST
requests to generated endpoints (unavoidable — the browser cannot hold a Rust
reference). The design minimises post-hydration server fn calls: most pages are
static after the initial SSR render and only genuinely live components trigger
re-fetches.

---

## 1. Crate Architecture

### The circular dependency

`hc-api` → `hc-web-admin` (router mount)  
`hc-web-admin` server fns need `hc-api` types (`RuleFileStore`, `LogStreamState`) ← **circular**

Extracting `AppState` to a shared crate would drag those types with it — they are
`hc-api` submodules. A new extraction crate solves the same problem by moving it.

### Solution: trait objects in AdminContext

`hc-web-admin` defines the capabilities it needs as traits. `hc-api` implements
them on its existing types and passes `Arc<dyn Trait>` at router construction.
No new crate. No type migration. Two traits total.

```rust
// crates/hc-web-admin/src/context.rs
// (SSR-only — AdminContext never enters the WASM binary)
#[cfg(feature = "ssr")]
pub trait RuleWriter: Send + Sync {
    async fn list(&self) -> anyhow::Result<Vec<Rule>>;
    async fn set_enabled(&self, id: Uuid, enabled: bool) -> anyhow::Result<()>;
    async fn upsert(&self, rule: Rule) -> anyhow::Result<()>;
    async fn delete(&self, id: Uuid) -> anyhow::Result<()>;
}

#[cfg(feature = "ssr")]
pub trait LogTap: Send + Sync {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<LogEntry>;
}

#[cfg(feature = "ssr")]
#[derive(Clone)]
pub struct AdminContext {
    pub store:       StateStore,
    pub pub_bus:     EventBus,              // typed events — never internal_bus
    pub jwt:         Arc<JwtService>,
    pub publish:     Option<PublishHandle>,
    pub rules:       Option<Arc<RwLock<Vec<Rule>>>>,  // read handle for rule list
    pub rule_writer: Option<Arc<dyn RuleWriter>>,
    pub log_tap:     Option<Arc<dyn LogTap>>,
}
```

```rust
// crates/hc-api/src/lib.rs
impl RuleWriter for RuleFileStore { ... }
impl LogTap for LogStreamState { ... }

// Construction — no circular dep
hc_web_admin::router(AdminContext {
    store:       state.store.clone(),
    pub_bus:     state.pub_bus.clone(),
    jwt:         Arc::clone(&state.jwt),
    publish:     state.publish.clone(),
    rules:       state.rules_handle.clone(),
    rule_writer: state.rule_file_store.as_ref()
                     .map(|s| Arc::new(s.clone()) as Arc<dyn RuleWriter>),
    log_tap:     state.log_stream.as_ref()
                     .map(|s| Arc::new(s.clone()) as Arc<dyn LogTap>),
})
```

---

## 2. WASM Binary Compatibility

`AdminContext` contains `StateStore` (redb/SQLite), `EventBus` (tokio broadcast),
`PublishHandle` (rumqttc) — none compile to `wasm32-unknown-unknown`. The WASM
(`hydrate`) build must never see these types.

### Rule: all server-side imports are SSR-only

```rust
// hc-web-admin/src/server/devices.rs
#[cfg(feature = "ssr")]
use crate::context::AdminContext;

#[server]
async fn get_device(id: Uuid) -> Result<DeviceState, ServerFnError> {
    // Body only compiles under ssr — correct
    let ctx = expect_context::<AdminContext>();
    ctx.store.get_device(id).await.map_err(Into::into)
}
// The server fn *signature* (return type) is compiled by both targets.
// DeviceState must be Serialize + Deserialize and have no SSR-only dependencies.
```

### hc-types WASM feature additions

`Cargo.toml` for the `hydrate` feature must enable:
```toml
[target.'cfg(target_arch = "wasm32")'.dependencies]
chrono = { workspace = true, features = ["wasmbind"] }
uuid   = { workspace = true, features = ["js"] }
```

### Feature flags in hc-web-admin/Cargo.toml

```toml
[features]
ssr     = ["leptos/ssr", "leptos_axum/default", "hc-types/default", ...]
hydrate = ["leptos/hydrate", "wasm-bindgen", "web-sys", ...]

[dependencies]
# SSR-only deps
leptos_axum  = { version = "0.8", optional = true }
hc-state     = { path = "../hc-state", optional = true }
# ...

# Both targets
leptos       = { version = "0.8", features = [] }
serde        = { features = ["derive"] }
hc-types     = { path = "../hc-types" }  # must be WASM-safe
```

---

## 3. Build System

```
cargo-leptos 0.3.x
  ├── SSR target (native)   → compiled into homecore binary
  └── hydrate target (wasm) → bundled via rust-embed (Phase 0)
```

`rust-embed` is Phase 0 infrastructure. Without it, production requires a loose
WASM directory next to the binary.

### Required `[package.metadata.leptos]` fields

Lives in `core/Cargo.toml` (the workspace binary crate):

```toml
[package.metadata.leptos]
output-name     = "hc-web-admin"
site-root       = "site"           # cargo-leptos output dir
site-pkg-dir    = "pkg"            # WASM + JS go here under site-root
style-file      = "crates/hc-web-admin/style/admin.css"
assets-dir      = "crates/hc-web-admin/assets"
lib-features    = ["hydrate"]
bin-features    = ["ssr"]
```

**Dev:** `cargo leptos watch` (from `core/`)  
**Prod:** `cargo leptos build --release` → single binary  
**CI without UI:** `cargo build` — ssr/hydrate features inactive

---

## 4. Auth Design

Cookie-based session, independent of the REST API JWT flow.

### Flow

1. `GET /admin/*` → server renders login page if no valid session cookie
2. Login server fn → verifies credentials → uses `ResponseOptions` to set cookie
3. All protected server fns extract and verify `AdminSession` from the request
4. Logout server fn → clears cookie via `ResponseOptions`

### Cookie setting via ResponseOptions

```rust
#[server]
async fn login(username: String, password: String) -> Result<(), ServerFnError> {
    let ctx = expect_context::<AdminContext>();
    let resp = expect_context::<leptos_axum::ResponseOptions>();

    // Verify credentials
    let user = ctx.store.get_user_by_name(&username).await
        .map_err(|_| ServerFnError::new("invalid credentials"))?;
    ctx.jwt.verify_password(&password, &user.password_hash)
        .map_err(|_| ServerFnError::new("invalid credentials"))?;
    if user.role != Role::Admin {
        return Err(ServerFnError::new("admin role required"));
    }

    let token = ctx.jwt.issue_token(user.id, user.role)?;
    resp.insert_header(
        http::header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "hc-admin-session={token}; HttpOnly; Secure; SameSite=Strict; Path=/admin"
        ))?,
    );
    leptos_axum::redirect("/admin/");
    Ok(())
}
```

### AdminSession extraction inside server fns

Server fns do not take axum extractor parameters — auth must be extracted from
context explicitly:

```rust
#[cfg(feature = "ssr")]
async fn require_admin() -> Result<AdminClaims, ServerFnError> {
    let req = expect_context::<leptos_axum::RequestParts>();
    let cookies = req.headers.get(http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = extract_session_cookie(cookies)
        .ok_or_else(|| ServerFnError::new("unauthenticated"))?;
    let ctx = expect_context::<AdminContext>();
    ctx.jwt.verify_admin(token).map_err(|_| ServerFnError::new("unauthenticated"))
}

// Every mutating server fn:
#[server]
async fn set_rule_enabled(id: Uuid, enabled: bool) -> Result<(), ServerFnError> {
    require_admin().await?;
    let ctx = expect_context::<AdminContext>();
    ctx.rule_writer.as_ref().unwrap().set_enabled(id, enabled).await
        .map_err(Into::into)
}
```

`SameSite=Strict` + same-origin admin makes CSRF a non-issue.

---

## 5. AdminContext Injection into Leptos

`expect_context::<AdminContext>()` requires `provide_context` to have been called
before any server fn runs. Axum `State<AdminContext>` is not automatically a Leptos
context. The wiring uses `leptos_axum::LeptosRoutes::leptos_routes_with_context`:

```rust
// In hc-web-admin/src/lib.rs — called from hc-api router construction
pub fn router(ctx: AdminContext) -> Router {
    let leptos_options = get_leptos_options();
    let routes = generate_route_list(App);

    Router::new()
        .route("/stream", get(admin_event_stream))
        .leptos_routes_with_context(
            &leptos_options,
            routes,
            {
                let ctx = ctx.clone();
                move || { provide_context(ctx.clone()); }
            },
            || view! { <App /> },
        )
        .with_state(ctx)   // also needed for the /stream axum handler
}
```

---

## 6. Core Architecture: Server Renders Everything

The fundamental model is **server-rendered navigation**, not a SPA:

```
User navigates to /admin/devices
  → Axum routes to Leptos SSR handler
  → <Suspense> boundaries cause Leptos to await create_resource() calls inline
  → Server fn get_devices_page() runs — direct StateStore call, no HTTP
  → Server renders complete HTML with device data embedded
  → Browser displays full page instantly
  → WASM hydrates the interactive islands (filters, toggles, live components)
  → No client-side data fetch for initial render

User clicks a nav link (after WASM hydrated)
  → Leptos client-side router handles the navigation
  → Component unmounts, new component mounts
  → create_resource() fires server fn as HTTP POST (not inline SSR)
  → Data arrives, <Suspense> shows fallback briefly then new content
  → This is unavoidable after hydration — minimise by keeping Tier 1 data fresh
```

---

## 7. Data Patterns — Three Tiers

### Tier 1: Static / slow-changing (SSR, no live updates)

Rules, Scenes, Areas, Plugins, System. Data changes only on user action.

```rust
// version signal — incremented after mutations to force refetch
let rules_version = RwSignal::new(0u64);

// Resource key includes version — re-runs when version changes
let rules = create_resource(
    move || rules_version.get(),
    |_| get_rules(),
);

// MUST be inside <Suspense> for SSR to await and embed the data
view! {
    <Suspense fallback=|| view! { <p class="loading">"Loading…"</p> }>
        {move || rules.get().map(|rs| view! {
            <RuleTable rules=rs version=rules_version />
        })}
    </Suspense>
}
```

After a mutation action completes, increment `rules_version`:

```rust
let toggle = create_server_action::<SetRuleEnabled>();
create_effect(move |_| {
    if toggle.value().get().is_some() {
        rules_version.update(|n| *n += 1);
    }
});
```

### Tier 2: Mutations — `<ActionForm>` (progressive enhancement)

Works without WASM. Server fn writes to store, response redirects back to list.

```rust
// Device commands → MQTT (hardware path)
#[server]
async fn send_device_command(
    device_id: String,
    payload: serde_json::Value,
) -> Result<(), ServerFnError> {
    require_admin().await?;
    let ctx = expect_context::<AdminContext>();
    let topic = format!("homecore/devices/{device_id}/cmd");
    ctx.publish.as_ref()
        .ok_or_else(|| ServerFnError::new("publish unavailable"))?
        .send(topic, payload).await
        .map_err(Into::into)
}

// Metadata mutations → direct stores (no MQTT)
#[server]
async fn set_rule_enabled(id: Uuid, enabled: bool) -> Result<(), ServerFnError> {
    require_admin().await?;
    let ctx = expect_context::<AdminContext>();
    ctx.rule_writer.as_ref().unwrap()
        .set_enabled(id, enabled).await
        .map_err(Into::into)
}
```

**Device commands must go through MQTT.** Direct `StateStore` writes for device
state bypass hardware and produce phantom state.

### Tier 3: Genuinely live (SSE → trigger signal → Suspense/Transition + resource)

Only components that need per-event reactivity. The client never holds mutable
state — the SSE event increments a version, the resource re-reads from `StateStore`.

```rust
let device_version = RwSignal::new(0u64);

// Both closures need device_id — use Copy (Uuid) or clone explicitly
let id = device_id;  // Uuid is Copy
let device = create_resource(
    move || (id, device_version.get()),
    move |(id, _)| get_device(id),
);

// <Transition> keeps previous value visible during refetch (no flicker)
view! {
    <Transition fallback=|| view! { <p>"Loading…"</p> }>
        {move || device.get().map(|d| view! { <AttributeTable device=d /> })}
    </Transition>
}

// SSE drives the trigger — effect only runs on client, not during SSR
create_effect(move |_| {
    let ctx = expect_context::<SseContext>();
    if let Some(ev) = ctx.device_state.get() {
        if ev.device_id == id.to_string() {
            device_version.update(|n| *n += 1);
        }
    }
});
```

---

## 8. SseContext — Client-Only, Per-Type Signals

### Client-only creation

`EventSource` is a browser API. The shell must not connect during SSR:

```rust
// In <AdminShell> — create_effect never runs on SSR
create_effect(move |_| {
    let ctx = SseContext::connect("/admin/stream");
    provide_context(ctx);
});
```

### Per-type signals — no event storm

A single `last_event: RwSignal<Option<Event>>` causes every subscribed effect to
re-evaluate on every SSE event, regardless of type. With 10 events/second and
multiple Tier 3 components, that is significant unnecessary reactive work.

`SseContext` exposes per-event-type signals. Effects subscribe only to what they
need and are unaffected by unrelated event types.

```rust
pub struct SseContext {
    pub device_state:        RwSignal<Option<DeviceStateChangedEvent>>,
    pub device_availability: RwSignal<Option<DeviceAvailabilityEvent>>,
    pub rule_fired:          RwSignal<Option<RuleFiredEvent>>,
    pub scene_activated:     RwSignal<Option<SceneActivatedEvent>>,
    pub log_entry:           RwSignal<Option<LogEntryEvent>>,
}

impl SseContext {
    pub fn connect(url: &str) -> Self {
        let ctx = Self::default();
        let es = web_sys::EventSource::new(url).unwrap();
        {
            let ctx = ctx.clone();
            let cb = Closure::wrap(Box::new(move |e: MessageEvent| {
                if let Ok(ev) = serde_json::from_str::<Event>(&e.data().as_string().unwrap_or_default()) {
                    match ev {
                        Event::DeviceStateChanged(e)    => ctx.device_state.set(Some(e)),
                        Event::DeviceAvailability(e)    => ctx.device_availability.set(Some(e)),
                        Event::RuleFired(e)             => ctx.rule_fired.set(Some(e)),
                        Event::SceneActivated(e)        => ctx.scene_activated.set(Some(e)),
                        Event::LogEntry(e)              => ctx.log_entry.set(Some(e)),
                    }
                }
            }) as Box<dyn FnMut(_)>);
            es.set_onmessage(Some(cb.as_ref().unchecked_ref()));
            cb.forget();
        }
        ctx
    }
}
```

---

## 9. SSE Endpoint — use `axum::response::sse::Sse`

`StreamBody` with manually formatted strings is fragile and has no keepalive.
Axum 0.7 ships `axum::response::sse::{Sse, Event, KeepAlive}`:

```rust
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use tokio::sync::broadcast::error::RecvError;

async fn admin_event_stream(
    State(ctx): State<AdminContext>,
    session: AdminSession,
    Query(params): Query<SseParams>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let mut rx = ctx.pub_bus.subscribe();
    let filter = params.event_types;

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if filter.is_empty() || filter.contains(&ev.type_name()) {
                        let data = serde_json::to_string(&ev).unwrap_or_default();
                        yield Ok(SseEvent::default().data(data));
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // Ring buffer overflowed — client must re-fetch state
                    yield Ok(SseEvent::default().event("resync").data("{}"));
                }
                Err(RecvError::Closed) => break,
            }
        }
    };

    // keep_alive sends a comment ping every 15s — prevents proxy/NAT timeouts
    Sse::new(stream).keep_alive(KeepAlive::default())
}
```

On `resync` or reconnect: client increments all active version triggers →
resources re-fetch from `StateStore` → consistent snapshot.

---

## 10. Devices Table — Single Island, Not Per-Row Dots

"Availability dot as Tier 3 island per device row" means 200 devices = 200
hydration roots. Each has its own hydration ID, serialised state, and mount cost.

Instead: the entire `<DevicesTable>` is one Tier 3 component, version-triggered.
When any availability event arrives, the version increments, the server fn returns
the current page of devices, Leptos diffs only the changed rows. One hydration
boundary, one SSE subscription, one resource.

```rust
// DevicesPage — single Tier 3 resource covering the whole table
let devices_version = RwSignal::new(0u64);
let devices = create_resource(
    move || (page.get(), filter.get(), devices_version.get()),
    move |(p, f, _)| get_devices_page(p, f),
);

// Reacts to availability changes via SseContext per-type signal
create_effect(move |_| {
    let ctx = expect_context::<SseContext>();
    if ctx.device_availability.get().is_some() {
        devices_version.update(|n| *n += 1);
    }
});
```

---

## 11. Which Components Use Which Tier

| Page / Component         | Tier | SSE signal       | Notes |
|--------------------------|------|------------------|-------|
| Rules list               | 1    | —                | Invalidated by toggle/edit mutations |
| Rule editor              | 2    | —                | Mutation + redirect |
| Scenes list              | 1    | —                | Invalidated by activate/edit |
| Areas CRUD               | 2    | —                | Mutation only |
| Plugins registry         | 1    | —                | 60s version refetch |
| System stats             | 1    | —                | 30s version refetch |
| Devices table            | 3    | device_availability | Single island, version-triggered |
| Device detail attributes | 3    | device_state     | (id, version) resource key |
| Device command panel     | 2    | —                | ActionForm → MQTT |
| Device history           | 1    | —                | Paginated, on demand |
| Overview stats           | 1    | —                | SSR counts |
| Overview event feed      | 3    | all types        | Append-only reactive list |
| Events page              | 3    | all types        | Live + history query |
| Logs page                | 3    | log_entry        | Append-only log list |
| Modes switcher           | 2    | —                | Toggle + redirect |

---

## 12. Component Hierarchy

```
<App>                           ← leptos router, AdminSession context
  <LoginPage>                   ← server fn (ResponseOptions for cookie), redirect
  <AdminShell>                  ← SSR: sidebar + topbar + <Outlet>
    // create_effect: SseContext::connect("/admin/stream") — client-only
    // SseContext provided here with per-type signals
    <OverviewPage>              ← Tier 1 stats in <Suspense>; Tier 3 event feed
    <DevicesPage>               ← Tier 3 single island, (page, filter, version)
    <DeviceDetailPage>          ← Tier 3: (id, version) resource in <Transition>
      <AttributeTable />
      <CommandPanel />          ← ActionForm → send_device_command → MQTT
      <DeviceHistory />         ← Tier 1, paginated, <Suspense>
    <RulesPage>                 ← Tier 1 <Suspense>; version signal; ActionForm toggles
    <RuleEditorPage>            ← Tier 2 mutations
    <ScenesPage>                ← Tier 1 <Suspense>; ActionForm activate
    <ModesPage>                 ← Tier 2 toggle → StateStore
    <AreasPage>                 ← Tier 2 CRUD
    <EventsPage>                ← Tier 3 live list; Tier 1 history query
    <PluginsPage>               ← Tier 1 <Suspense>; 60s version refetch
    <LogsPage>                  ← Tier 3, log_entry signal only
    <SystemPage>                ← Tier 1 <Suspense>; 30s version refetch
```

---

## 13. Styling

**No Tailwind.** Pure CSS custom properties — no Node.js in the Rust build
pipeline. `cargo-leptos` has no Tailwind integration without a custom build hook.
Theme tokens in `admin.css`, embedded via `rust-embed`.

---

## 14. Iteration Plan

### Phase 0 — Infrastructure gate: both builds pass, nothing else matters

- [ ] Define `AdminContext` + `RuleWriter` + `LogTap` traits in `hc-web-admin`
- [ ] Implement traits in `hc-api`; wire `router()` call
- [ ] `[package.metadata.leptos]` with all required fields in `core/Cargo.toml`
- [ ] `rust-embed` wired for WASM bundle
- [ ] `hc-types` WASM features (`chrono/wasmbind`, `uuid/js`)
- [ ] `#[cfg(feature = "ssr")]` guards on all server-side types
- [ ] **Gate:** `cargo leptos build` passes; `cargo build` (no features) passes

### Phase 1 — Shell + Auth + SSR verification

- [ ] Login page: server fn with `ResponseOptions` cookie set; redirect
- [ ] `require_admin()` helper extracting session from `RequestParts`
- [ ] `leptos_routes_with_context` wiring for `AdminContext`
- [ ] `<AdminShell>` SSR: sidebar renders with data in `<Suspense>`; `<Outlet>`
- [ ] `SseContext` connected in `create_effect` (client-only); per-type signals
- [ ] **Gate:** curl `/admin/` returns complete HTML with nav and data — no skeleton

### Phase 2 — Tier 1 pages with version invalidation

- [ ] Devices table: Tier 3 single island, availability signal, `<Transition>`
- [ ] Rules list: `<Suspense>`, version signal, `ActionForm` enable/disable
- [ ] Scenes: `<Suspense>`, `ActionForm` activate
- [ ] Modes: toggle server fn → StateStore
- [ ] System / Plugins: `<Suspense>`, timed version refetch

### Phase 3 — Tier 3 live components

- [ ] Overview event feed: append-only reactive list from all SSE types
- [ ] Device detail: `(id, version)` resource in `<Transition>`, command panel
- [ ] Events page: live append + history query
- [ ] Logs page: `log_entry` signal → append-only list

### Phase 4 — Power features

- [ ] Rule editor: TOML textarea (ships fast); structured form (deferred)
- [ ] Areas CRUD
- [ ] Bulk rule operations
- [ ] `resync` event handler: increments all active version signals

---

## 15. Open Questions

1. **Rule editor**: raw TOML textarea is ~20 lines and ships in Phase 4. Structured
   form is a separate project. Start with textarea.

2. **pub_bus capacity (1024)**: high-frequency sensors fill the ring fast. If admin
   SSE consumers are slow, consider a filtered admin re-broadcast channel at lower
   capacity — isolated from the core ring so a slow admin browser tab cannot lag
   the rule engine's receiver.

3. **Mobile layout**: icon-strip sidebar for desktop-primary admin. Hamburger if
   mobile matters later.
