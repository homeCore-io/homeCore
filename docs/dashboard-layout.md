# Dashboard layout — what a stored document means

Core stores `flow`, `frame`, `groups` and `rect` and **never acts on them**. It
is a document store, not a layout engine, and that split is deliberate: core has
no opinion about where a card goes, only about whether the placement arithmetic
is legal.

The cost of that split is what this document exists to pay. Until now the
*semantics* lived in exactly one place — hc-web's
`lib/core/dashboard/grid_engine.dart` — so another client reading `flow: free`
had to reimplement gravity, packing and the overlap rule from scratch and hope
it agreed. The document was portable; its interpretation was not.

Three things now describe it, in increasing order of authority:

| | |
|---|---|
| This file | the rules in prose |
| `hc_types::dashboard_layout` | the reference implementation |
| `docs/dashboard-layout-fixtures.json` | the cases every client must reproduce |

Where they disagree, the fixtures win — they are generated from the reference,
and a snapshot test fails the build if the two drift apart.

## Why it matters more than it looks

`normalize` runs before every save, and **core rejects the entire dashboard on
the first bad placement**. A client that normalises differently does not draw a
page differently — it loses the user's edit, all of it, including the parts that
were fine. That is why this is pinned case by case rather than described and
trusted.

## The model

A layout is a list of placements in **grid cells**, one layout per breakpoint:

- `x`, `y`, `w`, `h` — integers, in cells. `columns` comes from the layout.
- `section_id` — sections partition a layout; items never collide across them.
- `floating` — sits *above* the grid rather than in it.
- `rect` — where the element really sits, when the layout is composed. The cells
  are then a **snapped approximation** of it.

`rect` is the one that surprises people. A composed page still opens, still
legally, in software that has never heard of frames — approximately right rather
than broken. That is the only version of composition that is safe to ship to
documents already in redb, and it means **the cells are never decorative**: they
are what core validates and what a frame-unaware client draws.

## The overlap rule

Everything else inherits from this one predicate. Two elements compete when:

```
neither is floating
  AND neither is composed (rect is set)
  AND they share a section_id
  AND they are not the same element
  AND their rectangles intersect
```

**Overlapping is not the same as competing**, and the distinction is the whole
of the free layer. Two grid items in one cell is a layout to be resolved; a
floating element over a grid item is a design. A composed element answers *no*
for the same reason a floating one does — it was put where it is, and packing a
composition would be the engine overruling the design.

## `normalize`

Makes an arbitrary layout legal. Four steps, in order:

1. **Clamp.** `w` into `1..=columns`; `h` to at least 1; `x` so the card ends
   inside the grid; `y` to at least 0. A card wider than the grid is trimmed,
   never rejected — core would refuse the whole document and the user would lose
   everything.
2. **Reading order.** Sort by `(y, x)` and re-place in that order, so a corrupt
   layout resolves predictably rather than according to whatever order the JSON
   happened to be in.
3. **Push down.** Each element drops one row at a time until it competes with
   nothing already placed. A floating element skips this entirely.
4. **Settle.** Under `packed`, gravity pulls everything back up into the gaps, in
   reading order. Under `free`, nothing moves.

`normalize` is **idempotent**: normalising an already-normal layout changes
nothing. A document must not drift a little on each round trip.

## `flow`

The single place the two flows differ is step 4. Clamping, overlap resolution
and the column bound are identical under both, because **a gap being content
does not make an overlap acceptable**.

- `packed` — cards float up to close gaps. The default, and what every dashboard
  authored before the field existed meant. A gap could not be expressed at all
  then, so reading absence as `packed` is not a guess.
- `free` — cards stay where they were put. Gaps are content.

## `is_legal` and `rows`

`is_legal` is what core would accept, plus the overlap rule core does not check:
non-negative `x`/`y`, `w`/`h` of at least 1, `x + w <= columns`, and no competing
pair. `rows` is the largest `y + h` — how tall a canvas has to be.

## Known asymmetry: a composed element rises under `packed`

Step 3 skips floating elements *explicitly*, and composed ones *implicitly*
because the overlap rule answers no for them. Gravity in step 4 skips only
floating. So a composed element competes with nothing, never breaks gravity's
rise loop, and lands at `y = 0`.

This is what hc-web does today. The reference implementation reproduces it
deliberately, and `docs/dashboard-layout-fixtures.json` pins it, under a case
whose `why` says *documented, not endorsed*.

It is usually invisible — a composed element is drawn from its `rect`, so the
cells are only the approximation — and it is not harmless, because the
approximation is what core validates and what a frame-unaware client draws. In
practice composed layouts use `free`, where it does not arise.

**Changing it is a decision about the document, not a tidy-up.** Fixing the
asymmetry means regenerating the fixtures and updating every client in step. Do
it in a commit that says so, not as a drive-by.

## Conforming

Read `docs/dashboard-layout-fixtures.json`. Each case is:

```json
{
  "name": "packed closes a gap",
  "why": "Under `packed`, gravity pulls every card up into the space above it...",
  "columns": 12,
  "flow": "packed",
  "input":    [ { "id": "a", "x": 0, "y": 0, "w": 2, "h": 1 }, … ],
  "expected": [ { "id": "a", "x": 0, "y": 0, "w": 2, "h": 1 }, … ]
}
```

Run your engine's `normalize` over `input` with that `columns` and `flow`, and
compare against `expected` as a set keyed by `id`. Order in the array is not
significant; positions are.

`why` is in the artifact on purpose: a client author reading a failure needs to
know which rule broke, not just that two numbers differ.

Regenerate after changing the reference:

```
UPDATE_LAYOUT_FIXTURES=1 cargo test -p hc-types
```

## What is deliberately not specified

**Editor interactions.** `move`, `resize`, `add`, drop placement and marquee
selection are a client's business — how a card follows a cursor is a design
question, and two clients may reasonably differ. What a *saved document* means
is not a design question.

**Rendering.** Cell size, gap, typography, chrome. `row_height` and `gap` are on
the layout because they change what fits, not because core has a view about
pixels.

**Derivation between breakpoints.** Which layout is authored and which is
computed is recorded in `derived_from`, and core never acts on it. How a desktop
layout becomes a mobile one is an open question and not settled here.
