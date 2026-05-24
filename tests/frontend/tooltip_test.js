import { computeTooltipPosition } from "../../viz/js/tooltip.js";
import { assert_eq, report_done } from "./lib/assert.js";

// rect.top + rect.bottom only — the function ignores left/right.
// popperHeight is the rendered tooltip height (32 px is the
// documented default in design.md D-7 sketch).

const POPPER = 32;

// Element well below the top → place above.
{
    const out = computeTooltipPosition({
        rect: { top: 200, bottom: 220 },
        viewportHeight: 800,
        popperHeight: POPPER,
    });
    assert_eq("above", out.placement, "rect far from top -> above");
    // top = 200 - 32 - 8 = 160
    assert_eq(160, out.top, "above placement top");
}

// Element near the viewport top → flip below.
{
    const out = computeTooltipPosition({
        rect: { top: 4, bottom: 24 },
        viewportHeight: 800,
        popperHeight: POPPER,
    });
    assert_eq("below", out.placement, "rect near top -> below");
    // top = bottom + 8 = 32
    assert_eq(32, out.top, "below placement top");
}

// Element at the boundary: aboveY = 8 is the minimum (>=8 keeps
// above; <8 flips below).
{
    // top = 48 → aboveY = 48 - 32 - 8 = 8 (boundary)
    const above = computeTooltipPosition({
        rect: { top: 48, bottom: 68 },
        viewportHeight: 800,
        popperHeight: POPPER,
    });
    assert_eq("above", above.placement, "boundary above (aboveY=8) -> above");

    // top = 47 → aboveY = 7 -> below
    const below = computeTooltipPosition({
        rect: { top: 47, bottom: 67 },
        viewportHeight: 800,
        popperHeight: POPPER,
    });
    assert_eq("below", below.placement, "boundary below (aboveY=7) -> below");
}

// Element at the very bottom of the viewport: still above
// (we don't worry about clipping the bottom; the popper drops
// below outside the viewport, which is acceptable).
{
    const out = computeTooltipPosition({
        rect: { top: 780, bottom: 800 },
        viewportHeight: 800,
        popperHeight: POPPER,
    });
    assert_eq("above", out.placement, "rect at viewport bottom -> above");
}

// Variable popper heights: a tall popper near the top flips below
// even when the element is moderately far from the top.
{
    const tallOut = computeTooltipPosition({
        rect: { top: 30, bottom: 50 },
        viewportHeight: 800,
        popperHeight: 60,
    });
    // aboveY = 30 - 60 - 8 = -38 -> flip
    assert_eq("below", tallOut.placement, "tall popper near top -> below");
}

report_done();
