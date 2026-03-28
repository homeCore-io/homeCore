# HomeCore Web UI — Design Plan (`hc-web`)

> **Revision:** 2026-03-28
> **Reference systems:** Home Assistant (HA) Lovelace/Sections UI, HomeSeer 3 Status/Device panels

---

## 1. Goals and Design Philosophy

### What the redesign solves

| Current pain point | Fix |
|---|---|
| Device IDs everywhere ("light_desk_lamp") | Resolved names throughout — "Desk Lamp" |
| One static dashboard | Multiple user-configurable dashboards |
| Scene cards — wastes vertical space, no status | Compact list with active/inactive status |
| Rule editor is raw JSON-like fields | Visual step-by-step builder with plain-language descriptions |
| No group/context for dashboard items | Sections, areas, tags as first-class organizers |

### Design pillars

1. **Entity names, not IDs** — Device IDs are an implementation detail. Every picker, label, trigger description, and condition summary resolves IDs to human names from the device registry.
2. **Progressive disclosure** — Simple things are easy (toggle a light), powerful things are possible (nested RepeatWhile with WaitForEvent) without cluttering the default view.
3. **Real-time first** — Dashboard tiles, device states, and rule histories update via WebSocket without a page reload.
4. **HA-inspired for dashboards, HomeSeer-inspired for lists** — HA's tile grid is the gold standard for glanceable control. HomeSeer's dense tabular lists are the best way to work with many entities at once.
5. **Mobile-responsive** — Two-column grid on tablet, single-column stack on phone, always usable.

---

## 2. Global UI Changes

### 2.1 Name Resolution Layer

A `DeviceNameResolver` utility class is maintained globally. It maps `device_id → display_name`, built from the device list on startup and updated on WebSocket events.

```dart
// Injected via Riverpod
final deviceNameResolverProvider = Provider<DeviceNameResolver>((ref) {
  final devices = ref.watch(devicesProvider).valueOrNull ?? [];
  return DeviceNameResolver(devices);
});

class DeviceNameResolver {
  final Map<String, String> _map;
  DeviceNameResolver(List<DeviceState> devices)
      : _map = {for (final d in devices) d.deviceId: d.name};
  String resolve(String deviceId) => _map[deviceId] ?? deviceId;
}
```

**Everywhere** a `device_id` string is displayed — rule trigger summaries, condition labels, action descriptions, scene device lists, event log entries, fire history — it passes through `resolver.resolve(id)`.

A parallel `ModeNameResolver` maps mode device IDs (`mode_night`) to user-facing names ("Night").

### 2.2 Area-Aware Display

Every device has an `area` field. The UI groups by area in:
- Devices page
- Dashboard "Entities" cards
- Scene device picker
- Rule trigger/condition device pickers

Area names are fetched once from `/api/v1/areas` and cached.

### 2.3 Theme and Layout

- **Seed color:** Keep indigo `#3F51B5` but add a user-settable accent preference stored in SharedPreferences.
- **Typography:** Slightly tighter line-heights for dense list views (HomeSeer feel). Larger touch targets for dashboard tiles (HA feel).
- **AppBar:** Slimmer; title is the dashboard name. Breadcrumb for drill-downs (Device Detail, Rule Editor).
- **Sidebar (wide screens):** NavigationRail becomes a full sidebar drawer with section headers (Main / Automations / System). Icon + label visible at all times above 900px.

---

## 3. Multiple Dashboard System

### 3.1 Concept

Dashboards are user-created tabbed views, each containing an ordered list of **sections**, each section containing **cards**. This mirrors HA's 2024 Sections layout.

- A **default dashboard** ("Home") always exists and cannot be deleted.
- Additional dashboards ("Lighting", "Security", "Energy") can be created.
- Dashboard config is stored as JSON in the browser's localStorage (key: `hc_dashboards`), with an optional sync endpoint on the server (`GET/PUT /api/v1/ui/dashboards` — to be added to backend).
- A dashboard tab bar appears at the top of the dashboard shell.

```
┌─────────────────────────────────────────────────────────────┐
│  [🏠 Home] [💡 Lighting] [🔒 Security] [+]                   │  ← Dashboard tabs
├─────────────────────────────────────────────────────────────┤
│  Section: Living Room                                        │
│  ┌──────┐ ┌──────┐ ┌──────┐ ┌──────────────────────────┐   │
│  │ Tile │ │ Tile │ │ Tile │ │      Wide Tile           │   │
│  └──────┘ └──────┘ └──────┘ └──────────────────────────┘   │
│                                                              │
│  Section: Security                                           │
│  ┌────────────────────────┐ ┌────────────────────────┐      │
│  │  Entities Card         │ │  Glance Card           │      │
│  └────────────────────────┘ └────────────────────────┘      │
└─────────────────────────────────────────────────────────────┘
```

