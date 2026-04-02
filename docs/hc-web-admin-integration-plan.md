# HomeCore `hc-web-admin` Integration Plan

Status: Draft, living document
Last Updated: 2026-04-02
Scope: Fold `hc-web-leptos` into the `core` workspace as an internal HomeCore admin UI without reducing external client or API functionality and without degrading core performance or reliability.

## Intent

HomeCore should gain an internal Rust-native administrative web surface that is built, shipped, and versioned with `core`.

This is not a plan to replace the external HTTP API or to turn the browser into a privileged bypass around HomeCore internals. The goal is to remove the current repo and process split between `core` and `hc-web-leptos`, while preserving the existing external integration model for:

- `hc-tui`
- `hc-mcp`
- plugins
- automation tooling
- future third-party API consumers

The integrated web app should be treated as a first-party operator UI built on top of HomeCore services, not as a reason to weaken separation, observability, or runtime safety.

## Non-Negotiable Invariants

The migration must preserve these constraints:

1. External client functionality must not be reduced.
2. `/api/v1` behavior must not be reduced for existing consumers.
3. Web integration must not materially reduce `homecore` startup reliability, shutdown reliability, steady-state latency, or memory stability.
4. A broken admin UI build must not block the core automation runtime from functioning in development or production.
5. The rule engine, state bridge, MQTT broker, scheduler, and plugin supervision remain the priority runtime paths.
6. The integrated web layer must be optional at the crate and feature level so core runtime can still be built and tested without a browser target when needed.

## Current State

Today:

- `clients/hc-web-leptos` is a standalone CSR Leptos app.
- It uses duplicated client-side models that mirror `hc-types`.
- It uses a handwritten REST client layer against `/api/v1`.
- It uses the existing HomeCore WebSocket event stream for live updates.
- It is developed and bundled separately via Trunk.
- It is operationally external to the `core` workspace.

This gives fast UI iteration, but it creates avoidable duplication and drift:

- duplicated types
- duplicated auth handling
- duplicated query shapes
- separate build and release path
- no SSR or server-side access to HomeCore state/services

## Target Architecture

The target is a new internal workspace crate:

- `core/crates/hc-web-admin`

Recommended role split:

- `hc-api`
  - stable machine-facing HTTP and WebSocket API under `/api/v1`
  - remains available for `hc-tui`, `hc-mcp`, and future consumers
- `hc-web-admin`
  - Leptos admin UI
  - server-side rendering and hydration support
  - admin page routes, components, server functions, admin query adapters
- `hc-types`
  - canonical shared domain and wire types
- optional future crate: `hc-app-services`
  - reusable application queries and mutations shared by `hc-api` and `hc-web-admin`

Do not merge the UI directly into `hc-api` as page code. `hc-api` should remain transport-oriented and maintainable. `hc-web-admin` can be mounted from the top-level axum router without collapsing responsibilities into one crate.

## Primary Design Decisions

### 1. Keep `/api/v1` intact

The integrated admin UI may stop using some REST endpoints internally over time, but the endpoints themselves remain supported and tested.

### 2. Prefer same-origin mounting

Serve the admin UI from the same `homecore` process and HTTP listener, ideally at `/` or `/admin`, while keeping `/api/v1` unchanged.

### 3. Use shared Rust types, not mirrored client models

`hc-web-admin` should import `hc-types` directly instead of maintaining copies of device, area, history, and event types.

### 4. Use server-side access for initial reads and privileged mutations

Initial page loads and admin mutations should move toward Leptos server functions or shared application services. This removes redundant REST round-trips and duplicated JSON handling for first-party UI code.

### 5. Keep live updates on the existing event model first

Do not redesign the event bus contract as part of phase one. Reuse the existing HomeCore event stream, then tighten admin-specific subscriptions after parity is established.

### 6. Move browser auth to secure cookie-backed sessions

The current local-storage JWT model is acceptable for the standalone app, but the integrated admin UI should move to httpOnly cookie-backed auth for browser flows. Bearer token support remains for external clients.

## Performance and Reliability Guardrails

These are hard acceptance gates, not aspirational goals.

### Runtime isolation

