/**
 * WLED controllers (hc-wled): what the plugin publishes, read for the screens
 * that show a controller, and the commands those screens and automations send.
 *
 * The plugin publishes a controller's whole state — every segment, nightlight,
 * sync, live data, sound — and, on a slow tick, its presets and playlists, its
 * firmware, and where its effect catalogue is kept. The catalogue itself (each
 * effect's controls, each palette's colours) is large and fetched when a
 * screen needs it (`wledCatalog.svelte.ts`).
 *
 * Commands are homeCore's words for WLED's JSON API, as the plugin's
 * `command.rs` takes them: `{segment, effect_id, palette_id, color2, …}`.
 * Shared ideas use homeCore's shared names and units: `brightness_pct`
 * (0–100), `transition_ms`, `color_temp` (kelvin), `color_rgb` (`{r,g,b}`).
 */
import type { CoreDevice } from "./devices.js";

export const WLED = "plugin.wled";
export const isWled = (d: CoreDevice | undefined): boolean =>
  d?.plugin_id === WLED;

// ---- the catalogue ----

export interface Slider {
  /** The command word: effect_speed, effect_intensity, custom1–3. */
  key: string;
  label: string;
  max: number;
}
export interface EffectOption {
  key: string;
  label: string;
}
export interface ColourSlot {
  slot: number;
  label: string;
}
export interface Effect {
  id: number;
  name: string;
  sliders: Slider[];
  options: EffectOption[];
  colors: ColourSlot[];
  /** The palette's name for this effect, or null when it uses none. */
  palette: string | null;
  one_d: boolean;
  two_d: boolean;
  single_pixel: boolean;
  sound: boolean;
  /** What its controls start at, by command word. */
  defaults: Record<string, number>;
}
export interface Stop {
  at?: number;
  color?: string;
  /** A segment colour, 0–2. */
  slot?: number;
  random?: boolean;
}
export interface Palette {
  id: number;
  name: string;
  stops: Stop[];
}
export interface Catalog {
  firmware: string;
  fetched_at: string;
  effects: Effect[];
  palettes: Palette[];
}

// ---- what the device publishes ----

export interface Segment {
  id: number;
  name: string | null;
  start: number;
  stop: number;
  grouping: number;
  spacing: number;
  offset: number;
  on: boolean;
  brightness_pct: number;
  /** White temperature, kelvin (WLED's 1900–10091 K). */
  color_temp: number;
  colors: string[];
  effect_id: number;
  effect_speed: number;
  effect_intensity: number;
  custom1: number;
  custom2: number;
  custom3: number;
  option1: boolean;
  option2: boolean;
  option3: boolean;
  palette_id: number;
  selected: boolean;
  reverse: boolean;
  mirror: boolean;
  freeze: boolean;
  blend_mode: number;
}

export interface PlaylistStep {
  preset_id: number;
  duration_s: number;
  transition_ms: number;
}
export interface PlaylistView {
  steps: PlaylistStep[];
  /** 0 plays it forever. */
  repeat: number;
  shuffle: boolean;
  /** 0 stays on the last; 255 goes back to what ran before. */
  end_preset_id: number;
}

export interface Preset {
  id: number;
  name: string;
  kind: "look" | "playlist" | "command";
  quick_label?: string;
  colors?: string[];
  effect_id?: number;
  palette_id?: number;
  brightness_pct?: number;
  segments?: number;
  playlist?: PlaylistView;
  /** What applying it does to power; absent, it leaves power as it is. */
  on?: boolean | string;
}

const arr = <T>(v: unknown): T[] => (Array.isArray(v) ? (v as T[]) : []);

export const segmentsOf = (d: CoreDevice): Segment[] =>
  arr<Segment>(d.attributes.segments);
export const presetsOf = (d: CoreDevice): Preset[] =>
  arr<Preset>(d.attributes.presets);
export const playlistsOf = (d: CoreDevice): Preset[] =>
  presetsOf(d).filter((p) => p.kind === "playlist");
