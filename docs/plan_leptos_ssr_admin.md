# Leptos 0.8 SSR Admin Portal — Implementation Plan

> Created: 2026-04-02
> Status: Draft — pending review

## Architecture summary

`hc-web-admin` becomes a dual-target library: compiled to native x86-64 (SSR, server functions,
direct AppState access) and to `wasm32` (hydration, client interactivity). `cargo-leptos`
orchestrates both. `hc-api` mounts it at `/admin`. Zero REST API round-trips for data — server
functions call `AppState` directly.

---

## Critical blocker to resolve first: circular dependency

`hc-web-admin` needs `AppState` at compile time (for `use_context::<AppState>()`). But `hc-api`
already depends on `hc-web-admin`. **Circular.** Two clean options:

- **Option A (recommended):** Extract `AppState` into a new `crates/hc-app-state` crate. Both
  `hc-api` and `hc-web-admin` depend on it.
- **Option B:** Define an `AdminContext` trait in `hc-web-admin` with only the capabilities it
  needs (list devices, issue JWT, etc.); `AppState` implements it in `hc-api`. More boilerplate,
  no new crate.

Option A is cleaner. `AppState` migration into its own crate is a prerequisite for Phase 0.

---

## Section 1: Crate structure

**Single crate, two feature targets.** No new UI crate needed.

```
crates/hc-web-admin/src/
  lib.rs              — SSR router fn + hydrate WASM entry point (feature-gated)
  app.rs              — Root App component + leptos_router Routes (base="/admin")
  models.rs           — re-exports from hc-types (both features)
  auth.rs             — AuthState signal; localStorage guards under #[cfg(feature="hydrate")]
  pages/
    mod.rs
    login.rs          — ported from PoC
    devices.rs        — ported from PoC
    device_detail.rs  — ported from PoC
    areas.rs          — ported from PoC
    automations.rs    — new
    events.rs         — new
  components/
    mod.rs
    nav_shell.rs      — ported from PoC
  server/             — #[server] functions; cfg(feature="ssr") only
    mod.rs
    auth.rs
    devices.rs
    areas.rs
    automations.rs
    events.rs
```

`crate-type = ["cdylib", "rlib"]` — `cdylib` for WASM output; `rlib` so the SSR binary links it
as a library.

---

## Section 2: Cargo.toml changes

### `core/Cargo.toml` (workspace root) — additions

```toml
[workspace.dependencies]
leptos        = { version = "0.8.17" }
leptos_router = { version = "0.8.13" }
leptos_axum   = { version = "0.8.8" }   # requires axum 0.7 — matches workspace

[profile.wasm-release]
inherits  = "release"
opt-level = "z"
lto       = true

[package.metadata.leptos]
output-name     = "hc-web-admin"
site-root       = "target/site"
site-pkg-dir    = "pkg"
bin-package     = "homecore"
lib-package     = "hc-web-admin"
bin-features    = ["hc-web-admin/ssr"]
lib-features    = ["hydrate"]
lib-profile-release = "wasm-release"
```

`[package.metadata.leptos]` **must live in a bin crate** — the workspace root `[package]` section
satisfies this.

### `crates/hc-web-admin/Cargo.toml` — rewrite

```toml
[lib]
crate-type = ["cdylib", "rlib"]

[features]
ssr     = ["leptos/ssr", "leptos_router/ssr", "leptos_axum",
           "hc-types/wasm", "hc-app-state", "hc-auth", "axum"]
hydrate = ["leptos/hydrate", "leptos_router/csr",
           "hc-types/wasm", "wasm-bindgen", "web-sys",
           "console_error_panic_hook"]

[dependencies]
leptos        = { workspace = true }
leptos_router = { workspace = true }
leptos_axum   = { workspace = true, optional = true }
hc-types      = { path = "../hc-types", optional = true }
hc-app-state  = { path = "../hc-app-state", optional = true }  # new crate
hc-auth       = { path = "../hc-auth", optional = true }
axum          = { workspace = true, optional = true }
serde         = { workspace = true }
serde_json    = { workspace = true }
chrono        = { workspace = true, features = ["serde"] }
wasm-bindgen  = { version = "0.2", optional = true }
web-sys       = { version = "0.3", optional = true, features = ["Window", "WebSocket", ...] }
console_error_panic_hook = { version = "0.1", optional = true }
rust-embed    = { version = "8", optional = true }
```