### 3.2 Dashboard Data Model

```dart
class Dashboard {
  final String id;         // uuid
  final String name;       // "Home"
  final String icon;       // "home" (Material Icons name)
  final int order;         // tab position
  final List<DashSection> sections;
}

class DashSection {
  final String title;       // "Living Room" — optional
  final int columns;        // 2, 3, or 4 (default 3)
  final List<DashCard> cards;
}

class DashCard {
  final String type;        // see card type catalog below
  final int colSpan;        // 1..columns (default 1)
  final Map<String, dynamic> config; // card-specific config
}
```

### 3.3 Card Type Catalog

#### `entity` — Single device tile
Matches HA's "Tile" card. Full-width tile with device name, state badge, and a primary action.

```
┌─────────────────────────────┐
│  💡 Desk Lamp               │  ← name resolved from device_id
│      On — 75%  ●────────●  │  ← state + inline slider (if dimmer)
└─────────────────────────────┘
```

Config: `{ device_id, show_name, show_state, tap_action: "toggle|more-info|navigate" }`

Primary actions by device type:
- **Light (bool `on`):** tap = toggle
- **Dimmer (num `brightness`):** tap = toggle; long-press = brightness popup
- **Lock:** tap = lock/unlock with confirmation
- **Switch/Sensor:** tap = toggle (switches) or more-info (sensors)

#### `entities` — Multi-device compact list
HomeSeer-inspired dense list. Best for a room full of similar devices.

```
┌──────────────────────────────────────────┐
│  Living Room                             │
│  ─────────────────────────────────────  │
│  💡 Ceiling Light        On    [toggle]  │
│  💡 Floor Lamp           Off   [toggle]  │
│  🌡 Thermostat           72°F            │
│  🚪 Front Door           Closed          │
└──────────────────────────────────────────┘
```

Config: `{ title, entity_ids: [...], show_header_toggle }`

#### `glance` — Compact multi-entity icons
HA's Glance card. Shows icon + state for many entities in a grid.

```
┌──────────────────────────────────────────┐
│  💡  🎵  🌡  🚪  🔒  🪟               │
│  On  Off  72° Cld  Lck  Cld             │
└──────────────────────────────────────────┘
```

Config: `{ entity_ids: [...], columns }`

#### `button` — Scene or automation trigger
Big tappable button. Shows scene/rule name and last activated time.

```
┌──────────────────────────────┐
│          ▶ Movie Time        │  ← scene name
│       Last: 3 hours ago      │
└──────────────────────────────┘
```

Config: `{ label, icon, scene_id | rule_id, show_last_triggered }`

#### `mode` — Mode toggle tile
Shows current mode state with one-tap toggle (manual modes) or current solar time window (solar modes).

```
┌──────────────────────────────┐
│  🌙 Night Mode    ON   [●]   │
│  Sunset +0min → Sunrise +0min│
└──────────────────────────────┘
```

Config: `{ mode_id }`

#### `history-graph` — Sparkline chart
Attribute value over time. Best for temperature, humidity, power.

```
┌──────────────────────────────┐
│  🌡 Living Room Temp         │
│     ╭───╮ ╭──╮              │
│  ───╯   ╰─╯  ╰───  72°F now │
│  past 24h                   │
└──────────────────────────────┘
```

Config: `{ device_id, attribute, hours, color }`

#### `status` — System health summary
Rule fire counts, device online %, plugin status.

Config: `{}` (no config needed)

#### `markdown` — Free text / heading
Section title or notes. Plain markdown rendered in a card.

Config: `{ content: "## Good Morning\n..." }`

### 3.4 Dashboard Editor

Accessed via a pencil icon in the top-right when on any dashboard.

- **Edit mode** overlays each card with drag handles and a "⋮" menu (Edit config / Duplicate / Delete / Move to section).
- **Add card** button at the end of each section opens a "Pick card type" bottom sheet with previews.
- **Add section** button at the bottom of the dashboard.
- **Reorder sections** via drag handles.
- **Dashboard settings** (name, icon, column count per section) via Settings icon in edit mode.

All edits are saved live to localStorage with an auto-save debounce of 800ms.

### 3.5 Default "Home" Dashboard

Pre-populated on first load based on actual API data:
- **Section "Now":** Mode glance card + system status card
- **Section per Area:** "entities" card for each area that has devices
- **Section "Quick Actions":** Button cards for any existing scenes

---

## 4. Navigation Redesign

### 4.1 Sidebar Structure (wide layout)

