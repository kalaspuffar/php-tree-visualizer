import { nextFocusIndex } from "../../viz/js/keyboard.js";
import { assert_eq, report_done } from "./lib/assert.js";

// Synthetic flat list: depth-0 root child + depth-1 children + a
// depth-2 grandchild. Matches the kind of tree the virtualizer
// hands to nextFocusIndex.
//
//   0: depth 0, expanded, has children
//   1: depth 1, collapsed, has children   <- a child of 0
//   2: depth 1, expanded, has children    <- another child of 0
//   3: depth 2, leaf                       <- a child of 2
//   4: depth 0, leaf                       <- another top-level
const rows = [
    { hasChildren: true,  expanded: true,  indentDepthForUi: 0 },
    { hasChildren: true,  expanded: false, indentDepthForUi: 1 },
    { hasChildren: true,  expanded: true,  indentDepthForUi: 1 },
    { hasChildren: false, expanded: false, indentDepthForUi: 2 },
    { hasChildren: false, expanded: false, indentDepthForUi: 0 },
];

function call(currentIndex, key, modifiers = {}) {
    return nextFocusIndex({ rows, currentIndex, key, ...modifiers });
}

// ---- ArrowDown / ArrowUp ------------------------------------------

assert_eq(
    { newIndex: 1, expand: false, collapse: false, consumed: true },
    call(0, "ArrowDown"),
    "ArrowDown moves forward"
);
assert_eq(
    { newIndex: 4, expand: false, collapse: false, consumed: true },
    call(4, "ArrowDown"),
    "ArrowDown clamps at end"
);
assert_eq(
    { newIndex: 2, expand: false, collapse: false, consumed: true },
    call(3, "ArrowUp"),
    "ArrowUp moves backward"
);
assert_eq(
    { newIndex: 0, expand: false, collapse: false, consumed: true },
    call(0, "ArrowUp"),
    "ArrowUp clamps at 0"
);

// ---- Home / End ----------------------------------------------------

assert_eq(
    { newIndex: 0, expand: false, collapse: false, consumed: true },
    call(3, "Home"),
    "Home -> 0"
);
assert_eq(
    { newIndex: 4, expand: false, collapse: false, consumed: true },
    call(0, "End"),
    "End -> last"
);

// ---- PageUp / PageDown (step 10) ----------------------------------

assert_eq(
    { newIndex: 4, expand: false, collapse: false, consumed: true },
    call(0, "PageDown"),
    "PageDown clamps at end with only 5 rows"
);
assert_eq(
    { newIndex: 0, expand: false, collapse: false, consumed: true },
    call(4, "PageUp"),
    "PageUp clamps at 0"
);

// ---- ArrowRight ----------------------------------------------------

// Index 0 is expanded: focuses first child (= index 1).
assert_eq(
    { newIndex: 1, expand: false, collapse: false, consumed: true },
    call(0, "ArrowRight"),
    "ArrowRight on expanded -> first child"
);

// Index 1 is collapsed with children: signals expand.
assert_eq(
    { newIndex: 1, expand: true, collapse: false, consumed: true },
    call(1, "ArrowRight"),
    "ArrowRight on collapsed-with-children -> expand"
);

// Index 3 is a leaf: no-op but consumed.
assert_eq(
    { newIndex: 3, expand: false, collapse: false, consumed: true },
    call(3, "ArrowRight"),
    "ArrowRight on leaf -> no-op consumed"
);

// ---- ArrowLeft ----------------------------------------------------

// Index 2 is expanded: collapse.
assert_eq(
    { newIndex: 2, expand: false, collapse: true, consumed: true },
    call(2, "ArrowLeft"),
    "ArrowLeft on expanded -> collapse"
);

// Index 1 is collapsed: focus parent (= index 0).
assert_eq(
    { newIndex: 0, expand: false, collapse: false, consumed: true },
    call(1, "ArrowLeft"),
    "ArrowLeft on collapsed -> parent"
);

// Index 3 (leaf at depth 2): parent is index 2.
assert_eq(
    { newIndex: 2, expand: false, collapse: false, consumed: true },
    call(3, "ArrowLeft"),
    "ArrowLeft on leaf -> parent (walks shallower depth)"
);

// Index 4 is a top-level leaf: no parent. No-op consumed.
assert_eq(
    { newIndex: 4, expand: false, collapse: false, consumed: true },
    call(4, "ArrowLeft"),
    "ArrowLeft on top-level leaf -> no parent, no-op consumed"
);

// ---- Enter is reserved no-op (consumed) --------------------------

assert_eq(
    { newIndex: 2, expand: false, collapse: false, consumed: true },
    call(2, "Enter"),
    "Enter -> consumed no-op"
);

// ---- Unknown keys: not consumed ----------------------------------

assert_eq(
    { newIndex: 2, expand: false, collapse: false, consumed: false },
    call(2, "a"),
    "letter key -> not consumed"
);

// ---- Modifier keys: not consumed ---------------------------------

assert_eq(
    { newIndex: 2, expand: false, collapse: false, consumed: false },
    call(2, "ArrowDown", { ctrlKey: true }),
    "Ctrl+ArrowDown -> not consumed (browser scroll keystrokes)"
);
assert_eq(
    { newIndex: 2, expand: false, collapse: false, consumed: false },
    call(2, "ArrowDown", { metaKey: true }),
    "Meta+ArrowDown -> not consumed"
);

// ---- Empty list ---------------------------------------------------

assert_eq(
    { newIndex: 0, expand: false, collapse: false, consumed: false },
    nextFocusIndex({ rows: [], currentIndex: 0, key: "ArrowDown" }),
    "empty list -> no consume"
);

// ---- Out-of-range currentIndex clamps -----------------------------

assert_eq(
    { newIndex: 4, expand: false, collapse: false, consumed: true },
    call(99, "ArrowDown"),
    "stale currentIndex high -> clamps + moves"
);
assert_eq(
    { newIndex: 1, expand: false, collapse: false, consumed: true },
    call(-5, "ArrowDown"),
    "stale currentIndex negative -> treated as 0, then +1"
);

report_done();