- Web admin rendering must not run on hot paths for MQTT ingestion, rule evaluation, state bridging, or plugin supervision.
- Web admin background tasks must be bounded and observable.
- SSR work must use normal HTTP request lifecycle boundaries and must not subscribe broadly to the event bus per request.

### Resource control

- No unbounded per-client caches.
- No per-request cloning of large device inventories unless explicitly measured and accepted.
- Asset serving must use static files or baked assets with predictable memory behavior.

### Failure containment

- If web asset loading fails, `/api/v1` and automation runtime must still function.
- If SSR fails for a route, it must fail as an HTTP concern, not poison shared runtime state.
- Build-time web issues must not make `cargo test -p hc-core` or core-only dev loops depend on WASM tooling by default.

### Startup and shutdown

- Web-admin initialization must not delay plugin bring-up or internal MQTT readiness.
- Shutdown must treat web-admin task drain as bounded and non-blocking.
- Existing shutdown controls stay authoritative.

### Observability

- Add metrics for admin request latency, SSR render latency, server function latency, and active web socket counts.
- Add explicit tracing targets for `hc_web_admin` and any shared admin query layer.

## Compatibility Strategy

The compatibility model is additive:

- existing `/api/v1` routes remain
- existing event WebSocket remains
- `hc-tui` remains a first-class concrete API consumer
- auth for non-browser clients remains bearer-token capable

The integrated UI may gradually switch from REST calls to direct server functions internally, but only after parity is established and regression coverage exists for the external API.

## Rollout Plan

## Phase 0: Baseline and Guardrails

Goal:
Establish objective performance, reliability, and compatibility baselines before any integration work.

Deliverables:

- inventory current `hc-web-leptos` routes and feature coverage
- inventory `/api/v1` routes used by `hc-web-leptos`, `hc-tui`, and `hc-mcp`
- baseline measurements for:
  - `homecore` startup time
  - memory at idle
  - event stream latency
  - shutdown time
  - device page initial load time
- define build matrix:
  - core-only
  - core plus web-admin
  - browser hydrate build

Success criteria:

- clear list of compatibility contracts
- clear runtime baseline to compare against later phases

## Phase 1: Internalize the UI as a Workspace Crate

Goal:
Move the app into the `core` workspace without changing user-visible behavior.

Deliverables:

- create `core/crates/hc-web-admin`
- move `hc-web-leptos` source into the crate with minimal functional changes
- set up crate features for:
  - `ssr`
  - `hydrate`
  - optional `csr` only for transitional local dev if needed
- keep a separate asset/build path for the browser bundle

Rules:

- no API changes yet
- no auth changes yet
- no route changes yet

Success criteria:

- the web-admin crate builds inside `core`
- the old standalone behavior can still be reproduced during transition
- `cargo check` for core runtime remains straightforward

Rollback:

- the old external client remains usable until later phases complete

## Phase 2: Shared Types and Shared Query Boundaries

Goal:
Remove duplicated domain models and establish a clean first-party service boundary.

Deliverables:

- replace mirrored UI types with `hc-types` imports where possible
- identify gaps in `hc-types` and add only genuinely shared types there
- create a shared admin query/mutation layer if needed for:
  - devices
  - areas
  - dashboards
  - rules
  - scenes
  - calendars

Rules:

- do not move browser-specific view helpers into `hc-types`
- do not bloat `hc-api` handlers with UI-only shaping code

Success criteria:

- `hc-web-admin` no longer mirrors core device and event models
- API and UI use the same source-of-truth types where appropriate

## Phase 3: Mount `hc-web-admin` from HomeCore

Goal:
Serve the admin UI from the main HomeCore HTTP server.

Deliverables:

- add top-level axum mounting for admin routes
- serve static assets from HomeCore
- keep `/api/v1` namespaced and unchanged
- define admin route namespace:
  - preferred: `/`
  - conservative option: `/admin`

Recommendation:

- use `/admin` first for safer rollout
- optionally move to `/` later after deployment confidence

Rules:

- mounting web routes must not interfere with `/api/v1`, `/metrics`, `/health`, `/logs/stream`, or `/events/stream`

