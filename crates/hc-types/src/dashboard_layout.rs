//! What a dashboard layout *means* — the reference implementation.
//!
//! Core stores `flow`, `frame`, `groups` and `rect` and deliberately never acts
//! on them: it is a document store, not a layout engine. That split is right,
//! but it left the *semantics* living in exactly one place — hc-web's
//! `lib/core/dashboard/grid_engine.dart`. Another client reading `flow: free`
//! has to reimplement gravity, packing and the overlap rule from scratch and
//! hope it agrees. The document was portable; its interpretation was not.
//!
//! So this module is the executable spec, the way
//! [`crate::dashboard_vocabulary`] is for config. Prose in `docs/` describes it;
//! `docs/dashboard-layout-fixtures.json` pins it case by case; and any client
//! that can read JSON can check itself against the same fixtures rather than
//! against its own belief about what packing means.
//!
//! # Scope: the save contract, not the editor
//!
//! Only the operations that decide whether two clients agree about a stored
//! document are here — [`Engine::normalize`], [`Engine::is_legal`] and
//! [`Engine::rows`]. `move`, `resize`, `add` and marquee selection are editor
//! *interactions*: how a card follows a cursor is a client's business and two
//! clients may reasonably differ. What a saved document means is not.
//!
//! `normalize` is the one that matters most. Core's validator rejects the whole
//! dashboard on the first bad placement, so a client that normalises
//! differently does not merely draw a page differently — it loses the user's
//! edit.
//!
//! # Ported from the Dart, faithfully
//!
//! Including where the Dart is surprising. See [`Engine::normalize`] on
//! composed elements under `packed`: the behaviour is asymmetric, it is
//! reproduced here on purpose, and a reference that quietly disagreed with the
//! shipped client would be worse than none.

use serde::{Deserialize, Serialize};

use crate::dashboard::{DashboardFlow, DashboardRect};

/// One card's box, in grid cells.
///
/// A subset of hc-web's `GridItem`: `minW`/`minH` govern resizing and `z`
/// governs paint order, neither of which changes what a stored layout means.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridItem {
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,

    /// Sections partition a layout; items never collide across them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section_id: Option<String>,

    /// Sits *above* the grid rather than in it: it keeps cell geometry, but
    /// nothing pushes it, it pushes nothing, and gravity does not pull it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub floating: bool,

    /// Where it really sits, when the layout is composed. The cells are then a
    /// snapped approximation of this — and the fallback for a client that has
    /// never heard of frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<DashboardRect>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl GridItem {
    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    /// Placed, not packed: somebody put this at a point on a canvas.
    pub fn is_composed(&self) -> bool {
        self.rect.is_some()
    }

    /// Do these two compete for the same cells?
    ///
    /// **Overlapping is not the same as competing**, and that distinction is
    /// the whole of the free layer. Two grid items in one cell is a layout to
    /// be resolved; a floating element over a grid item is a design. So a
    /// floating *or composed* element on either side answers no, and every
    /// other rule inherits that from here rather than restating it.
    pub fn overlaps(&self, other: &GridItem) -> bool {
        !self.floating
            && !other.floating
            && !self.is_composed()
            && !other.is_composed()
            && self.section_id == other.section_id
            && self.id != other.id
            && self.x < other.right()
            && self.right() > other.x
            && self.y < other.bottom()
            && self.bottom() > other.y
    }
}

/// The layout engine for one breakpoint's layout.
#[derive(Debug, Clone, Copy)]
pub struct Engine {
    /// Core validates `x + w <= columns` and rejects the whole dashboard
    /// otherwise, so this is a hard bound rather than a preference.
    pub columns: i32,
    pub flow: DashboardFlow,
}

impl Engine {
    pub fn new(columns: i32, flow: DashboardFlow) -> Self {
        Self { columns, flow }
    }

