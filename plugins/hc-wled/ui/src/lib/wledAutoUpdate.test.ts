import { describe, expect, it } from "vitest";
import {
  AUTO_UPDATE_TAG,
  autoUpdateRule,
  findAutoUpdate,
} from "./wledAutoUpdate.js";

describe("install WLED updates at night", () => {
  it("is an automation at 03:00 that presses each waiting update and waits for it", () => {
    const r = autoUpdateRule();
    expect(r.trigger).toEqual({ TimeOfDay: { time: "03:00:00", days: [] } });
    const each = r.actions![0]!.action.ForEachAlert;
    expect(each.filter).toEqual({
      plugin_id: "plugin.wled",
      topic: "firmware_update",
    });
    expect(each.actions[0]).toEqual({ PressAlert: {} });
    expect(each.actions[1].WaitAlertCleared.timeout_secs).toBe(600);
    expect(each.actions[1].WaitAlertCleared.otherwise.at(-1)).toBe("ExitRule");
  });
  it("is found again by its tag", () => {
    const mine = { ...autoUpdateRule(), id: "x" };
    expect(findAutoUpdate([{ ...mine, id: "y", tags: [] }, mine])?.id).toBe(
      "x",
    );
    expect(findAutoUpdate([])).toBeUndefined();
    expect(mine.tags).toContain(AUTO_UPDATE_TAG);
  });
});