### `crates/hc-types/Cargo.toml` — add wasm feature

```toml
[features]
wasm = ["chrono/wasmbind", "uuid/js"]
```

`hc-types` types are WASM-safe as-is (no `std::fs`, `Path`, or `Mutex`). Only the feature flags
on `chrono` and `uuid` need to be activated for WASM targets.

### `crates/hc-api/Cargo.toml`

Replace the direct `AppState` dependency with `hc-app-state`. `hc-api` still depends on
`hc-web-admin` for mounting the router.

---

## Section 3: AppState injection into server functions

`leptos_axum` provides `leptos_routes_with_context` — a closure runs per-request injecting values
into the Leptos context:

```rust
// hc-web-admin/src/lib.rs  (feature = "ssr")
pub fn router_ssr(state: AppState, leptos_opts: LeptosOptions) -> axum::Router {
    let routes = generate_route_list(App);
    axum::Router::new()
        .leptos_routes_with_context(
            &leptos_opts,
            routes,
            move || { provide_context(state.clone()); },
            App,
        )
        .fallback(leptos_axum::file_and_error_handler(shell))
        .with_state(leptos_opts)
}
```

Inside any `#[server]` function:

```rust
#[server(GetDevices, "/api")]
pub async fn get_devices() -> Result<Vec<DeviceState>, ServerFnError> {
    let state = use_context::<AppState>()
        .ok_or_else(|| ServerFnError::ServerError("no state".into()))?;
    let devices = state.store.list_devices().await?;
    Ok(devices)
}
```

The `#[server]` macro compiles the body to a native axum handler (SSR) and generates a
client-side fetch stub (hydrate). Page components call it identically on both sides:

```rust
// same call in a Leptos component — works on server and client
let devices = get_devices().await?;
```

---

## Section 4: Router integration in hc-api

`hc-api/src/lib.rs` router function, existing branch:

```rust
// Before:
if web_admin_enabled {
    app = app.nest("/admin", hc_web_admin::router())
}

// After:
if web_admin_enabled {
    let leptos_opts = build_leptos_options(); // reads site-root, pkg-dir from config
    app = app.nest("/admin", hc_web_admin::router_ssr(state.clone(), leptos_opts));
}
```

`leptos_router` base path must match the mount point — `/admin` — so all `<Route>` paths resolve
correctly:

```rust
// app.rs
<Router base="/admin">
  <Routes fallback=NotFound>
    <Route path=path!("/login")       view=LoginPage />
    <Route path=path!("/devices")     view=DevicesPage />
    <Route path=path!("/areas")       view=AreasPage />
    <Route path=path!("/automations") view=AutomationsPage />
    <Route path=path!("/events")      view=EventsPage />
  </Routes>
</Router>
```

Server function HTTP endpoints register relative to the mount point —
`/admin/api/GetDevices` — automatically.

---

## Section 5: Authentication inside server functions

The admin sub-router gets the same auth middleware as the REST API:

```rust
axum::Router::new()
    .leptos_routes_with_context(...)
    .layer(middleware::from_fn_with_state(
        state.clone(),
        require_admin_auth,  // adapted from hc-api's require_auth middleware
    ))
```

The middleware inserts a `ValidatedUser` extension. Server functions extract it:

```rust
#[server(Me, "/api")]
pub async fn me() -> Result<UserInfo, ServerFnError> {
    let user = leptos_axum::extract::<Extension<ValidatedUser>>().await
        .map_err(|_| ServerFnError::ServerError("Unauthorized".into()))?;
    Ok(UserInfo { id: user.id, role: user.role })
}
```