export const effectNames = (d: CoreDevice): { id: number; name: string }[] =>
  arr<{ id: number; name: string }>(d.attributes.effects);
export const paletteNames = (d: CoreDevice): { id: number; name: string }[] =>
  arr<{ id: number; name: string }>(d.attributes.palettes);

export function nameOf(
  list: { id: number; name: string }[],
  id: unknown,
): string | undefined {
  return list.find((x) => x.id === id)?.name;
}

/** The segment a screen opens on: the main one, else the first. */
export function mainSegment(d: CoreDevice): Segment | undefined {
  const segs = segmentsOf(d);
  return segs.find((s) => s.id === d.attributes.main_segment) ?? segs[0];
}

/** Where its catalogue is kept: on the controller, or a copy in the app. */
export const catalogStorage = (d: CoreDevice): "controller" | "app" =>
  d.attributes.catalog_storage === "app" ? "app" : "controller";

// ---- drawing ----

/** The random palettes' stand-in colours: WLED makes new ones as it runs. */
const RANDOM = ["#e63c3c", "#3cb4e6", "#f0c828", "#783cdc"];

/** A palette as a CSS gradient, its colour slots drawn in `colours`. */
export function paletteCss(p: Palette | undefined, colours: string[]): string {
  if (!p || p.stops.length === 0) return colours[0] ?? "#808080";
  const colourOf = (s: Stop, i: number) =>
    s.color ??
    (s.slot !== undefined
      ? (colours[s.slot] ?? "#000000")
      : (RANDOM[i % RANDOM.length] ?? "#808080"));
  if (p.stops.length === 1) return colourOf(p.stops[0]!, 0);
  const n = p.stops.length;
  const parts = p.stops.map((s, i) => {
    const at = s.at ?? i / (n - 1);
    return `${colourOf(s, i)} ${Math.round(at * 100)}%`;
  });
  return `linear-gradient(90deg, ${parts.join(", ")})`;
}

/** What a segment shows, roughly: its palette if the effect uses one, else its colours. */
export function segmentCss(
  seg: Segment | undefined,
  catalog: Catalog | undefined,
): string {
  if (!seg) return "#808080";
  const effect = catalog?.effects.find((e) => e.id === seg.effect_id);
  const usesPalette = effect ? effect.palette !== null : true;
  if (usesPalette) {
    const p = catalog?.palettes.find((x) => x.id === seg.palette_id);
    if (p) return paletteCss(p, seg.colors);
  }
  const c = seg.colors.filter((x) => x && x !== "#000000");
  if (c.length === 0) return seg.colors[0] ?? "#000000";
  if (c.length === 1) return c[0]!;
  return `linear-gradient(90deg, ${c.join(", ")})`;
}

/** A preset's look as a CSS background, from the colours it holds. */
export function presetCss(p: Preset): string {
  const c = (p.colors ?? []).filter(Boolean);
  if (c.length === 0) return "var(--surface-2)";
  if (c.length === 1) return c[0]!;
  const step = 100 / c.length;
  return `linear-gradient(90deg, ${c.map((x, i) => `${x} ${Math.round(i * step)}% ${Math.round((i + 1) * step)}%`).join(", ")})`;
}

// ---- finding an effect ----

export type EffectFilter = "all" | "palette" | "sound" | "favourites";

export function findEffects(
  effects: Effect[],
  q: string,
  filter: EffectFilter,
  favourites: ReadonlySet<number> = new Set(),
): Effect[] {
  const words = q.trim().toLowerCase().split(/\s+/).filter(Boolean);
  return effects.filter((e) => {
    if (filter === "palette" && e.palette === null) return false;
    if (filter === "sound" && !e.sound) return false;
    if (filter === "favourites" && !favourites.has(e.id)) return false;
    const name = e.name.toLowerCase();
    return words.every((w) => name.includes(w));
  });
}

// ---- commands ----

export type SegmentTarget = number | "all" | "selected";

