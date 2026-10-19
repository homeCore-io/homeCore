/**
 * The `host` the app hands this plugin's elements, kept for the modules
 * below — which give WLED's screens the names they used when they were the
 * app's own (`#lib/commands.svelte.js` and the rest), each over `host`.
 */
import type { Host } from "../../../../../sdk/ui/index.js";
import { devices } from "./devices.svelte.js";
export type { Host };

let current: Host | undefined;
let stop: (() => void) | undefined;

/** Called by each element as it starts; the first one wires the devices. */
export function setHost(h: Host): void {
  if (current === h) return;
  current = h;
  stop?.();
  stop = h.devices.subscribe((list) => devices.replace(list));
}

export function host(): Host {
  if (!current)
    throw new Error("hc-wled UI: used before the app handed it a host");
  return current;
}