The login server function bypasses middleware (public route) and calls
`state.jwt.issue_token(...)` directly — no HTTP round-trip.

---

## Section 6: Static asset serving

**Development:** `ServeDir` pointed at `target/site/pkg/`:

```rust
.fallback_service(
    ServeDir::new("target/site/pkg").append_index_html_on_directories(false)
)
```

Assets served at `/admin/pkg/hc-web-admin_bg.wasm`, `/admin/pkg/hc-web-admin.js`.

**Production (single-binary):** `rust-embed` bakes the compiled `target/site/pkg/` into the
binary at compile time. Gate behind a cargo feature `embedded-assets` so plain `cargo build`
without cargo-leptos still works (WASM bundle doesn't need to be present at build time except
when this feature is active).

---

## Section 7: PoC migration guide

| PoC file | Migration |
|----------|-----------|
| `src/models.rs` | Move to `hc-web-admin/src/models.rs`; replace local structs with `pub use hc_types::...` |
| `src/api.rs` | **Delete entirely**; replaced by `server/*.rs` `#[server]` functions |
| `src/auth.rs` | Port; add `#[cfg(feature="hydrate")]` guards on localStorage/web_sys |
| `src/app.rs` | Port; add `base="/admin"` to `<Router>` |
| `src/pages/login.rs` | Port; replace `api::login()` call with `server::auth::login()` |
| `src/pages/devices.rs` | Port; replace `api::fetch_devices(token)` with `server::devices::get_devices()` (no token arg — middleware handles auth) |
| `src/pages/areas.rs` | Port; same pattern |
| WebSocket in `devices.rs` | Keep `web_sys::WebSocket` to `/api/v1/events/stream?token=<jwt>` — existing REST WS endpoint, unchanged |

Page components change their data calls; render logic is identical.

---

## Section 8: hc-types WASM compatibility

All `hc-types` types are WASM-safe (no `std::fs`, `Path`, `Mutex`, or OS types). Changes needed:

- `hc-types/Cargo.toml`: add `wasm` feature enabling `chrono/wasmbind` + `uuid/js`
- `chrono/wasmbind`: uses `js_sys::Date::now()` for `Utc::now()` in WASM — correct behavior
- `uuid/js`: enables `getrandom/js` for random UUID generation in WASM — low risk (IDs are
  server-assigned in practice)

Crates that will NOT compile to WASM (already SSR-only in the plan):
- `hc-state` (redb), `hc-core` (tokio), `hc-auth` (argon2), `axum`, `leptos_axum`

---

## Section 9: Build pipeline

| Command | What it does |
|---------|-------------|
| `cargo leptos watch` | Dev server: compiles native + WASM, hot-reloads on change |
| `cargo leptos build --release` | Production: outputs binary + `target/site/pkg/` |
| `cargo build` | No cargo-leptos needed; SSR/hydrate code is dead under no features; used for `cargo test`, `cargo clippy` |

Prerequisites: `rustup target add wasm32-unknown-unknown` + `cargo install cargo-leptos@0.3.5`

All `cargo leptos` commands run from `/home/john/RustroverProjects/homeCore/core/`.

---

## Phase Breakdown

### Phase 0 — Dependencies + scaffold (1–2 days)

**Goal:** Workspace builds with new dep graph; no admin functionality yet.

1. Extract `AppState` into `crates/hc-app-state` (circular dep resolution)
2. Add `hc-app-state` to workspace members
3. Rewrite `hc-web-admin/Cargo.toml` with feature flags
4. Add workspace leptos deps + `[package.metadata.leptos]` + `wasm-release` profile to `Cargo.toml`
5. Add `wasm` feature to `hc-types/Cargo.toml`
6. Replace `hc-web-admin/src/lib.rs` with feature-gated stubs (SSR returns old static HTML; hydrate is empty WASM entry)
7. Install `wasm32-unknown-unknown` target + cargo-leptos 0.3.5

**Gate:** `cargo build` green; `cargo leptos build` produces `target/site/pkg/` artifacts