```
HomeCore                       ← logo + app name
────────────────────
🏠 Dashboards
   Home
   Lighting
   Security
   [+ New Dashboard]
────────────────────
🏠 Devices
🎭 Scenes
⚡ Automations
🌙 Modes
────────────────────
🔧 Admin ▾
   Areas
   Users
   Plugins
   System
   Logs
────────────────────
📡 Events
```

### 4.2 Bottom Nav (mobile)

Five primary destinations: Dashboard · Devices · Scenes · Automations · More (opens drawer for Modes/Admin/Events).

---

## 5. Scenes Redesign

### 5.1 Why List, Not Cards

Cards communicate "this is an object to manage." A scene is fundamentally an **action** — you fire it. A list with an inline button is the right metaphor: HomeSeer's device status table, not a Pinterest board.

### 5.2 List Layout

```
Scenes                                         [+ New Scene]  [🔍]
──────────────────────────────────────────────────────────────────
[Filter: All ▾]  [Sort: Name ▾]  [Tag: ▾]

Name                    Devices  Status     Last Activated    Actions
──────────────────────────────────────────────────────────────────
🎬 Movie Night          6        ─          2 hours ago       [▶] [⋮]
☀  Good Morning        8        ─          Yesterday 7:02am  [▶] [⋮]
🌙 Bedtime             5        ─          Yesterday 10:18pm [▶] [⋮]
🎵 Dinner Party        4        ─          3 days ago        [▶] [⋮]
── Plugin scenes ──────────────────────────────────────────────────
Lutron: Entryway       —        (plugin)   —                 [▶]
Hue: Relax             —        (plugin)   —                 [▶]
```

**Column definitions:**

| Column | Description |
|---|---|
| **Name** | Resolved name; tap to open editor (native scenes only) |
| **Devices** | Count of device states captured; click to expand inline |
| **Status** | "Active" (badge, green) if a fire happened within ~30 seconds of last check; "─" otherwise. Updated via `scene_activated` WebSocket event |
| **Last Activated** | Relative time from `scene_activated` event log; "Never" if no history |
| **Actions** | `▶` = activate (with visual feedback spinner); `⋮` = Edit / Duplicate / Delete |

### 5.3 Inline Device Expansion

Clicking the device count cell expands a sub-row showing which devices are captured and their stored states:

```
  ↳  💡 Desk Lamp — brightness: 80, on: true
     💡 Floor Lamp — on: false
     🔊 Sonos — volume: 30
```

### 5.4 Scene Editor

**Two-panel layout:**

Left panel: **Device picker** (grouped by area, with name + current state)
Right panel: **Captured states** (target values for each device with inline controls)

```
┌──────────────────────────────┬────────────────────────────────┐
│  Add devices                 │  Scene: Movie Night            │
│  ─────────────────────────   │  ────────────────────────────  │
│  Living Room                 │  💡 Desk Lamp         80%  🔆  │
│    💡 Ceiling Light    [+]   │      On ●  Brightness ──●──   │
│    💡 Floor Lamp       [+]   │                                │
│    🔊 Sonos            [+]   │  🔊 Sonos              30%  🔊 │
│  Bedroom                     │      Volume ────●──────        │
│    💡 Bedside Lamp     [+]   │                                │
│                              │  [Save]  [Cancel]              │
└──────────────────────────────┴────────────────────────────────┘
```

- **"Capture current"** button snapshots all device states in a selected area into the right panel
- Removing a device from right panel clicks the `−` on that row
- Scene name is inline-editable at the top

---

## 6. Automation / Rule Editor

### 6.1 Design Philosophy

The rule editor is the most complex screen. The goal:

- **HA-style** for common cases: visual, step-by-step, plain English descriptions
- **Power accessible** but not forced: advanced action types (RepeatWhile, WaitForEvent, FadeDevice, nested Conditional) are available but tucked behind a "More action types" panel
- **Bidirectional**: every rule can be viewed/edited visually AND as raw TOML (toggle switch in the header) — advanced users keep their workflow

### 6.2 Editor Layout

```
[← Back]  Edit Automation: "Turn on lights at sunset"       [TOML ↔ Visual]

─── Trigger ──────────────────────────────────────────────────────────────────
  [Sun Event: Sunset + 0 min ▾]                             [Change trigger]

─── Run When (Conditions) ───────────────────────────────── [+ Add condition]
  Logic: [All (AND) ▾]

  ① Mode is Night  ✓ pass                                       [✕]
  ② Desk Lamp is Off  ✓ pass                                     [✕]

─── Then Do (Actions) ─────────────────────────────────────── [+ Add action]

  ① Turn On  Desk Lamp                                       [⋮] [✕]
  ② Set      Desk Lamp  brightness → 80                     [⋮] [✕]
  ③ Wait     30 seconds                                     [⋮] [✕]
  ④ If  Night Mode is On  ▼                                 [⋮] [✕]
      ① Dim  Desk Lamp  brightness → 40
     Else
      ① Set  Desk Lamp  brightness → 80

─── Settings ─────────────────────────────────────────────────────────────────
  Name: [Turn on lights at sunset            ]
  Priority: [50   ]   Cooldown: [none ▾]    Run mode: [Parallel ▾]
  Tags: [evening] [lighting] [+]
  ☐ Log events   ☐ Log triggers   ☐ Log actions

                                               [Test Run]  [Save]  [Cancel]
```

