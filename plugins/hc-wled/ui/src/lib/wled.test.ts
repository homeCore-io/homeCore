import { describe, expect, it } from "vitest";
import {
  effectCommand,
  findEffects,
  hex,
  paletteCss,
  presetCss,
  presetOn,
  rgb,
  segmentCss,
  stepOf,
  stepSentence,
  stepState,
  type Catalog,
  type Effect,
  type Segment,
  type WledStep,
} from "./wled.js";
import type { CoreDevice } from "./devices.js";

const fairy: Effect = {
  id: 51,
  name: "Fairytwinkle",
  sliders: [
    { key: "effect_speed", label: "Speed", max: 255 },
    { key: "effect_intensity", label: "Intensity", max: 255 },
  ],
  options: [],
  colors: [
    { slot: 0, label: "Primary" },
    { slot: 1, label: "Background" },
  ],
  palette: "Palette",
  one_d: true,
  two_d: false,
  single_pixel: false,
  sound: false,
  defaults: { mapping_1d2d: 0 },
};
const rainbow: Effect = {
  ...fairy,
  id: 9,
  name: "Rainbow",
  colors: [],
  defaults: {},
};
const solid: Effect = {
  ...fairy,
  id: 0,
  name: "Solid",
  palette: null,
  defaults: {},
};
const pixels: Effect = {
  ...fairy,
  id: 128,
  name: "Pixels",
  sound: true,
  defaults: { sound_sim: 0 },
};

const seg = (over: Partial<Segment> = {}): Segment => ({
  id: 0,
  name: null,
  start: 0,
  stop: 400,
  grouping: 1,
  spacing: 0,
  offset: 0,
  on: true,
  brightness: 255,
  brightness_pct: 100,
  cct: 127,
  colors: ["#6a0dad", "#ff4500", "#00ff3c"],
  effect_id: 51,
  effect_speed: 128,
  effect_intensity: 128,
  custom1: 128,
  custom2: 128,
  custom3: 16,
  option1: false,
  option2: false,
  option3: false,
  palette_id: 0,
  selected: true,
  reverse: false,
  mirror: false,
  freeze: false,
  blend_mode: 0,
  ...over,
});

describe("WLED palettes and looks, drawn", () => {
  it("a gradient palette as its stops; a colour palette in the segment’s colours", () => {
    expect(
      paletteCss(
        {
          id: 9,
          name: "Ocean",
          stops: [
            { at: 0, color: "#191970" },
            { at: 1, color: "#00ffff" },
          ],
        },
        [],
      ),
    ).toBe("linear-gradient(90deg, #191970 0%, #00ffff 100%)");
    expect(
      paletteCss(
        { id: 3, name: "* Colors 1&2", stops: [{ slot: 0 }, { slot: 1 }] },
        ["#ff0000", "#0000ff"],
      ),
    ).toBe("linear-gradient(90deg, #ff0000 0%, #0000ff 100%)");
    expect(
      paletteCss({ id: 2, name: "* Color 1", stops: [{ slot: 0 }] }, [
        "#ff0000",
      ]),
    ).toBe("#ff0000");
  });

  it("a segment shows its palette when its effect uses one, else its colours", () => {
    const catalog: Catalog = {
      firmware: "16.0.1",
      fetched_at: "",
      effects: [fairy, solid],
      palettes: [
        {
          id: 0,
          name: "Default",
          stops: [
            { at: 0, color: "#9b00d5" },
            { at: 1, color: "#0032fc" },
          ],
        },
      ],
    };
    expect(segmentCss(seg(), catalog)).toContain("#9b00d5");
    expect(
      segmentCss(
        seg({ effect_id: 0, colors: ["#ff7f00", "#000000", "#000000"] }),
        catalog,
      ),
    ).toBe("#ff7f00");
    expect(
      presetCss({
        id: 14,
        name: "Halloween 1",
        kind: "look",
        colors: ["#6a0dad", "#ff4500"],
      }),
    ).toBe("linear-gradient(90deg, #6a0dad 0% 50%, #ff4500 50% 100%)");
  });

  it("colours go both ways", () => {
    expect(rgb("#6a0dad")).toEqual([106, 13, 173]);
    expect(hex([106, 13, 173])).toBe("#6a0dad");
    expect(hex("#ffffff")).toBe("#ffffff");
  });
});