/** What changes a segment's look: one target, the words of the change. */
export function lookCommand(
  target: SegmentTarget,
  change: Record<string, unknown>,
): Record<string, unknown> {
  return { segment: target, ...change };
}

/** Choosing an effect starts it at its own settings, as WLED's page does. */
export function effectCommand(
  target: SegmentTarget,
  e: Effect,
): Record<string, unknown> {
  return lookCommand(target, { effect_id: e.id, ...e.defaults });
}

/** `#rrggbb` as `[r,g,b]`. */
export function rgb(hex: string): [number, number, number] {
  const h = hex.replace("#", "");
  return [0, 2, 4].map((i) => parseInt(h.slice(i, i + 2), 16) || 0) as [
    number,
    number,
    number,
  ];
}

export interface Rgb {
  r: number;
  g: number;
  b: number;
}

/** `#rrggbb` as the `{r,g,b}` a colour is sent as. */
export function rgbOf(hex: string): Rgb {
  const [r, g, b] = rgb(hex);
  return { r, g, b };
}

/** A colour — `{r,g,b}`, `[r,g,b]` or `#rrggbb` — as `#rrggbb`. */
export function hex(c: unknown): string {
  if (typeof c === "string") return c;
  const o = c as Partial<Rgb> | null;
  const a = Array.isArray(c)
    ? c
    : o && typeof o === "object"
      ? [o.r, o.g, o.b]
      : [];
  return `#${[0, 1, 2]
    .map((i) =>
      Math.max(0, Math.min(255, Number(a[i] ?? 0)))
        .toString(16)
        .padStart(2, "0"),
    )
    .join("")}`;
}

// ---- an automation's WLED step ----

export type StepKind =
  "preset" | "playlist" | "look" | "nightlight" | "power" | "sync";

export interface WledStep {
  kind: StepKind;
  preset_id?: number;
  playlist_id?: number;
  segment?: SegmentTarget;
  /** The look's words: effect_id, palette_id, effect_speed, color_rgb, color2, … */
  look?: Record<string, unknown>;
  nightlight?: {
    on: boolean;
    duration_min: number;
    mode: string;
    target_pct: number;
  };
  power?: boolean | "toggle";
  sync?: { send?: boolean; receive?: boolean; live_override?: number };
  /** Brightness to set with it; left out, it stays as the preset or strip has it. */
  brightness_pct?: number;
  /** Its fade, ms; left out, the controller's own. */
  transition_ms?: number;
  /** "Turn it on" said beside a preset, playlist or look, as older steps do. */
  on?: true;
}

const LOOK_KEYS = new Set([
  "effect_id",
  "palette_id",
  "effect_speed",
  "effect_intensity",
  "custom1",
  "custom2",
  "custom3",
  "option1",
  "option2",
  "option3",
  "color_rgb",
  "color2",
  "color3",
  "colors",
  "color_temp",
  "segment_on",
  "segment_brightness_pct",
]);
const SHARED = new Set(["brightness_pct", "transition_ms", "on"]);
/** Old spellings the plugin still takes, read as the names they became. */
const RENAMED: Record<string, string> = {
  transition: "transition_ms",
  color: "color_rgb",
};

/** The command a step sends. */
export function stepState(s: WledStep): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  switch (s.kind) {
    case "preset":
      out.preset_id = s.preset_id ?? 1;
      break;
    case "playlist":
      out.playlist_id = s.playlist_id ?? 1;
      break;
    case "look":
      out.segment = s.segment ?? "all";
      Object.assign(out, s.look ?? {});
      break;
    case "nightlight": {
      const n = s.nightlight ?? {
        on: true,
        duration_min: 30,
        mode: "fade",
        target_pct: 0,
      };
      out.nightlight = { ...n };
      break;
    }
    case "power":
      out.on = s.power ?? true;
      break;
    case "sync":
      if (s.sync?.send !== undefined) out.sync_send = s.sync.send;
      if (s.sync?.receive !== undefined) out.sync_receive = s.sync.receive;
      if (s.sync?.live_override !== undefined)
        out.live_override = s.sync.live_override;
      break;
  }
  if (
    s.kind !== "power" &&
    s.kind !== "sync" &&
    s.kind !== "nightlight" &&
    s.brightness_pct !== undefined
  )
    out.brightness_pct = s.brightness_pct;
  if (
    s.transition_ms !== undefined &&
    s.kind !== "sync" &&
    s.kind !== "nightlight"
  )
    out.transition_ms = s.transition_ms;
  if (
    s.on &&
    (s.kind === "preset" || s.kind === "playlist" || s.kind === "look")
  )
    out.on = true;
  return out;
}

