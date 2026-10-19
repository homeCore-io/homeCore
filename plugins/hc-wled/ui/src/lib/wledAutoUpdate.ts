/**
 * "Install WLED updates by itself, at night": an ordinary automation the
 * app writes, which anyone can open, change or switch off. It is found again
 * by its tag, so turning the choice off and on reuses the one there is.
 *
 * It acts on the "Firmware update available" alert, so turning it on turns
 * that alert on too — an alert that's off is never raised.
 */
import type { Rule } from "./ruleWords.js";
import { WLED } from "./wled.js";

export const AUTO_UPDATE_TAG = "wled-auto-update";
const FILTER = { plugin_id: WLED, topic: "firmware_update" };

/** The automation: at 03:00, each waiting update in turn; stop at the first that doesn't take. */
export function autoUpdateRule(): Rule {
  return {
    id: "",
    name: "Install WLED updates at night",
    enabled: true,
    priority: 0,
    tags: [AUTO_UPDATE_TAG],
    trigger: { TimeOfDay: { time: "03:00:00", days: [] } },
    conditions: [{ AlertActive: { filter: FILTER } }],
    actions: [
      {
        enabled: true,
        action: {
          ForEachAlert: {
            filter: FILTER,
            actions: [
              { PressAlert: {} },
              {
                WaitAlertCleared: {
                  timeout_secs: 600,
                  otherwise: [
                    {
                      Notify: {
                        channel: "all",
                        message:
                          "{alert.device} didn’t come back on WLED {alert.key}; the rest wait.",
                        title: null,
                      },
                    },
                    "ExitRule",
                  ],
                },
              },
            ],
          },
        },
      },
    ],
  };
}

export const findAutoUpdate = (rules: Rule[]): Rule | undefined =>
  rules.find((r) => (r.tags ?? []).includes(AUTO_UPDATE_TAG));
