/**
 * The plugin's actions and settings, through `host`, in the shapes the
 * screens used (`Answer`, `Started`).
 */
import { host } from "./host.js";

export type Answer<T> =
  { ok: true; data: T } | { ok: false; error: string; status?: number };
export type Started =
  | { kind: "stream"; request_id: string }
  | { kind: "reply"; reply: Record<string, unknown> }
  | { kind: "busy"; request_id?: string };
export interface PluginConfig {
  config?: Record<string, unknown>;
}

/** One of this plugin's actions. The id is the plugin's own; it is ignored. */
export async function runAction(
  _plugin: string,
  action: string,
  params: Record<string, unknown>,
): Promise<Answer<Started>> {
  const r = await host().action(action, params);
  return r.ok
    ? { ok: true, data: { kind: "reply", reply: r.data } }
    : { ok: false, error: r.error };
}

export async function lifecycle(
  _plugin: string,
  what: "restart",
): Promise<Answer<unknown>> {
  if (what !== "restart") return { ok: false, error: "only restart" };
  const r = await host().config.restartPlugin();
  return r.ok ? { ok: true, data: null } : { ok: false, error: r.error };
}

export async function loadConfig(
  _plugin: string,
): Promise<Answer<PluginConfig>> {
  const r = await host().config.get();
  return r.ok
    ? { ok: true, data: { config: r.data } }
    : { ok: false, error: r.error };
}

export async function saveConfig(
  _plugin: string,
  body: { patch: Record<string, Record<string, unknown>> },
): Promise<Answer<unknown>> {
  for (const [section, values] of Object.entries(body.patch)) {
    const r = await host().config.patch(section, values);
    if (!r.ok) return { ok: false, error: r.error };
  }
  return { ok: true, data: null };
}
