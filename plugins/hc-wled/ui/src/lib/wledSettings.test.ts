import { describe, expect, it } from "vitest";
import {
  emptied,
  ipList,
  ipText,
  timersToSend,
  type Timer,
} from "./wledSettings.js";

const t = (hour: number, macro = 3): Timer => ({
  en: 1,
  hour,
  min: 0,
  macro,
  dow: 127,
});

describe("WLED settings", () => {
  it("sends regular timers before the sunrise and sunset ones", () => {
    const out = timersToSend([t(254), t(7), t(255), t(22)]);
    expect(out.map((x) => x.hour)).toEqual([7, 22, 255, 254]);
  });
  it("removes a timer by emptying it, so WLED drops it", () => {
    expect(emptied(t(7))).toMatchObject({ en: 0, hour: 0, min: 0, macro: 0 });
  });
  it("reads and writes addresses, all zeros being DHCP", () => {
    expect(ipText([192, 168, 1, 74])).toBe("192.168.1.74");
    expect(ipText([0, 0, 0, 0])).toBe("");
    expect(ipList("")).toEqual([0, 0, 0, 0]);
    expect(ipList("192.168.1.80")).toEqual([192, 168, 1, 80]);
    expect(ipList("192.168.1")).toBeNull();
    expect(ipList("192.168.1.300")).toBeNull();
  });
});