Success criteria:

- single-process same-origin UI works
- no Trunk proxy required for integrated runtime
- machine API remains unchanged

## Phase 4: Browser Auth Hardening

Goal:
Shift first-party browser auth from local storage JWT to secure session cookies.

Deliverables:

- browser login flow using httpOnly cookie-backed auth
- CSRF posture defined for mutating operations
- preserve bearer-token login and auth behavior for non-browser clients

Rules:

- do not break `hc-tui` login or automation clients
- do not remove whitelist behavior without an explicit broader auth project

Success criteria:

- browser UI no longer depends on local storage token as primary auth mechanism
- external clients continue to use JWT/bearer paths

## Phase 5: SSR and First-Party Server Functions

Goal:
Use direct Rust integration for first-party admin reads and writes while preserving external API support.

Deliverables:

- SSR for shell and initial page loads
- server functions or shared services for:
  - devices list
  - device detail
  - metadata updates
  - device commands
  - areas
- hydrate on the client for live interaction after initial render

Rules:

- do not duplicate business logic between server functions and REST handlers
- shared mutation logic should live below both entry points

Success criteria:

- initial page render no longer depends on browser-only bootstrap fetches
- first-party admin mutations can use direct server-side Rust integration
- external `/api/v1` remains supported

## Phase 6: Live Update Tightening

Goal:
Keep the admin UI live without excessive event fan-out or client-side decoding complexity.

Deliverables:

- keep compatibility with existing event stream first
- add narrower admin subscriptions if beneficial
- evaluate route-level subscriptions and typed admin stream adapters

Rules:

- do not redesign the global event model until real usage data justifies it

Success criteria:

- device and dashboard pages remain live
- subscription cost stays bounded

## Phase 7: Expand Admin Coverage

Goal:
Bring the rest of the admin surface into the integrated model after devices are stable.

Modules:

- dashboards
- automations
- scenes
- events
- modes
- plugin/system admin tools

Rules:

- use the devices module as the reference implementation pattern
- each module must meet the same compatibility and runtime guardrails

## Phase 8: Decommission the Standalone Client

Goal:
Retire `clients/hc-web-leptos` only after parity is achieved and operational confidence is high.

Exit criteria:

- admin route coverage reaches agreed parity
- deployment path is documented
- build/release path is stable
- external API compatibility tests pass

Only after that:

- archive or remove the standalone client repo/path
- update workspace documentation and developer workflows

## Testing and Validation Matrix

Every phase should be checked against:

### API compatibility

- `hc-tui` login, data fetch, event stream, and mutation flows
- `hc-mcp` core workflows
- existing curl and OpenAPI-backed behavior

### Runtime reliability

- startup with plugins enabled
- restart loops
- shutdown under active WebSocket clients
- degraded plugin scenarios

### Performance

- startup delta vs baseline
- steady-state memory delta vs baseline
- SSR latency for key pages
- event stream latency under load
- no measurable regression in state-bridge and rule-engine responsiveness

### Build reliability

- `cargo check --workspace`
- core-only targeted checks
- web-admin build path
- CI split so web build failures are visible but do not hide core runtime regressions

## Open Questions

These need explicit decisions before implementation gets deep:

1. Should the integrated admin surface mount at `/admin` first, then move to `/` later?
2. Should SSR be mandatory for the admin shell, or optional behind a feature during rollout?
3. Do we want a dedicated shared application service crate, or is a smaller internal module split enough initially?
4. What release artifact format should own web assets:
   - embedded into the binary
   - sidecar static directory
   - hybrid
5. Do we preserve the current event-stream contract for the browser indefinitely, or add a narrower first-party stream once integrated?

## Recommended Immediate Next Steps

1. Create `core/crates/hc-web-admin` as a workspace member.
2. Make it build with shared `hc-types` instead of mirrored models.
3. Add a non-default feature-gated mount in the HomeCore router for integrated admin serving.
4. Establish baseline startup, memory, and shutdown metrics before enabling it by default.
5. Migrate the devices list and device detail pages first.

## Change Log

- 2026-04-02: Initial draft created as living plan.
