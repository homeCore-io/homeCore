## HomeCore Matter Plan (matter-rs)

### Goal
Add first-class Matter support in two roles:
1. Matter controller: commission and control native Matter devices from HomeCore.
2. Matter bridge: expose HomeCore devices to external Matter controllers.

This plan is designed around current HomeCore composition and plugin model.

### Why Plugin-First
Use one new Rust plugin process (or two plugins) instead of embedding Matter directly in the HomeCore binary.
1. Matches existing supervision/restart behavior.
2. Reuses stable MQTT registration and command contract.
3. Limits blast radius of stack-level Matter changes.
4. Allows phased delivery: controller first, bridge second.

Reference integration points:
1. [core/src/main.rs](core/src/main.rs)
2. [core/src/plugin_launcher.rs](core/src/plugin_launcher.rs)
3. [core/plugins/plugin-sdk-rs/src/lib.rs](core/plugins/plugin-sdk-rs/src/lib.rs)
4. [core/docs/openapi.yaml](core/docs/openapi.yaml)

### High-Level Architecture
New component proposal:
1. plugins/hc-matter (new Rust plugin using matter-rs)

Internal modules in hc-matter:
1. fabric_store: persistent fabric, node, key, session state.
2. controller: commissioner/client workflows and subscription engine.
3. bridge: local Matter bridge server exposing HomeCore-backed endpoints.
4. mapper: Matter cluster <-> HomeCore device type/state mapping.
5. discovery: DNS-SD and commissioning window handling.
6. telemetry: plugin status, diagnostics, and commissioning events.

### Phase 0: Discovery and Spike (1-2 weeks)
Deliverables:
1. Validate matter-rs version and feature set for both controller and bridge roles.
2. Confirm async runtime integration with Tokio and current Linux target.
3. Build a standalone spike that:
1. commissions one test bulb/switch,
2. reads OnOff + Level attributes,
3. toggles OnOff,
4. advertises one bridged endpoint.

Exit criteria:
1. Verified crate versions and transitive deps compile cleanly in workspace.
2. No runtime conflict with existing MQTT/event-loop behavior.

### Phase 1: Controller MVP (2-4 weeks)
Scope:
1. Commission Matter devices into a dedicated HomeCore fabric.
2. Discover endpoints/clusters and register HomeCore devices.
3. Subscribe to attribute reports and publish state updates.
4. Translate HomeCore commands back to Matter writes/commands.

Implementation details:
1. Create new plugin config: plugins/hc-matter/config/config.toml.
2. Add mapping table for first device classes:
1. OnOffLight,
2. DimmableLight,
3. OccupancySensor,
4. ContactSensor,
5. TemperatureMeasurement.
3. Register with typed device_type values via plugin-sdk-rs.
4. Keep HomeCore-facing state normalized and compact.

MVP API additions in HomeCore:
1. POST /api/v1/plugins/matter/commission
2. GET /api/v1/plugins/matter/nodes
3. POST /api/v1/plugins/matter/reinterview
4. DELETE /api/v1/plugins/matter/nodes/{id}

OpenAPI updates:
1. Add Matter operations and schemas in [core/docs/openapi.yaml](core/docs/openapi.yaml).

Exit criteria:
1. Pairing + control works for at least two certified Matter device models.
2. Device restart and plugin restart restore subscriptions and state.

### Phase 2: Bridge MVP (2-4 weeks)
Scope:
1. Expose selected HomeCore devices as bridged Matter endpoints.
2. Preserve HomeCore as source of truth for state.
3. Accept Matter writes/commands and route to HomeCore command paths.

Implementation details:
1. Bridge include-list in hc-matter config:
1. explicit device IDs,
2. optional device_type filter,
3. optional area filter.
2. Endpoint lifecycle:
1. stable endpoint IDs derived from HomeCore device IDs,
2. deterministic cluster composition by device_type/capabilities.
3. Change propagation:
1. HomeCore state -> Matter attribute update,
2. Matter command -> HomeCore device cmd topic/API.

Initial bridged types:
1. Light OnOff,
2. LevelControl,
3. Binary sensor (contact/occupancy),
4. Temperature measurement.

Exit criteria:
1. External ecosystem app can discover bridge and control mapped endpoints.
2. No duplicate/oscillating state loops under rapid writes.

### Phase 3: Hardening and Operations (1-2 weeks)
Security:
1. At-rest encryption for fabric secrets and operational credentials.
2. Strict file permissions on plugin data directory.
3. Optional passphrase or OS keyring hook for key unseal.

Reliability:
1. Resume subscriptions after plugin restart.
2. Backoff and retry policies for commissioner operations.
3. Event de-duplication for idempotent state updates.

Observability:
1. plugin_metrics events for matter:
1. commissioned_nodes,
2. bridged_endpoints,
3. subscription_reconnects,
4. command_latency_ms,
5. failed_commands.

### Data Model and Mapping Rules
Canonical mapping guidance:
1. Matter cluster attributes should map to stable HomeCore keys.
2. Preserve raw values when useful under namespaced keys.
3. Keep unit conversions centralized in mapper code.

Examples:
1. OnOff cluster -> on
2. LevelControl CurrentLevel -> brightness_pct
3. Temperature Measurement MeasuredValue -> temperature_c and temperature
4. OccupancySensing -> motion or occupied (normalized per profile)

### Config Additions
In plugins/hc-matter config (new):
1. role = controller|bridge|both
2. storage_dir
3. commissioner vendor_id/product_id
4. network interface selection
5. bridge include/exclude rules
6. commissioning window defaults

In HomeCore top-level config (optional later):
1. [[plugins]] entry for plugin.matter
2. optional auth/whitelist constraints for Matter admin endpoints

### Testing Strategy
Unit tests:
1. cluster-to-attribute mapping,
2. command translation,
3. endpoint stability and ID derivation.

Integration tests:
1. commission simulated node,
2. read/subscribe/write loop,
3. bridge endpoint discovery and command roundtrip.

End-to-end tests:
1. HomeCore + hc-matter + at least one real Matter device.
2. restart/recovery tests (HomeCore and plugin separately).

### Rollout Plan
1. Feature-flag plugin only in dev profile first.
2. Ship controller role before bridge role.
3. Add bridge for limited device classes, then expand.
4. Keep legacy ecosystems unchanged during Matter rollout.

### Risks and Mitigations
1. matter-rs API churn.
Mitigation: pin exact crate revision and isolate wrapper module.

2. State loop amplification between bridge and HomeCore.
Mitigation: correlation IDs + origin tags + write de-dup window.

3. Credential loss or corruption.
Mitigation: backup/export command + encrypted snapshots.

4. Multi-admin interoperability edge cases.
Mitigation: test matrix with at least two external controllers.

### Suggested First Implementation Ticket List
1. Create plugins/hc-matter skeleton with plugin-sdk-rs registration and heartbeat.
2. Add config schema and persistent storage layout.
3. Implement controller commissioning and node inventory API hooks.
4. Implement OnOff + Level mapping end to end.
5. Add OpenAPI endpoints and hc-tui admin controls for Matter plugin ops.
6. Implement bridge endpoint exporter for light devices.
7. Add metrics and recovery tests.

