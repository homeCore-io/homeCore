/** The kinds of alert plugins raise, and what the house decided — through `host`. */
import { host } from "./host.js";

export interface AlertKindSetting {
  plugin_id: string;
  topic: string;
  label: string;
  enabled: boolean;
  notify?: string | null;
}

class AlertKinds {
  list = $state<AlertKindSetting[]>([]);
  #started = false;
  ensure(): void {
    if (this.#started) return;
    this.#started = true;
    void this.load();
  }
  async load(): Promise<void> {
    const r = await host()
      .fetch("/alerts/kinds")
      .catch(() => null);
    this.list = r?.ok ? ((await r.json()) as AlertKindSetting[]) : [];
  }
  async put(
    pluginId: string,
    topic: string,
    body: { enabled?: boolean | null; notify?: string | null },
  ): Promise<string | null> {
    const r = await host()
      .fetch(
        `/alerts/kinds/${encodeURIComponent(pluginId)}/${encodeURIComponent(topic)}`,
        {
          method: "PUT",
          headers: { "content-type": "application/json" },
          body: JSON.stringify(body),
        },
      )
      .catch(() => null);
    if (!r) return "homeCore is not answering.";
    if (!r.ok)
      return r.status === 403
        ? "You aren't allowed to do this."
        : `homeCore said ${r.status}.`;
    await this.load();
    return null;
  }
}
export const alertKinds = new AlertKinds();