    /// Makes an arbitrary layout legal: clamps every item inside the grid, then
    /// removes every overlap. Run before every save.
    ///
    /// 1. **Clamp.** `w` into `1..=columns`, `h` to at least 1, `x` so the card
    ///    ends inside the grid, `y` to at least 0.
    /// 2. **Reading order.** Re-place by `(y, x)`, so a corrupt layout resolves
    ///    predictably rather than according to whatever order the JSON happened
    ///    to be in.
    /// 3. **Push down.** Each item drops one row at a time until it competes
    ///    with nothing already placed. A floating element skips this entirely —
    ///    the position is the design.
    /// 4. **Settle.** Under [`DashboardFlow::Packed`], gravity pulls everything
    ///    back up into the gaps. Under [`DashboardFlow::Free`], gaps are content
    ///    and nothing moves.
    ///
    /// # The composed-under-packed asymmetry
    ///
    /// Step 3 skips floating elements explicitly, and skips composed ones
    /// implicitly because [`GridItem::overlaps`] answers no for them. Gravity
    /// (step 4) skips only *floating*. A composed element therefore competes
    /// with nothing, never breaks gravity's rise loop, and is pulled to `y = 0`.
    ///
    /// This is what the Dart does today and is reproduced deliberately. It is
    /// usually invisible — a composed element is drawn from its `rect`, and the
    /// cells are only an approximation — but the cells are also what core
    /// validates and what a frame-unaware client draws, so it is not harmless.
    /// Changing it is a decision about the document, not a tidy-up, and it
    /// belongs in a commit that says so.
    pub fn normalize(&self, items: &[GridItem]) -> Vec<GridItem> {
        let mut clamped: Vec<GridItem> = items
            .iter()
            .map(|i| {
                let w = i.w.clamp(1, self.columns.max(1));
                GridItem {
                    w,
                    h: i.h.max(1),
                    x: i.x.clamp(0, (self.columns - w).max(0)),
                    y: i.y.max(0),
                    ..i.clone()
                }
            })
            .collect();

        clamped.sort_by(reading_order);

        let mut out: Vec<GridItem> = Vec::with_capacity(clamped.len());
        for item in clamped {
            if item.floating {
                out.push(item);
                continue;
            }
            let mut placed = item;
            while out.iter().any(|o| placed.overlaps(o)) {
                placed.y += 1;
            }
            out.push(placed);
        }

        self.settle(out)
    }

    /// Gravity, or not — the single place the two flows differ.
    ///
    /// Everything else (clamping, overlap resolution, the column bound) is
    /// identical, because a gap being content does not make an overlap
    /// acceptable.
    fn settle(&self, items: Vec<GridItem>) -> Vec<GridItem> {
        match self.flow {
            DashboardFlow::Packed => self.gravity(items),
            DashboardFlow::Free => items,
        }
    }

    /// Pulls every item as far up as it will go, in reading order.
    fn gravity(&self, items: Vec<GridItem>) -> Vec<GridItem> {
        let mut ordered = items;
        ordered.sort_by(reading_order);

        let mut out: Vec<GridItem> = Vec::with_capacity(ordered.len());
        for item in ordered {
            // A floating element is where it was put. Gravity would pull it to
            // the top of the page, since nothing below it can block something
            // that competes with nothing.
            if item.floating {
                out.push(item);
                continue;
            }
            let mut placed = item;
            while placed.y > 0 {
                let mut up = placed.clone();
                up.y -= 1;
                if out.iter().any(|o| up.overlaps(o)) {
                    break;
                }
                placed = up;
            }
            out.push(placed);
        }

        out
    }

    /// Whether core would accept this layout as-is.
    ///
    /// The bounds core checks, plus the overlap rule it does not: core validates
    /// placement arithmetic, and a client that saved overlapping cards would get
    /// a document every *other* client draws differently.
    pub fn is_legal(&self, items: &[GridItem]) -> bool {
        for i in items {
            if i.x < 0 || i.y < 0 || i.w < 1 || i.h < 1 {
                return false;
            }
            if i.right() > self.columns {
                return false;
            }
            if items.iter().any(|o| i.overlaps(o)) {
                return false;
            }
        }
        true
    }

    /// The grid's height in rows — what a canvas must be tall enough to show.
    pub fn rows(&self, items: &[GridItem]) -> i32 {
        items.iter().fold(0, |max, i| max.max(i.bottom()))
    }
}

