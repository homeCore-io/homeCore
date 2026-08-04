# Getting hc-web off Riverpod's legacy shim

Fourteen files still import `package:flutter_riverpod/legacy.dart`. Riverpod 3
kept `StateProvider`, `StateNotifier` and `StateNotifierProvider` alive behind
that import so the 2 → 3 upgrade could be a version bump; it was. This is the
other half, and it is not a version bump.

## Why, and why the urgency is low

Nothing is broken and nothing is warning. The shim carries no `@Deprecated`
annotation in 3.4.2, so `flutter analyze` is green today and stays green — the
CI gate will not tell us when this becomes a problem. The forcing function is
Riverpod 4, whenever that lands, at which point the shim goes away and this
becomes an emergency instead of a plan.

So: schedule it, do it once, do it properly. The reason to do it *properly*
rather than mechanically is that the eight preference providers all share an
async-load pattern whose correctness has never been tested, and the port is
the only moment when we will be looking straight at it.

## What is actually there

Two cohorts, and they are not the same job.

### Cohort A — eight `StateNotifier` preference stores (the real work)

Every one of these is the same shape: construct with a default, kick off an
async `_load()` from `SharedPreferences`, assign `state` if still `mounted`,
and write through on every mutation.

| File | Provider(s) | State | Consumers outside its file |
|---|---|---|---|
| `core/providers/time_display_provider.dart` | `timeUtcProvider` | `bool` | 7 |
| `core/providers/collapsed_groups_provider.dart` | `collapsedGroupsProvider` | `Set<String>` | 3 |
| `core/providers/nav_prefs_provider.dart` | `navRailVisibleProvider`, `navRailExpandedProvider`, `landingRouteProvider` | `bool`, `bool`, `String` | 2, 1, 1 |
| `core/providers/active_sort_provider.dart` | `activeSortProvider` | `bool` | 1 |
| `core/providers/room_collapse_provider.dart` | `roomCollapseProvider` | `Set<String>` | 1 |
| `core/providers/thermostat_prefs_provider.dart` | `thermostatLargeProvider` | `Set<String>` | 1 |
| `core/providers/client_error_log_provider.dart` | `clientErrorLogProvider` | `List<ApiErrorEntry>` | 1 |
| `core/providers/scenes_provider.dart` | `sceneActivatedTimesProvider` | `Map<String, DateTime>` | **0** |

`clientErrorLogProvider` is the odd one out — in-memory only, no
`SharedPreferences`, a 100-entry ring buffer. It is the easiest of the eight.

### Cohort B — six bare `StateProvider`s of private UI state (mechanical)

| File | Providers | Notes |
|---|---|---|
| `features/events/events_page.dart` | `_liveEventsProvider`, `_liveTypeFilterProvider`, `_historyLimitProvider`, `_historyTypeFilterProvider`, `_historyDeviceSearchProvider` | five, all file-private |
| `features/automations/automation_list_page.dart` | `_filterProvider`, `_selectionProvider` | file-private |
| `features/devices/device_list_page.dart` | `_queryProvider` | file-private |
| `features/scenes/scenes_page.dart` | `_filterProvider` | file-private |
| `shell/shell_scope.dart` | `skinOverrideProvider` | **public**, and overridden in a test |
| `shell/wall_chrome.dart` | `kioskProvider` | public, one-way latch to `true` |

There are 12 `ref.read(x.notifier).state = …` assignment sites across cohort B.
`Notifier` has no public `state` setter from outside, so each becomes a named
method on the notifier. That is the bulk of the diff and none of it is subtle —
but it is also the part that makes the change readable, because
`.notifier).state = next` says nothing about intent and `.clearFilters()` does.

## Three things to decide before writing any code

**1. `sceneActivatedTimesProvider` is dead.** Nothing watches it, which means
its notifier is never constructed, which means its `ref.listen` on the event
stream never runs. The "scene last activated at" data it exists to collect is
not being collected. Do not port dead code — either delete it, or wire it up
first and port it as a live feature. That is a product call, not a migration
call.