const isObj = (v: unknown): v is Record<string, unknown> =>
  v !== null && typeof v === "object" && !Array.isArray(v);

/**
 * A command read back as a step — the step's form for one written by hand,
 * too — or null when it says something no step does (then it stays JSON).
 */
export function stepOf(given: unknown): WledStep | null {
  if (!isObj(given)) return null;
  const state: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(given)) {
    const to = RENAMED[k];
    if (to === undefined) state[k] = v;
    else if (given[to] === undefined) state[to] = v;
  }
  const keys = Object.keys(state).filter((k) => k !== "action");
  const num = (k: string) =>
    typeof state[k] === "number" ? (state[k] as number) : undefined;
  const shared = {
    ...(num("brightness_pct") !== undefined
      ? { brightness_pct: num("brightness_pct") }
      : {}),
    ...(num("transition_ms") !== undefined
      ? { transition_ms: num("transition_ms") }
      : {}),
    // Older steps say "on" beside a preset or a look; keep it, say it.
    ...(state.on === true ? { on: true as const } : {}),
  };
  const only = (allowed: Set<string>) => keys.every((k) => allowed.has(k));
  const preset = num("preset_id");
  if (
    preset !== undefined &&
    only(new Set(["preset_id", "brightness_pct", "transition_ms", "on"]))
  )
    return { kind: "preset", preset_id: preset, ...shared };
  if (
    num("playlist_id") !== undefined &&
    only(new Set(["playlist_id", "brightness_pct", "transition_ms", "on"]))
  )
    return { kind: "playlist", playlist_id: num("playlist_id"), ...shared };
  if (isObj(state.nightlight) && keys.length === 1) {
    const n = state.nightlight;
    return {
      kind: "nightlight",
      nightlight: {
        on: n.on !== false,
        duration_min: Number(n.duration_min ?? 60),
        mode: String(n.mode ?? "fade"),
        target_pct: Number(n.target_pct ?? 0),
      },
    };
  }
  if (
    keys.some((k) => k.startsWith("sync_") || k === "live_override") &&
    only(new Set(["sync_send", "sync_receive", "live_override"]))
  ) {
    return {
      kind: "sync",
      sync: {
        ...(typeof state.sync_send === "boolean"
          ? { send: state.sync_send }
          : {}),
        ...(typeof state.sync_receive === "boolean"
          ? { receive: state.sync_receive }
          : {}),
        ...(num("live_override") !== undefined
          ? { live_override: num("live_override") }
          : {}),
      },
    };
  }
  if (
    keys.some((k) => LOOK_KEYS.has(k)) &&
    keys.every((k) => LOOK_KEYS.has(k) || SHARED.has(k) || k === "segment")
  ) {
    if (keys.includes("on") && state.on !== true) return null;
    const look: Record<string, unknown> = {};
    for (const k of keys) if (LOOK_KEYS.has(k)) look[k] = state[k];
    const seg = state.segment;
    const segment: SegmentTarget =
      seg === "all" || seg === "selected"
        ? seg
        : typeof seg === "number"
          ? seg
          : 0;
    return { kind: "look", segment, look, ...shared };
  }
  if (
    (state.on === true || state.on === false || state.on === "toggle") &&
    only(new Set(["on", "brightness_pct", "transition_ms"]))
  )
    return {
      kind: "power",
      power: state.on as boolean | "toggle",
      ...(num("transition_ms") !== undefined
        ? { transition_ms: num("transition_ms") }
        : {}),
    };
  return null;
}

