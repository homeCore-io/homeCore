/** A device's room, as the app reported it with the device. */
import type { CoreDevice } from "./devices.js";
export const roomOf = (
  d: CoreDevice,
  _rooms?: unknown,
): { name: string } | undefined => (d.room ? { name: d.room } : undefined);
