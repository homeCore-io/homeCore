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

### Implementation Backlog (Prioritized)
Effort scale:
1. S: 0.5-1 day
2. M: 2-4 days
3. L: 1-2 weeks

#### Phase A: Foundation and Spike
1. MAT-001: Create `hc-matter` plugin skeleton.
Owner: plugin/runtime
Estimate: M
Depends on: none
Acceptance:
1. Plugin starts under `[[plugins]]` and registers plugin status over MQTT.
2. Plugin supports graceful shutdown and restart under supervisor.

2. MAT-002: Add config schema and storage layout.
Owner: plugin/runtime
Estimate: M
Depends on: MAT-001
Acceptance:
1. Config supports `role`, `storage_dir`, commissioner IDs, and interface selection.
2. Data directory is created with strict permissions.

3. MAT-003: Matter-rs compatibility spike.
Owner: plugin/matter stack
Estimate: M
Depends on: MAT-001, MAT-002
Acceptance:
1. Commission one test node and read OnOff + Level.
2. Toggle OnOff command roundtrip from plugin.
3. Advertise one bridged endpoint in spike mode.

#### Phase B: Controller MVP
4. MAT-004: Implement fabric and credential persistence.
Owner: plugin/security
Estimate: M
Depends on: MAT-002, MAT-003
Acceptance:
1. Restart preserves commissioned nodes and session-relevant material.
2. Corrupt store handling fails safely with clear diagnostics.

5. MAT-005: Endpoint/cluster interview and internal node inventory.
Owner: plugin/controller
Estimate: M
Depends on: MAT-003, MAT-004
Acceptance:
1. Nodes are discoverable in plugin inventory with endpoint/cluster metadata.
2. Re-interview updates inventory without duplicate node records.

6. MAT-006: HomeCore admin API for commissioning lifecycle.
Owner: core/api
Estimate: M
Depends on: MAT-005
Acceptance:
1. Implement:
1. `POST /api/v1/plugins/matter/commission`
2. `GET /api/v1/plugins/matter/nodes`
3. `POST /api/v1/plugins/matter/reinterview`
4. `DELETE /api/v1/plugins/matter/nodes/{id}`
2. Operations validate auth and return typed error payloads.

7. MAT-007: OpenAPI contract updates.
Owner: core/api
Estimate: S
Depends on: MAT-006
Acceptance:
1. `core/docs/openapi.yaml` includes all new endpoints and schemas.
2. Contract checks pass in CI.

8. MAT-008: Mapping layer v1 (OnOffLight, DimmableLight).
Owner: plugin/mapper
Estimate: L
Depends on: MAT-005
Acceptance:
1. Attribute reports map to stable HomeCore keys (`on`, `brightness_pct`).
2. HomeCore commands map back to Matter writes/commands.

9. MAT-009: Mapping layer v2 (Contact, Occupancy, Temperature).
Owner: plugin/mapper
Estimate: L
Depends on: MAT-008
Acceptance:
1. Sensor attributes publish normalized state keys.
2. Unit handling is centralized and deterministic.

10. MAT-010: Subscription engine and reconnect behavior.
Owner: plugin/controller
Estimate: M
Depends on: MAT-008
Acceptance:
1. Attribute subscriptions auto-resume after transient disconnect.
2. De-duplication prevents repeated state storms.

#### Phase C: Bridge MVP
11. MAT-011: Bridge include-list and endpoint ID strategy.
Owner: plugin/bridge
Estimate: M
Depends on: MAT-002
Acceptance:
1. Include/exclude rules support explicit IDs and optional filters.
2. Endpoint IDs are deterministic across restart.

12. MAT-012: Bridge exporter for light endpoints.
Owner: plugin/bridge
Estimate: L
Depends on: MAT-011, MAT-008
Acceptance:
1. External controller can discover and control bridged light endpoints.
2. No duplicate endpoint creation across plugin restart.

13. MAT-013: Bridge exporter for binary and temperature sensors.
Owner: plugin/bridge
Estimate: L
Depends on: MAT-012, MAT-009
Acceptance:
1. Contact, occupancy, and temperature are visible in external controller apps.
2. Attribute updates reflect HomeCore source-of-truth state.

14. MAT-014: Loop prevention and origin tagging.
Owner: plugin/bridge
Estimate: M
Depends on: MAT-012
Acceptance:
1. Bridge-origin writes do not re-emit as duplicate upstream commands.
2. Correlation/origin metadata is present in debug traces.

#### Phase D: Hardening and Operations
15. MAT-015: Metrics and diagnostics.
Owner: plugin/ops
Estimate: M
Depends on: MAT-010, MAT-014
Acceptance:
1. Emit metrics: `commissioned_nodes`, `bridged_endpoints`, `subscription_reconnects`, `command_latency_ms`, `failed_commands`.
2. Health/status output includes actionable error states.

16. MAT-016: Security hardening for credential material.
Owner: plugin/security
Estimate: M
Depends on: MAT-004
Acceptance:
1. Secrets encrypted at rest or protected via pluggable key provider.
2. Backup/export flow exists and is documented.

17. MAT-017: Integration and restart/recovery test matrix.
Owner: core+plugin QA
Estimate: L
Depends on: MAT-010, MAT-013, MAT-015
Acceptance:
1. Automated scenarios cover controller and bridge restarts.
2. At least two external controller ecosystems are validated.

18. MAT-018: hc-tui/admin UX for Matter operations.
Owner: client/tui
Estimate: M
Depends on: MAT-006, MAT-007
Acceptance:
1. Basic commission/list/reinterview/remove flows available from admin UI.
2. Operation errors are surfaced clearly to operators.

### Dependency-Ordered Execution Plan
1. MAT-001 -> MAT-002 -> MAT-003
2. MAT-004 -> MAT-005 -> MAT-006 -> MAT-007
3. MAT-008 -> MAT-009 -> MAT-010
4. MAT-011 -> MAT-012 -> MAT-013 -> MAT-014
5. MAT-015 + MAT-016 + MAT-018
6. MAT-017 (final gate before broad rollout)

### Suggested Sprint Cut
1. Sprint 1: MAT-001 to MAT-005
2. Sprint 2: MAT-006 to MAT-010
3. Sprint 3: MAT-011 to MAT-014
4. Sprint 4: MAT-015 to MAT-018