**2. `landing_route` has two readers and one of them hard-codes the key.**
`nav_prefs_provider.dart` owns `_kLandingRouteKey = 'landing_route'`, but
`app.dart:65` reads `prefs.getString('landing_route')` directly, as a string
literal, inside the router's redirect. That is deliberate and arguably correct
— the router can `await` and the provider cannot — but it means the key exists
in two places. Export the constant and have `app.dart` use it, in the same
commit, or this desyncs the first time someone renames it.

**3. Whether to fix the async-load race or preserve it.** Every cohort-A
provider yields its default for one or more frames and then swaps to the stored
value. Visibly, that is the nav rail flashing expanded before collapsing, and
Home sorting A–Z before re-sorting active-first. It has always done this. The
port can preserve that behaviour exactly, or fix it by awaiting
`SharedPreferences.getInstance()` once in `main()` and injecting it through an
overridden provider — after which every `build()` is synchronous and correct
with no flash.

Fixing it is the better outcome and is genuinely small (Riverpod 3 caches
nothing here; `SharedPreferences.getInstance()` is already a singleton after
first call). But it changes startup ordering for the whole app and it is not
the migration. **Recommendation: preserve behaviour in the migration, fix the
flash in a separate follow-up commit**, so that if something regresses we know
which of the two caused it.

## Phases

### Phase 0 — pin the behaviour that has no tests — **DONE**

Landed as `hc-web` `ee8131c`, `test/core/providers/preferences_test.dart`,
34 tests. Suite went 871 → 905, green; `flutter analyze` clean; `dart format`
clean.

Two notes on what was actually written:

- **Mutation-checked, not just green.** Renaming `time_display_utc` fails 3
  tests; changing its absent-key fallback from `false` to `true` fails 3. Both
  were verified by actually making the change and running, then reverting.
- **The two public cohort-B providers were left alone.** The plan called for
  pinning `kioskProvider`'s latch and `skinOverrideProvider`'s default, but both
  are one-line `StateProvider` initializers with no logic — a test would assert
  the literal sitting two lines above it. `shell_test.dart` already exercises
  `skinOverrideProvider` through an override, and Phase 2's conversion of both
  is compiler-checked at every call site. Not worth a vacuous test.

The original brief, kept for reference:


None of the eight preference providers has a single test. `SharedPreferences`
appears nowhere in `test/`. This is the phase that makes the rest safe, and it
should land and be reviewed *before* any provider changes.

Write `test/core/providers/preferences_test.dart` against the **current,
unported** code, using `SharedPreferences.setMockInitialValues({})`. For each
of the eight, assert:

- the default when the key is absent (`activeSort` is `true`, `navRailExpanded`
  is `false`, `landingRoute` is `'/'`, the three `Set` providers start empty)
- the stored value wins once `_load()` completes
- a mutation writes through — read `SharedPreferences` back and check the key
  *and its exact stored type* (`getBool` / `getStringList` / `getString`)
- the key strings themselves, spelled out as literals in the test, so a rename
  during the port fails loudly instead of silently orphaning everyone's saved
  preferences

That last one matters more than it looks. These keys are live on John's
browsers; changing one is indistinguishable from a factory reset of that
preference, and nothing would report an error.

Also pin `clientErrorLogProvider`'s 100-entry cap and its FIFO eviction, and
`kioskProvider`'s latch.

Green here, committed, before proceeding.

### Phase 1 — cohort A, one provider per commit

Convert `StateNotifier<T>` → `Notifier<T>`, `StateNotifierProvider<N, T>` →
`NotifierProvider<N, T>`. The mapping:

- constructor default → the return value of `build()`
- constructor side effects → statements in `build()` before the return
- `mounted` → `ref.mounted` (present in 3.4.2, same semantics)
- `ref.listen(...)` in the constructor → `ref.listen(...)` in `build()`
  (`scenes_provider.dart` only, and only if decision 1 says keep it)