/// `(y, x)` — the order somebody reads a page in.
fn reading_order(a: &GridItem, b: &GridItem) -> std::cmp::Ordering {
    a.y.cmp(&b.y).then(a.x.cmp(&b.x))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, x: i32, y: i32, w: i32, h: i32) -> GridItem {
        GridItem {
            id: id.to_string(),
            x,
            y,
            w,
            h,
            section_id: None,
            floating: false,
            rect: None,
        }
    }

    fn packed(columns: i32) -> Engine {
        Engine::new(columns, DashboardFlow::Packed)
    }

    fn free(columns: i32) -> Engine {
        Engine::new(columns, DashboardFlow::Free)
    }

    fn at(out: &[GridItem], id: &str) -> (i32, i32) {
        let i = out.iter().find(|i| i.id == id).expect("missing item");
        (i.x, i.y)
    }

    #[test]
    fn packed_closes_a_gap_and_free_keeps_it() {
        let items = vec![item("a", 0, 0, 2, 1), item("b", 0, 5, 2, 1)];

        assert_eq!(at(&packed(12).normalize(&items), "b"), (0, 1));
        assert_eq!(at(&free(12).normalize(&items), "b"), (0, 5));
    }

    #[test]
    fn an_overlap_is_resolved_under_both_flows() {
        // A gap being content does not make an overlap acceptable.
        let items = vec![item("a", 0, 0, 2, 2), item("b", 1, 1, 2, 2)];
        for engine in [packed(12), free(12)] {
            let out = engine.normalize(&items);
            assert!(engine.is_legal(&out), "{:?}", engine.flow);
        }
    }

    #[test]
    fn a_card_wider_than_the_grid_is_clamped_not_rejected() {
        let out = packed(4).normalize(&[item("a", 3, 0, 99, 1)]);
        assert_eq!(out[0].w, 4);
        assert_eq!(out[0].x, 0);
    }

    #[test]
    fn negative_coordinates_come_back_inside() {
        let out = packed(12).normalize(&[item("a", -5, -5, 2, 1)]);
        assert_eq!(at(&out, "a"), (0, 0));
    }

    #[test]
    fn items_never_collide_across_sections() {
        let mut a = item("a", 0, 0, 2, 1);
        let mut b = item("b", 0, 0, 2, 1);
        a.section_id = Some("top".into());
        b.section_id = Some("bottom".into());
        // Same cell, different sections: legal, and normalize leaves both.
        assert!(!a.overlaps(&b));
        let out = free(12).normalize(&[a, b]);
        assert_eq!(at(&out, "a"), (0, 0));
        assert_eq!(at(&out, "b"), (0, 0));
    }

    #[test]
    fn a_floating_element_is_neither_pushed_nor_pulled() {
        let mut floater = item("float", 0, 4, 2, 1);
        floater.floating = true;
        let out = packed(12).normalize(&[item("a", 0, 0, 2, 1), floater]);
        // Gravity closed nothing under it and nothing pushed it aside.
        assert_eq!(at(&out, "float"), (0, 4));
    }

    #[test]
    fn a_composed_element_rises_under_packed() {
        // Documented, not endorsed — see `normalize`. Gravity skips only
        // floating, and a composed element competes with nothing, so it never
        // breaks the rise loop. Pinned here so a change to it is a decision
        // somebody made rather than one that slipped through.
        let mut composed = item("c", 0, 6, 2, 1);
        composed.rect = Some(DashboardRect {
            x: 10.0,
            y: 200.0,
            w: 100.0,
            h: 40.0,
        });
        let out = packed(12).normalize(&[composed.clone()]);
        assert_eq!(at(&out, "c"), (0, 0), "composed element did not rise");

        // Under `free` it stays where it was put, which is the flow a composed
        // layout actually uses.
        let out = free(12).normalize(&[composed]);
        assert_eq!(at(&out, "c"), (0, 6));
    }

    #[test]
    fn reading_order_decides_who_wins_a_contested_cell() {
        // Same cell, and the JSON order is deliberately the reverse of reading
        // order: the higher-then-lefter card keeps the spot regardless.
        let out = free(12).normalize(&[item("later", 4, 0, 2, 1), item("first", 0, 0, 2, 1)]);
        assert_eq!(at(&out, "first"), (0, 0));
        assert_eq!(at(&out, "later"), (4, 0));
    }

    #[test]
    fn rows_is_the_lowest_bottom_edge() {
        let e = free(12);
        assert_eq!(e.rows(&[item("a", 0, 0, 1, 2), item("b", 2, 3, 1, 1)]), 4);
        assert_eq!(e.rows(&[]), 0);
    }

    #[test]
    fn normalize_is_idempotent() {
        // The property that makes it safe to run before every save: normalising
        // an already-normal layout must change nothing, or a document would
        // drift a little on each round trip.
        let items = vec![
            item("a", 0, 0, 3, 2),
            item("b", 3, 0, 2, 1),
            item("c", 0, 7, 4, 1),
        ];
        for engine in [packed(12), free(12)] {
            let once = engine.normalize(&items);
            let twice = engine.normalize(&once);
            assert_eq!(once, twice, "{:?} is not idempotent", engine.flow);
        }
    }
}
