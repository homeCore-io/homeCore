/**
 * Changing a WLED controller's own settings (`cfg.json`) through the plugin:
 * WLED merges what is sent, so each editor sends only its part, in WLED's
 * shape. Secrets come back masked and are never sent back by the plugin.
 */
import { runAction } from "./pluginAdmin.js";
import { WLED } from "./wled.js";

/** `null` when done, else what went wrong. */
export async function applySettings(
  deviceId: string,
  settings: Record<string, unknown>,
  restart = false,
): Promise<string | null> {
  const r = await runAction(WLED, "apply_settings", {
    device_id: deviceId,
    settings,
    restart,
  });
  return r.ok ? null : r.error;
}

/** A WLED timer, as `cfg.json` keeps it. */
export interface Timer {
  en: number;
  /** 0–23, or 255 at sunrise, 254 at sunset (then `min` is an offset). */
  hour: number;
  min: number;
  macro: number;
  /** Bit 0 Monday … bit 6 Sunday. */
  dow?: number;
  start?: { mon: number; day: number };
  end?: { mon: number; day: number };
}

export const SUNRISE = 255;
export const SUNSET = 254;
export const isSun = (t: Timer) => t.hour === SUNRISE || t.hour === SUNSET;
/** A slot WLED keeps but nothing is in (it drops it when saving). */
export const isEmptyTimer = (t: Timer) =>
  !isSun(t) && t.macro === 0 && t.en === 0;
export const DAYS = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];

/**
 * The list to send. WLED (0.14 and on) keeps regular timers before the
 * sunrise and sunset ones, and doesn't drop one that is left out — a removed
 * timer is sent emptied, in its place, and WLED leaves it out when it saves.
 */
export function timersToSend(list: Timer[]): Timer[] {
  const regular = list.filter((t) => !isSun(t));
  const sun = list.filter(isSun).sort((a, b) => b.hour - a.hour);
  return [...regular, ...sun];
}
export const emptied = (t: Timer): Timer => ({
  ...t,
  en: 0,
  hour: 0,
  min: 0,
  macro: 0,
});

/** WLED's time zones, by the number `cfg.json` keeps (`if.ntp.tz`). */
export const TIME_ZONES = [
  "UTC",
  "UK",
  "Central Europe",
  "Eastern Europe",
  "US Eastern",
  "US Central",
  "US Mountain",
  "US Arizona",
  "US Pacific",
  "China",
  "Japan",
  "Australia Eastern",
  "New Zealand",
  "North Korea",
  "India",
  "Saskatchewan",
  "Australia Northern",
  "Australia Southern",
  "Hawaii",
  "Novosibirsk",
  "Anchorage",
  "Mexico Central",
  "Pakistan",
  "Brasília",
  "Australia Western",
];

/** `[10,0,10,74]` ↔ `"10.0.10.74"`; all zeros is "by DHCP". */
export const ipText = (ip: unknown) =>
  Array.isArray(ip) && ip.length === 4 && ip.some((n) => n !== 0)
    ? ip.join(".")
    : "";
export function ipList(text: string): number[] | null {
  const t = text.trim();
  if (!t) return [0, 0, 0, 0];
  const parts = t.split(".").map(Number);
  return parts.length === 4 &&
    parts.every((n) => Number.isInteger(n) && n >= 0 && n <= 255)
    ? parts
    : null;
}
