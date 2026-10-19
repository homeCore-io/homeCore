/** This plugin's devices, live, as `devices.byId` — from `host`. */
import type { Device } from "../../../../../sdk/ui/index.js";
import type { CoreDevice } from "./devices.js";

class Devices {
  byId = $state<Record<string, CoreDevice>>({});
  replace(list: Device[]): void {
    this.byId = Object.fromEntries(
      list.map((d) => [
        d.device_id,
        {
          device_id: d.device_id,
          name: d.name,
          name_override: d.name_override ?? undefined,
          plugin_id: d.plugin_id,
          device_type: d.device_type ?? undefined,
          available: d.available,
          attributes: d.attributes,
          last_seen: d.last_seen ?? undefined,
          sw_version: d.sw_version ?? undefined,
          room: d.room,
        },
      ]),
    );
  }
}
export const devices = new Devices();