### 6.3 Trigger Picker

The trigger picker is a two-step bottom sheet / dialog:

**Step 1: Trigger category**

```
What starts this rule?

┌────────────────┐  ┌─────────────────┐  ┌──────────────────┐
│  🔌 Device     │  │  ⏰ Time        │  │  ☀  Solar        │
│  State Change  │  │  of Day         │  │  Event           │
└────────────────┘  └─────────────────┘  └──────────────────┘
┌────────────────┐  ┌─────────────────┐  ┌──────────────────┐
│  📅 Calendar   │  │  🌙 Mode        │  │  🔔 Custom       │
│  Event         │  │  Changed        │  │  Event           │
└────────────────┘  └─────────────────┘  └──────────────────┘
┌────────────────┐  ┌─────────────────┐  ┌──────────────────┐
│  📡 MQTT       │  │  🔗 Webhook     │  │  🔁 Cron /       │
│  Message       │  │  Received       │  │  Periodic        │
└────────────────┘  └─────────────────┘  └──────────────────┘
┌────────────────┐  ┌─────────────────┐  ┌──────────────────┐
│  🔑 Hub        │  │  🔘 Button      │  │  🔢 Numeric      │
│  Variable      │  │  Event          │  │  Threshold       │
└────────────────┘  └─────────────────┘  └──────────────────┘
┌────────────────┐  ┌─────────────────┐
│  🚀 System     │  │  🎛  Manual     │
│  Started       │  │  Trigger        │
└────────────────┘  └─────────────────┘
```

**Step 2: Trigger configuration form** (context-sensitive fields)

Each trigger type shows a compact form with named fields and dropdowns. All entity pickers show **names** in the dropdown, store the `device_id` in the model.