Both `NotifierProvider` and `StateNotifierProvider` default to
`isAutoDispose = false` in 3.4.2, so provider lifetime does not change. That is
worth stating because it is the one thing that would silently break the error
log and the kiosk latch, and it does not.

Order by blast radius, smallest first: `client_error_log` (no persistence),
then `active_sort`, `thermostat_prefs`, `room_collapse`, `collapsed_groups`,
`nav_prefs` (three notifiers in one file), and `time_display` last — it has
seven consumers and touches every timestamp on screen.

Public method names stay identical. `ref.watch(p)` and
`ref.read(p.notifier).toggle()` are unchanged at every call site, so cohort A
should produce **no diff outside `lib/core/providers/`**. If it does, something
was misunderstood.

Re-run Phase 0's tests unchanged after each commit. They were written against
the old implementation and must pass against the new one without edits — that
is the whole point of writing them first.

### Phase 2 — cohort B

`StateProvider<T>` → `NotifierProvider<X, T>` with a small named notifier and
explicit mutators replacing the 12 `.state =` sites. File-private providers
first (events, automations, devices, scenes pages) — those are contained and
the compiler finds every site.

Then the two public ones:

- `kioskProvider` — one setter, `enterKiosk()`.
- `skinOverrideProvider` — **this one breaks a test.**
  `test/shell/shell_test.dart:114` does
  `skinOverrideProvider.overrideWith((ref) => HcSkin.controlRoom)`.
  `StateProvider.overrideWith` takes `(ref) => value`; `NotifierProvider`'s
  takes `() => Notifier`. The test must change to
  `overrideWith(() => SkinOverride(HcSkin.controlRoom))` or equivalent. It is
  the only override site of a legacy provider in the suite, and it is the only
  test file that has to change in this whole plan.

### Phase 3 — remove the shim and prove it

Delete every `import 'package:flutter_riverpod/legacy.dart';`. Grep must return
zero. Then, and only then, `dart format`, `flutter analyze`, `flutter test`.

`dart format` is a release gate on this repo — analyze and test passing is not
sufficient. Run all three before pushing.

### Phase 4 — verify on the running system

Unit tests cannot see a flash, a lost preference, or a nav rail that comes back
in the wrong state. Deploy to the sandbox and check, by screenshot, on a
profile that already has stored preferences:

- collapsed rooms on Home are still collapsed after reload
- the nav rail's visible/expanded state survives a reload
- "Set as Home page" still lands there on a fresh load
- the events page filters still filter, and the device list query still queries
- 12/24-hour time is still whatever it was set to

Restore the sandbox afterwards. Do not claim this is done from a green test
run; the failure mode this plan is most exposed to — a preference key that
quietly stopped matching what is on disk — is invisible to every test and
obvious on the running page.

## What could go wrong

**A renamed or retyped preference key.** The worst outcome, because it presents
as "my collapsed rooms reset" weeks later with no error anywhere. Phase 0's
literal-key assertions are the defence, and they are the reason Phase 0 is not
optional.

**Startup ordering.** If someone folds the flash fix into Phase 1, a failure in
Phase 4 has two candidate causes instead of one. Keep them apart.

**`ref.listen` inside `build()`.** `build()` re-runs on dependency change,
whereas the old constructor ran once. Only `scenes_provider.dart` is affected,
and only if it survives decision 1 — but if it does, a duplicated listener is
the failure mode to look for.

## Not in scope

- **`ref.persist()`**, Riverpod 3's built-in storage-backed provider support.
  Seven of these eight providers hand-roll exactly what it does, so it is
  tempting. It is also a second migration wearing the first one's clothes, with
  its own storage-format questions. Note it, revisit after this lands.
- **`riverpod_generator` / `@riverpod` codegen.** The repo has
  `riverpod_generator` 2.6.4 in the pub cache but writes providers by hand
  throughout. Not a decision this plan should make for the codebase.
- **The 10 existing `AsyncNotifier`s** (auth, devices, automations, dashboards,
  users, audit, plugins, system config, camera store). They are already on the
  modern API and are the house style this migration is converging toward — the
  target shape already exists in-tree to copy from.