---

### Phase 1 — Shell renders + WASM loads (2–3 days)

**Goal:** `/admin/` returns SSR HTML; WASM bundle loads in browser.

1. Implement `router_ssr()` with `leptos_routes_with_context` and AppState injection
2. Minimal `App` component with one placeholder route
3. Shell HTML template with correct `/admin/pkg/` script src
4. Update `hc-api` router to call `router_ssr()`
5. Configure `ServeDir` for WASM assets at `/admin/pkg/`
6. Set `leptos_options.site_pkg_dir = "/admin/pkg"` for correct script paths

**Gate:** `GET /admin/` returns SSR HTML; WASM loads in DevTools Network tab without 404;
hydration console message appears

---

### Phase 2 — Auth flow (2–3 days)

**Goal:** Login works end-to-end; server functions can validate identity.

1. Port `auth.rs` from PoC with `#[cfg(feature="hydrate")]` guards
2. Implement `#[server] login(username, password)` → `state.jwt.issue_token()`
3. Implement `#[server] me()` → extract `ValidatedUser` extension
4. Port `pages/login.rs` replacing gloo-net call with server function
5. Implement `AuthGuard` component
6. Apply auth middleware layer to admin router

**Gate:** Login → JWT → redirect works; wrong credentials show error; unauthenticated request
to `/admin/devices` redirects to login

---

### Phase 3 — Devices page (3–4 days)

**Goal:** Devices page renders SSR HTML with real data; hydrates for interactivity.

1. Port `models.rs` — replace local structs with `pub use hc_types::...`
2. Implement `#[server] get_devices()` → `state.store.list_devices()`
3. Implement `#[server] set_device_state(id, payload)` → publish command
4. Port `pages/devices.rs` and `pages/device_detail.rs`
5. Wire `web_sys::WebSocket` to `/api/v1/events/stream` under `#[cfg(feature="hydrate")]`

**Gate:** `curl /admin/devices` returns server-rendered device list HTML; browser hydrates;
toggle state changes persist; live WS updates visible

---

### Phase 4 — Remaining pages (2–3 days)

**Goal:** All sidebar routes show real data.

1. Areas: `#[server] get_areas/create_area/update_area/delete_area` + port `pages/areas.rs`
2. Automations: `#[server] list_automations()` via `state.rules_handle` + new `pages/automations.rs`
3. Events: `#[server] get_events()` via `state.event_log.recent()` + new `pages/events.rs`

**Gate:** All sidebar routes navigate and show real data

---

### Phase 5 — Single-binary production (1–2 days)

**Goal:** One binary, no `target/site/` needed at runtime.

1. Add `rust-embed` dependency under `embedded-assets` feature
2. Implement embedded asset handler (serves WASM/JS from binary memory)
3. Gate with `#[cfg(feature = "embedded-assets")]`; dev path stays `ServeDir`
4. Add `embedded-assets` to `bin-features` in `[package.metadata.leptos]`
5. Verify: copy binary to clean directory with no `target/`, confirm `/admin/` works

**Gate:** Fresh binary with no `target/` directory serves portal including WASM

---

## Key risks

| Risk | Mitigation |
|------|-----------|
| Server function URL prefix under nested `/admin` mount | Verify in Phase 1 that `leptos_axum` registers handlers at `/admin/api/*`; if not, configure `prefix` on the `#[server]` macro |
| `chrono/wasmbind` + `uuid/js` not yet in `hc-types` | Phase 0 adds `wasm` feature; test with `cargo build --target wasm32-unknown-unknown --features hydrate` early |
| AppState circular dep | Resolved by Phase 0 `hc-app-state` extraction — must be done before any Leptos code |
| `cargo-leptos` workspace support | cargo-leptos 0.3.5 supports workspace via `bin-package`/`lib-package`; test in Phase 0 before building UI |
| `leptos_shadcn_ui` not in workspace | Either add to workspace deps or replace with plain HTML; not a blocker |
