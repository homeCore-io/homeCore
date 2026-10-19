/**
 * What the automation editor gave WLED's step form when it was the app's
 * own — the devices it can pick from, and a lookup — from `host` now: this
 * plugin's own devices are the only ones a WLED step can set.
 */
import { devices } from "#lib/devices.svelte.js";
import type { CoreDevice } from "#lib/devices.js";

export function editor() {
  return {
    get devices(): CoreDevice[] {
      return Object.values(devices.byId);
    },
    house: {
      device: (id: string): CoreDevice | undefined => devices.byId[id],
    },
  };
}