| Trigger Type | Form Fields |
|---|---|
| **Device State Change** | Device picker (name→id), Attribute (dropdown of device's known attributes), From/To value (optional), "and stays for" duration |
| **Time of Day** | Time picker, Weekday checkboxes (all, weekdays, weekends, custom) |
| **Solar Event** | Event dropdown (Sunrise/Sunset/Noon/Civil Dawn/Dusk), Offset slider (±120 min) |
| **Mode Changed** | Mode picker (name), To state (On / Off / Either) |
| **Calendar Event** | Calendar picker (from loaded ICS files), Title filter (optional), Offset minutes |
| **Hub Variable Changed** | Variable name (autocomplete from known vars), optional filter |
| **MQTT Message** | Topic pattern, payload match (optional), JSON path match (optional) |
| **Webhook** | Path (shows full URL preview: `https://server/api/v1/webhooks/{path}`) |
| **Cron** | Expression builder with visual preview ("Every day at 7:30am") |
| **Periodic** | Every N [Minutes/Hours/Days/Weeks] |
| **Button Event** | Device picker, Button number (optional), Event type |
| **Numeric Threshold** | Device picker, Attribute, Operator, Value, For-duration |
| **Custom Event** | Event type string |
| **System Started** | (no fields) |
| **Manual** | (no fields) |

**Trigger summary chip** (shown in the rule list and at the top of the editor):

> `Sun event: Sunset +0 min`
> `Device: Desk Lamp → On`
> `Mode changed: Night → On`
> `Every day at 07:30`

### 6.4 Condition Builder

Conditions appear in an ordered list below the trigger. The logic mode toggles between **All (AND)** and **Any (OR)** — this maps to the top-level condition structure.

For compound logic (AND group inside OR, etc.), an "Add group" option nests a `Condition::And`/`Or` block visually indented.

Each condition row shows:
- Condition type icon
- Plain-English summary: `"Night Mode is On"`, `"Desk Lamp brightness > 50"`, `"Time is between 22:00 and 06:00"`
- Edit icon → inline form expands in place
- Remove icon

**Condition picker** (same bottom-sheet pattern as trigger picker):

| Type | Summary format |
|---|---|
| **Device State** | `{Device} {attribute} {op} {value}` — e.g. "Desk Lamp is On" |
| **Mode Is** | `{Mode} mode is {on/off}` |
| **Hub Variable** | `Hub var "{name}" {op} {value}` |
| **Time Window** | `Time is between {start} and {end}` |
| **Time Elapsed** | `{Device} {attribute} unchanged for {duration}` |
| **Private Boolean** | `Private flag "{name}" is {true/false}` |
| **Script Expression** | Shows first 60 chars of Rhai expression |
| **Not** | `NOT: {inner condition summary}` |
| **And / Or / Xor** | `All/Any/Exactly one of {N} conditions` — expands |

### 6.5 Action Sequence Editor

Actions are the heart of the rule. The action list uses an **outline tree** — control flow actions (Conditional, RepeatWhile, RepeatCount, Parallel) visually contain their child actions with indentation.

```
Actions
────────────────────────────────────────────────────────
① Turn On           Desk Lamp                    [⋮][✕]
② Set State         Desk Lamp  brightness = 80   [⋮][✕]
③ Wait              5 seconds                    [⋮][✕]
④ If  Night Mode is On  ──────────────────────── [⋮][✕]
   │  ① Dim  Desk Lamp  to 40%                   [⋮][✕]
   │  [+ Add action in THEN]
   Else ───────────────────────────────────────────────
   │  ① Set  Desk Lamp  brightness = 80          [⋮][✕]
   │  [+ Add action in ELSE]
⑤ Repeat 3 times ──────────────────────────────  [⋮][✕]
   │  ① Flash  Desk Lamp  0.5s                   [⋮][✕]
   │  ② Wait  500ms                              [⋮][✕]
   │  [+ Add action in REPEAT]
────────────────────────────────────────────────────────
[+ Add action]
```

Drag handles (three-line icon) allow reordering at the same level. Actions cannot be dragged into/out of nested contexts via drag (use Cut/Paste from the ⋮ menu instead).

### 6.6 Action Picker

"+ Add action" opens a categorized picker:

**Category: Device Control**
- **Set Device State** — `Set [device] [attribute] to [value]`
- **Set Per Mode** — `Set [device] state based on mode`
- **Fade Device** — `Fade [device] to [state] over [duration]`
- **Activate Scene** — `Activate [scene name]`
- **Activate Scene Per Mode** — `Activate scene based on mode`
- **Capture Device State** — `Save state of [devices] as [key]`
- **Restore Device State** — `Restore state saved as [key]`

**Category: Modes**
- **Set Mode** — `Set [mode] to [On/Off/Toggle]`

**Category: Wait & Delay**
- **Wait (Delay)** — `Wait [N] seconds`
- **Wait Per Mode** — `Wait [duration] based on mode`
- **Wait for Event** — `Wait until [event type / device change]`
- **Wait for Expression** — `Wait until [expression] is true`

**Category: Logic & Flow**
- **If/Then/Else** (Conditional) — `If [expression] then ... else ...`
- **Repeat N Times** (RepeatCount) — `Repeat [N] times`
- **Repeat While** — `Repeat while [expression] is true`
- **Repeat Until** — `Repeat until [expression] is true`
- **Run in Parallel** — `Do these at the same time`
- **Exit Rule** — `Stop this rule`
- **Stop Rule Chain** — `Stop all rules for this event`

**Category: Variables & State**
- **Set Variable** (rule-local) — `Set var [name] to [value]`
- **Set Hub Variable** — `Set hub var [name] to [value]`
- **Set Private Boolean** — `Set flag [name] to [true/false]`

**Category: Notifications & Integrations**
- **Notify** — `Send notification to [channel]: [message]`
- **Publish MQTT** — `Publish to [topic]: [payload]`
- **Call Service (HTTP)** — `POST/GET [url]`
- **Fire Event** — `Fire custom event [type]`
- **Ping Host** — `Ping [host]`

**Category: Rule Management**
- **Run Rule Actions** — `Run actions of [rule name]`
- **Pause Rule** — `Pause [rule name]`
- **Resume Rule** — `Resume [rule name]`
- **Cancel Delays** — `Cancel delays [all/key]`
- **Cancel Rule Timers** — `Cancel all timers`

**Category: Misc**
- **Log Message** — `Log "[message]" at [level]`
- **Comment** — `// [text]`

### 6.7 Action Configuration Forms

Each action type has an inline form that expands when clicked. Forms use the device name resolver for all pickers.

**Set Device State** example:
```
Device:    [ Desk Lamp ▾ ]  (resolved name, stores device_id)
Attribute: [ brightness ▾ ] (dropdown from device schema)
Value:     [────●──────]  80   (slider if numeric, toggle if bool)
☐ Track event value (mirror trigger value)
```

**If / Conditional** example:
```
Condition (Rhai expression):
[ device_state("light_desk", "on") == true          ]  (with name hints)

Alternatively: [Use condition builder ▾]
  → opens same condition picker as main Conditions section
```

**Wait (Delay)** example:
```
Duration: [ 30 ] [ seconds ▾ ]
☐ Cancellable   Label: [         ]
```

**Fade Device** example:
```
Device:    [ Desk Lamp ▾ ]
Target:
  on:         [●] On
  brightness: [──────●──]  40
Duration:  [ 30 ] seconds
Steps:     [ auto (1/sec) ]
```

### 6.8 Run Mode, Priority, and Settings

A collapsible "Settings" panel at the bottom of the editor:

| Field | UI |
|---|---|
| **Name** | Text field |
| **Priority** | Number field with stepper (higher = runs first) |
| **Run Mode** | Segmented button: Parallel \| Single \| Restart \| Queued(N) |
| **Cooldown** | Duration picker: none / 30s / 1m / 5m / custom |
| **Tags** | Chip input with autocomplete from existing tags |
| **Required Expression** | Optional Rhai expression gate (collapsible, with code hint) |
| **Trigger Label** | Optional string (used in Rhai as `trigger_label()`) |
| **Log Events/Triggers/Actions** | Three checkboxes |

### 6.9 TOML / Visual Toggle

A toggle in the editor header switches between visual editor and raw TOML view. When switching Visual→TOML, the current visual state is serialized to TOML. When switching TOML→Visual, the TOML is parsed and the form is populated; parse errors are shown inline.

This gives advanced users the full power of the underlying data model (e.g. custom Rhai expressions in RepeatWhile conditions) while keeping the visual editor as the default.

### 6.10 Dry Run ("Test") Panel

The **"Test Run"** button evaluates the rule's conditions against the current live state (calls `POST /automations/{id}/test`) and shows a result panel:

```
Test Results (as of 14:03:22)

Trigger:    Sun event — not applicable in dry run

Conditions:
  ✓ Night Mode is On       (actual: On)
  ✗ Desk Lamp is Off       (actual: On) ← FAIL

→ Rule would NOT fire (condition 2 failed)

Actions that would have run: (hypothetical)
  ① Set Desk Lamp brightness = 80
  ② Wait 5 seconds
  ...
```

---

## 7. Automations List Page

### 7.1 List Layout

```
Automations                                           [+ New]  [🔍]  [⚙]
────────────────────────────────────────────────────────────────────────────
[Filter: All ▾]  [Tag: ▾]  [Sort: Priority ▾]  [☐ Enabled only]

●  Name                     Trigger           Last Fired      Pri  Actions
────────────────────────────────────────────────────────────────────────────
●  Turn on at sunset        Sun: Sunset       2h ago          50   [▶][⋮]
●  Morning routine          Time: 07:00 daily 8h ago          60   [▶][⋮]
○  Away mode lights off     Mode: Away → On   Never           40   [▶][⋮]
●  Door open fan-in         Device: any door  4m ago          70   [▶][⋮]
```

- `●` green = enabled, `○` grey = disabled; click to toggle inline
- **[▶]** = test dry-run; shows result inline below the row
- **[⋮]** = Edit / Clone / Fire manually / Enable/Disable / Delete
- Trigger column: resolved human summary using `DeviceNameResolver`
- Last Fired: from fire history WebSocket updates / initial load

### 7.2 Bulk Operations

A checkbox column appears when hovering (or via "Select" mode button):
- Bulk enable / disable
- Bulk tag
- Bulk delete
- Bulk export (downloads TOML zip)

### 7.3 Fire History Drawer

Clicking a rule name opens a side drawer showing the last 20 firings from `/automations/{id}/history`:

```
Fire History: Morning routine
──────────────────────────────
▸ Fired         Today 07:00:03    ← expand to see condition traces
▸ Cooldown      Yesterday 07:00   remaining 0s
▸ Fired         2d ago 07:00:01
▸ Fired         3d ago 07:00:00
```

Expanding a "Fired" entry shows:
- Trigger context (device, attribute, value)
- Per-condition results (✓ / ✗ with actual vs expected)
- Per-action outcomes (Executed / Skipped)
- Total eval_ms + action_ms

---

## 8. Devices Page Enhancement

### 8.1 Grouped List (current) + Card View Toggle

Add a view toggle (list icon / grid icon) in the page header:
- **List view** (current): grouped by area, dense rows with toggle switches
- **Card view**: larger tiles with icon, name, key state value — better for touch/tablet

### 8.2 Inline State Controls

In both views, show the most useful control inline without needing to open the detail page:
- **Light with brightness:** slider chip inline
- **Switch:** toggle
- **Sensor:** last value + relative time
- **Thermostat:** current/setpoint inline

### 8.3 Device Detail Page

Add a **"Used in automations"** section at the bottom showing which rules reference this device (by scanning `trigger.device_id`, `condition.device_id`, action `device_id` fields).

Add a **"Used in scenes"** section listing scenes that include this device.

---

## 9. Modes Page Enhancement

### 9.1 Current State: Full Cards

Keep cards (they work well for modes) but make them more compact and add a "Modes at a glance" summary row at the top.

### 9.2 Active Mode Banner

A small banner/chip bar at the top of the Modes page (and in the dashboard status card) shows which modes are currently active:

```
Active modes:  🌙 Night  ●   🏠 Home  ●   🚗 Away  ○
```

Clicking a manual mode chip toggles it directly.

### 9.3 Mode History

Add a "Recent changes" list below each mode card: "Night turned On at 20:43" (from `mode_changed` event log).

---

## 10. Events / Log Page Enhancement

### 10.1 Filtered Event Stream

Add filter chips above the live event list:
- `device_state_changed` (default on)
- `rule_fired` (default on)
- `scene_activated` (default on)
- `mode_changed` (default on)
- `device_availability_changed` (default off)
- Custom event types (discovered from history)

### 10.2 Entity Name Resolution in Events

Events currently show raw JSON. Resolve all `device_id` fields to names in the event list. Show a "Raw JSON" toggle per event to see the original.

---

## 11. Implementation Phases

### Phase A — Foundation (name resolution + scenes list)
**Scope:** Non-breaking, high-value wins

- [ ] Implement `DeviceNameResolver` and `ModeNameResolver` providers
- [ ] Apply name resolution to: rule list trigger summaries, event log, fire history drawer
- [ ] Rebuild Scenes page as list (table layout) with activate, status, inline expansion
- [ ] Scene editor two-panel layout with device picker showing names
- [ ] Device detail: "Used in rules" + "Used in scenes" sections

### Phase B — Multiple Dashboards
**Scope:** New dashboard system, replaces the static dashboard page

- [ ] `DashboardConfig` model + localStorage persistence
- [ ] Dashboard tab bar in AppShell
- [ ] Implement 5 core card types: `entity`, `entities`, `glance`, `button`, `status`
- [ ] Default dashboard auto-generation from API data
- [ ] Dashboard editor (edit mode overlay with add/remove/reorder)
- [ ] Add card picker bottom sheet
- [ ] History-graph card (`fl_chart` sparkline)
- [ ] Markdown card
- [ ] Mode card
- [ ] Dashboard sync API endpoint integration (when backend adds it)

### Phase C — Automation Editor Visual Rebuild
**Scope:** Replace current editor with visual step-by-step builder

- [ ] Trigger picker (all trigger types with form fields)
- [ ] Condition builder (list with AND/OR logic toggle, nested groups)
- [ ] Action sequence tree (outline view, drag-to-reorder at same level)
- [ ] Action picker with categorized catalog
- [ ] Action configuration forms (all action types)
- [ ] TOML ↔ Visual toggle
- [ ] Dry-run result panel
- [ ] Settings panel (run_mode, cooldown, tags, etc.)
- [ ] Rule list enhancements: trigger summary, last fired, bulk ops
- [ ] Fire history side drawer

### Phase D — Device & Mode Enhancements
**Scope:** Polish remaining pages

- [ ] Device list card view toggle
- [ ] Inline brightness sliders / controls in device list
- [ ] Modes active-mode banner/chips
- [ ] Mode recent-changes list
- [ ] Events page filter chips + entity name resolution

### Phase E — Advanced / Power-User Features
**Scope:** Completeness for power users

- [ ] Rule import/export UI (drag-drop TOML files)
- [ ] Rule cloning from list (currently backend-only)
- [ ] Stale rule references warning (using `GET /automations/stale-refs`)
- [ ] Bulk automation operations UI
- [ ] Dashboard export/import (share dashboards as JSON)
- [ ] Rule templates library (pre-built patterns: "Turn off after N minutes", "Announce on doorbell", etc.)

---

## 12. Technical Implementation Notes

### 12.1 Entity Picker Widget

Used in rule editor trigger/condition/action forms everywhere a device is selected:

```dart
class EntityPickerField extends ConsumerWidget {
  final String? value;          // device_id (stored value)
  final ValueChanged<String?> onChanged;
  final String label;
  final List<String>? filterByType; // e.g. ["dimmer", "light"]

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final devices = ref.watch(devicesProvider).valueOrNull ?? [];
    final areas = ref.watch(areasProvider).valueOrNull ?? [];
    // Shows SearchableDropdown grouped by area, displays name, stores device_id
  }
}
```

Similar `ModePickerField`, `ScenePickerField`, `RulePickerField`, `HubVariableField`.

### 12.2 Attribute Value Widget

Renders the right control for an attribute value based on its type, derived from the device's JSON schema:

```dart
class AttributeValueControl extends StatelessWidget {
  final String deviceId;
  final String attribute;
  final dynamic value;
  final ValueChanged<dynamic> onChanged;
  // Renders: Switch (bool), Slider (num 0-100/0-255), TextInput (string),
  //          ColorPicker (color_xy/color_hs), SelectChip (enum)
}
```

### 12.3 Trigger Summary Builder

A pure function that produces a human-readable string from a `Trigger` object:

```dart
String buildTriggerSummary(Trigger trigger, DeviceNameResolver resolver) {
  return switch (trigger) {
    Trigger_DeviceStateChanged t => _deviceTriggerSummary(t, resolver),
    Trigger_TimeOfDay t => 'Every ${_formatDays(t.days)} at ${_formatTime(t.time)}',
    Trigger_SunEvent t => 'Sun: ${t.event.name}${_formatOffset(t.offsetMinutes)}',
    Trigger_ModeChanged t => t.modeId != null
        ? 'Mode: ${resolver.resolveMode(t.modeId!)}${t.to != null ? " → ${t.to! ? "On" : "Off"}" : ""}'
        : 'Any mode changed',
    Trigger_CalendarEvent t => 'Calendar${t.calendarId != null ? ": ${t.calendarId}" : ""}',
    Trigger_HubVariableChanged t => 'Hub var: ${t.name ?? "any"}',
    Trigger_MqttMessage t => 'MQTT: ${t.topicPattern}',
    Trigger_WebhookReceived t => 'Webhook: /${t.path}',
    Trigger_Cron t => _cronSummary(t.expression),
    Trigger_Periodic t => 'Every ${t.everyN} ${t.unit.name}',
    Trigger_ButtonEvent t => 'Button: ${resolver.resolve(t.deviceId)} ${t.event.name}',
    Trigger_NumericThreshold t => '${resolver.resolve(t.deviceId)}.${t.attribute} ${t.op.symbol} ${t.value}',
    Trigger_SystemStarted _ => 'System started',
    Trigger_ManualTrigger _ => 'Manual',
    Trigger_CustomEvent t => 'Event: ${t.eventType}',
  };
}
```

### 12.4 Dashboard Persistence

```dart
// Stored as:
// localStorage key: "hc_dashboards"
// Value: JSON array of Dashboard objects

class DashboardRepository {
  Future<List<Dashboard>> load();
  Future<void> save(List<Dashboard> dashboards);
  Future<void> reset(); // restore to default
}

// Default dashboard generation:
class DefaultDashboardBuilder {
  Dashboard build(List<DeviceState> devices, List<Area> areas, List<Scene> scenes, List<ModeState> modes);
  // Creates Home dashboard with: status card, per-area entities cards, quick scene buttons
}
```

### 12.5 Rule Model Updates

The Dart `Rule` model needs to be updated to match the new Rust fields:

```dart
// Add to rule.dart:
enum RunMode { parallel, single, restart, queued }

class Rule {
  // ... existing fields ...
  final RunMode runMode;       // default: parallel
  final int? queuedMaxQueue;  // only used when runMode == queued
}
```

Similarly, add `ModeChanged` trigger, `ModeIs` condition, `SetMode` action to the Dart model — these are new types added in items 56/59.

---

## 13. Reference: Full Screen Inventory

| Screen | Route | Primary Use | Phase |
|---|---|---|---|
| Login | `/login` | Auth | existing |
| Dashboard (multi) | `/` → tab | Home overview + custom | B |
| Devices List | `/devices` | Browse + control | A (enhances) |
| Device Detail | `/devices/:id` | Full device control | A |
| Device History | `/devices/:id/history` | Charts | existing |
| Scenes | `/scenes` | List + activate | A |
| Scene Editor | `/scenes/:id` | Create/edit | A |
| Automations | `/automations` | Browse + manage | C |
| Automation Editor | `/automations/:id` | Visual rule builder | C |
| Modes | `/modes` | Toggle modes | D |
| Events | `/events` | Live event stream | D |
| Admin/Users | `/admin/users` | User mgmt | existing |
| Admin/Plugins | `/admin/plugins` | Plugin status | existing |
| Admin/Areas | `/admin/areas` | Area mgmt | existing |
| Admin/System | `/admin/system` | System health | existing |
| Admin/Logs | `/admin/logs` | Log viewer | existing |
