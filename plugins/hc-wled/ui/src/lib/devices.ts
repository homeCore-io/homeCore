/** A device as the screens use it — the app's `CoreDevice`, from `host`. */
export interface CoreDevice {
  device_id: string;
  name: string;
  name_override?: string;
  plugin_id?: string;
  device_type?: string;
  available: boolean;
  attributes: Record<string, unknown>;
  last_seen?: string;
  sw_version?: string;
  /** Its room's name, as the app reports it. */
  room?: string | null;
}

export function displayName(d: CoreDevice): string {
  return d.name_override ?? d.name;
}
