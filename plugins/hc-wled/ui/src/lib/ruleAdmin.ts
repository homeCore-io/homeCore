/** Automations, through homeCore's API as the person using the screens. */
import { host } from "./host.js";
import type { Answer } from "./pluginAdmin.js";
import type { Rule } from "./ruleWords.js";

async function call<T>(
  method: string,
  path: string,
  body?: unknown,
): Promise<Answer<T>> {
  try {
    const r = await host().fetch(path, {
      method,
      headers:
        body === undefined ? undefined : { "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const data = (await r.json().catch(() => null)) as T & { error?: string };
    return r.ok
      ? { ok: true, data }
      : {
          ok: false,
          error: data?.error ?? `homeCore said ${r.status}.`,
          status: r.status,
        };
  } catch {
    return { ok: false, error: "homeCore is not answering." };
  }
}

export const loadRules = () => call<Rule[]>("GET", "/automations");
export const setEnabled = (id: string, enabled: boolean) =>
  call<Rule>("PATCH", `/automations/${encodeURIComponent(id)}`, { enabled });
export function createRule(rule: Rule): Promise<Answer<Rule>> {
  const { id: _id, error: _error, ...body } = rule;
  return call<Rule>("POST", "/automations", body);
}