describe("finding an effect", () => {
  const all = [solid, rainbow, fairy, pixels];
  it("by words, and by what it uses", () => {
    expect(findEffects(all, "fairy", "all").map((e) => e.id)).toEqual([51]);
    expect(findEffects(all, "", "palette").map((e) => e.id)).toEqual([
      9, 51, 128,
    ]);
    expect(findEffects(all, "", "sound").map((e) => e.id)).toEqual([128]);
    expect(
      findEffects(all, "", "favourites", new Set([9])).map((e) => e.id),
    ).toEqual([9]);
  });
  it("choosing one starts it at its own settings", () => {
    expect(effectCommand("all", pixels)).toEqual({
      segment: "all",
      effect_id: 128,
      sound_sim: 0,
    });
  });
});

describe("an automation’s WLED step", () => {
  const round = (s: WledStep) => stepOf(stepState(s));
  it("every kind round-trips through the command it sends", () => {
    const steps: WledStep[] = [
      { kind: "preset", preset_id: 14, transition: 2000 },
      { kind: "playlist", playlist_id: 20 },
      {
        kind: "look",
        segment: "all",
        look: { effect_id: 51, palette_id: 0, color: [106, 13, 173] },
      },
      {
        kind: "nightlight",
        nightlight: {
          on: true,
          duration_min: 30,
          mode: "sunrise",
          target_pct: 0,
        },
      },
      { kind: "power", power: false },
      { kind: "sync", sync: { send: false, receive: true } },
    ];
    for (const s of steps) expect(round(s)).toEqual(s);
  });
  it("a step written by hand reads back too, in the old words", () => {
    expect(stepOf({ preset: 3 })).toEqual({ kind: "preset", preset_id: 3 });
    expect(stepOf({ action: "apply_preset", apply_preset: 5 })).toEqual({
      kind: "preset",
      preset_id: 5,
    });
    expect(stepOf({ effect: 9, palette: 1 })).toEqual({
      kind: "look",
      segment: 0,
      look: { effect_id: 9, palette_id: 1 },
    });
  });
  it("what no step says stays JSON", () => {
    expect(stepOf({ raw: { lor: 2 } })).toBeNull();
    expect(stepOf({ effect_id: 2, on: true })).toBeNull();
    expect(stepOf("not an object")).toBeNull();
  });
  it("reads as a sentence, with names", () => {
    const names = {
      preset: (id: number) => (id === 14 ? "Halloween 1" : undefined),
      effect: () => "Fairytwinkle",
    };
    expect(
      stepSentence(
        { kind: "preset", preset_id: 14, transition: 2000 },
        "Eaves lights",
        names,
      ),
    ).toBe("Apply Halloween 1 on Eaves lights, fading over 2 s");
    expect(
      stepSentence(
        { kind: "look", segment: "all", look: { effect_id: 51 } },
        "Eaves lights",
        names,
      ),
    ).toBe("Set Eaves lights to Fairytwinkle");
    expect(
      stepSentence(
        {
          kind: "nightlight",
          nightlight: {
            on: true,
            duration_min: 30,
            mode: "fade",
            target_pct: 0,
          },
        },
        "Eaves lights",
      ),
    ).toBe("Start Eaves lights's nightlight: fade to off over 30 min");
  });
  it("a preset on another controller is found by name", () => {
    const dev = (id: string, presets: { id: number; name: string }[]) =>
      ({
        device_id: id,
        name: id,
        available: true,
        attributes: { presets },
      }) as unknown as CoreDevice;
    const a = dev("a", [{ id: 14, name: "Halloween 1" }]);
    const b = dev("b", [{ id: 3, name: "Halloween 1" }]);
    const c = dev("c", [{ id: 14, name: "Other" }]);
    expect(presetOn(b, a, 14)).toBe(3);
    expect(presetOn(c, a, 14)).toBeUndefined();
  });
});