/** A step read as a sentence: "Apply Halloween 1 on Eaves lights, fading over 2 s". */
export function stepSentence(
  s: WledStep,
  who: string,
  names: {
    preset?: (id: number) => string | undefined;
    effect?: (id: number) => string | undefined;
    palette?: (id: number) => string | undefined;
  } = {},
): string {
  const fade =
    s.transition_ms !== undefined
      ? s.transition_ms === 0
        ? ", at once"
        : `, fading over ${Math.round(s.transition_ms / 100) / 10} s`
      : "";
  const bri = s.brightness_pct !== undefined ? ` at ${s.brightness_pct}%` : "";
  switch (s.kind) {
    case "preset": {
      const n = names.preset?.(s.preset_id ?? 0) ?? `preset ${s.preset_id}`;
      return s.on
        ? `Turn ${who} on with ${n}${bri}${fade}`
        : `Apply ${n} on ${who}${bri}${fade}`;
    }
    case "playlist": {
      const n =
        names.preset?.(s.playlist_id ?? 0) ?? `playlist ${s.playlist_id}`;
      return s.on
        ? `Turn ${who} on and start the playlist ${n}`
        : `Start the playlist ${n} on ${who}`;
    }
    case "look": {
      const l = s.look ?? {};
      const bits: string[] = [];
      if (typeof l.effect_id === "number")
        bits.push(names.effect?.(l.effect_id) ?? `effect ${l.effect_id}`);
      if (typeof l.palette_id === "number")
        bits.push(`palette ${names.palette?.(l.palette_id) ?? l.palette_id}`);
      if (l.color_rgb || l.color2 || l.color3 || l.colors)
        bits.push("new colours");
      if (typeof l.color_temp === "number") bits.push(`${l.color_temp} K`);
      const where =
        s.segment === "all" || s.segment === undefined
          ? ""
          : s.segment === "selected"
            ? " (selected segments)"
            : ` (segment ${s.segment})`;
      return `${s.on ? "Turn on " : "Set "}${who}${where}${s.on ? " with" : " to"} ${bits.join(", ") || "a new look"}${bri}${fade}`;
    }
    case "nightlight": {
      const n = s.nightlight;
      if (!n || !n.on) return `Stop ${who}'s nightlight`;
      const to = n.target_pct > 0 ? `${n.target_pct}%` : "off";
      const how =
        n.mode === "sunrise"
          ? "brighten (sunrise)"
          : n.mode === "instant"
            ? "go"
            : "fade";
      return `Start ${who}'s nightlight: ${how} to ${to} over ${n.duration_min} min`;
    }
    case "power":
      return s.power === "toggle"
        ? `Flip ${who}`
        : `Turn ${who} ${s.power === false ? "off" : "on"}${fade}`;
    case "sync": {
      const parts: string[] = [];
      if (s.sync?.send !== undefined)
        parts.push(s.sync.send ? "send changes" : "stop sending changes");
      if (s.sync?.receive !== undefined)
        parts.push(
          s.sync.receive ? "follow other WLEDs" : "stop following others",
        );
      if (s.sync?.live_override !== undefined)
        parts.push(
          [
            "let live data take over",
            "ignore live data until it stops",
            "ignore live data until restart",
          ][s.sync.live_override] ?? "",
        );
      return `Make ${who} ${parts.join(", ") || "keep its sync as it is"}`;
    }
  }
}

/**
 * The same preset on another controller, by name: a step on several
 * controllers applies "Halloween 1" on each, whatever slot each keeps it in.
 */
export function presetOn(
  target: CoreDevice,
  from: CoreDevice,
  id: number,
): number | undefined {
  const name = presetsOf(from).find((p) => p.id === id)?.name;
  if (name === undefined) return undefined;
  return presetsOf(target).find((p) => p.name === name)?.id;
}
