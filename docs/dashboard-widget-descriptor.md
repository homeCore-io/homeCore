# The widget descriptor — what a plugin may declare, and who can draw it

Core does not close the widget type set. `DashboardWidget.type` is a plain
`String` and `validate_widget_config`'s fallback arm is `_ => Ok(())`, so a
plugin card needs no core release. The cost of that openness is that
`plugin_widget` today names a widget **nothing can enumerate** — core knows the
identity `{plugin_id, widget_id}` and nothing else, so no client can list a
plugin's cards, validate their config, or draw them.

This document specifies what a plugin declares to close that gap, and — the part
that determines whether homeCore gets an ecosystem or one client — **what a
declaration is allowed to say**.

Its companions: `dashboard-vocabulary.json` (what core's own widgets accept) and
`dashboard-layout.md` (what a stored layout means). This is the third contract,
and the only one that is about capability rather than arithmetic.

## The tension this resolves

**"Match Lovelace" and "anyone can write a UI for core" pull in opposite
directions.** A Lovelace custom card is a JavaScript custom element: it works in
the Home Assistant frontend and it cannot work anywhere else. Adopting that
model wholesale would make hc-web the only viable homeCore UI, which is the one
outcome the portability constraint forbids.

The resolution is that a widget declares **two things at once**: a portable
description every client can render, and — optionally — a richer web-only
implementation registered *against* it. A non-web client draws the portable one.
It is not a downgrade path bolted on afterwards; the portable description is the
registration, and code is an attachment to it.

## ⚠ A naming collision to settle first

The word *tier* already means something else in this codebase, and the two
meanings are one function call apart.

`svg_bindings.dart` opens with: *"Tier 2 of the element model. Tier 1 draws
instruments we know how to draw; tier 3 runs code you wrote."* That is a scale
of **authoring effort** inside hc-web. `designer-direction.md` §6.2 uses tier 1
and tier 2 for a scale of **portability**. They do not line up, and one case
crosses them badly:

> The element model's tier 2 — your own SVG, wired to devices by binding — is
> *declarative*, and it is **not portable**. Its own header says the bindings
> "compile to a short script that runs beside the drawing inside the same
> sandbox the code element uses." A drawing plus a script in an iframe is a
> browser feature. `hc-tui` cannot run it.

So declarative does not imply portable, and this contract says **portability
class**, not tier:

| class | means | who renders it |
|---|---|---|
| `portable` | an instrument description, drawn natively by each client | every client, `hc-tui` included |
| `web-only` | SVG-with-bindings, or code, in the sandboxed iframe | hc-web; others fall back |

The element model keeps its own three tiers, unchanged, as a description of how
much work an author is doing. This document never uses the word for anything
else.

## What a plugin registers

Plugins already register device capability schemas over
`homecore/plugins/{id}/register`. Widgets extend that same seam:

```json
{
  "widgets": [
    {
      "widget_id": "boiler_flow",
      "title": "Boiler flow",
      "icon": "water",
      "config_schema": [
        { "name": "device_id", "type": "string", "required": true },
        { "name": "max_lpm", "type": "number", "required": false, "min": 0 }
      ],
      "bindings": [
        { "name": "flow", "device": "{{config.device_id}}", "key": "flow_lpm" }
      ],
      "render": { "kind": "gauge", "value": "flow", "max": "{{config.max_lpm}}" },
      "code": { "entry": "boiler_flow.html", "grant": ["{{config.device_id}}"] }
    }
  ]
}
```

- `config_schema` uses **the field shape `dashboard-vocabulary.json` already
  defines** — `name`, `type`, `required`, `one_of`, `min`, `max`. Not a second
  schema language. The vocabulary endpoint merges core widgets and plugin
  widgets into one list, so a client asks one question to learn every card that
  exists on this installation.
- `bindings` name the readings the widget needs. A binding is resolved by the
  client against device state; core stores it and does not evaluate it.
- `render` is the portable description. **Required.**
- `code` is optional and web-only. A widget with `code` and no `render` is
  rejected at registration — that rejection is the whole portability guarantee,
  and it has to be enforced where the declaration arrives, not where it is drawn.

## The portable render description

The rule that shapes it: **an element kind is an instrument, not markup.** A
client renders `gauge` with whatever it has — CanvasKit arcs, an ANSI meter, a
native progress ring — because it is told *what is being shown*, not what pixels
to set. Markup would make every non-browser client a browser.

hc-web already has most of this as declarative specs, and lifting them into the
vocabulary is the work rather than inventing a format:

| kind | fields | already implemented as |
|---|---|---|
| `gauge` | `shape` (radial\|bar), `start_degrees`, `sweep_degrees`, `thickness`, `round_cap`, `track`, `color`, `color_to`, `readout` (value\|none), `decimals`, `label` | `gauge_spec.dart` — `GaugeSpec` |
| `shape` | `kind` (rectangle\|circle\|pill\|octagon\|path), `corner`, `path` | `primitives.dart` — `ShapeKind`, `shapePath` |
| `text` | `content`, `align` (start\|center\|end), `size_role`, `decimals`, `unit` | `primitives.dart` — `TextAlignChoice` |
| `icon` | `name`, `color` | the icon set core already names |
| `row` / `column` / `stack` | `children`, `gap`, `align` | the container step |

`glow` on a gauge is deliberately **not** in the portable set: it is a
`feGaussianBlur`, the exact thing `code_runtime.dart`'s header says a renderer
without SVG filters draws flat. A field that renders on one client and silently
vanishes on another is worse than a field that does not exist. It stays
available to `code`.

Styling reuses `CardStyle` — `filled`, `bordered`, `titled`, `tint`, `blur`,
`image`, `image_fit`, `image_opacity`, `corner` — for the same reason
`config_schema` reuses the vocabulary field shape.

### Values, and one mapping shape

A number rarely arrives in the units the instrument wants. `SvgBinding` already
solved this — `in_from`, `in_to`, `out_from`, `out_to`, `decimals`, all four or
none, because *"a half-specified mapping is the case where a gauge quietly reads
0–1"* — and that mapping is the portable half of a web-only feature. It is
lifted out of `svg_bindings.dart` and specified once here, so a binding maps the
same way whether it feeds a portable gauge or a sandboxed drawing.

## Declared, then executed

Core validates a plugin widget's config **by running the registered
`config_schema`**, exactly as `validate_widget_config` now runs
`dashboard_vocabulary::catalogue()` rather than restating it. There are no
per-plugin match arms, ever. A field not in the schema is not enforced; a field
enforced is in the schema by construction.

The same applies to `render`: core validates that every element kind and field
appears in the element vocabulary, and nothing about whether the result looks
good. Core remains a document store.

## The z-order constraint

A DOM platform view composited over CanvasKit costs a canvas split per view and
cannot be freely interleaved with canvas-drawn cards. `z` and deliberate overlap
are coming in the model work, so **`code` elements occupy their own layer band**
above the canvas. Decided here rather than discovered later: it is a property of
the descriptor, not of the renderer, because a page that could interleave them
would be authoring a layout no client can honour.

## Conformance

Same three-part shape as the layout contract, and the fixtures win:

| | |
|---|---|
| this file | the rules in prose |
| `hc_types::widget_descriptor` | the reference implementation |
| `docs/dashboard-widget-fixtures.json` | `descriptor → accepted / rejected, with the reason` |

The cases that must be pinned: a descriptor with `code` and no `render` is
rejected; an unknown element kind is rejected; an unknown *field* on a known
kind is rejected; a binding with a half-specified range is rejected; a portable
render using only core element kinds is accepted and round-trips.
