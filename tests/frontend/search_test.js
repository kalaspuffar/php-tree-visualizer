import { findMatches, highlightFqn } from "../../viz/js/search.js";
import { assert_eq, assert_true, report_done } from "./lib/assert.js";

// ---- findMatches ---------------------------------------------------

const rows = [
    { fqn: "Foo\\Bar::baz" },
    { fqn: "Foo\\Quux::baz" },
    { fqn: "Other::method" },
    { fqn: "" },
    { fqn: "FOO\\Loud::Bar" },
    {},                    // missing fqn
];

assert_eq([], findMatches(rows, ""),           "empty query -> no matches");
assert_eq([], findMatches(rows, null),         "null query -> no matches");
assert_eq([0, 1, 4], findMatches(rows, "foo"), "case-insensitive substring");
assert_eq([0, 4], findMatches(rows, "bar"),    "bar matches positions 0 + 4 (case-insensitive)");
assert_eq([2], findMatches(rows, "method"),    "single match");
assert_eq([], findMatches(rows, "absent"),     "no match");
assert_eq([], findMatches([], "foo"),          "empty rows -> no matches");
assert_eq([], findMatches([{}], "foo"),        "row without fqn skipped");

// ---- highlightFqn --------------------------------------------------

assert_eq(
    [{ text: "Foo\\Bar::baz", matched: false }],
    highlightFqn("Foo\\Bar::baz", ""),
    "empty query -> single unmatched span"
);
assert_eq(
    [{ text: "Foo\\Bar::baz", matched: false }],
    highlightFqn("Foo\\Bar::baz", null),
    "null query -> single unmatched span"
);

assert_eq(
    [
        { text: "Foo\\", matched: false },
        { text: "Bar",  matched: true  },
        { text: "::baz", matched: false },
    ],
    highlightFqn("Foo\\Bar::baz", "Bar"),
    "match in the middle"
);

assert_eq(
    [
        { text: "Foo",   matched: true  },
        { text: "\\Bar::baz", matched: false },
    ],
    highlightFqn("Foo\\Bar::baz", "Foo"),
    "match at start"
);

assert_eq(
    [
        { text: "Foo\\Bar::", matched: false },
        { text: "baz",        matched: true  },
    ],
    highlightFqn("Foo\\Bar::baz", "baz"),
    "match at end"
);

// Case-insensitivity in the matcher; output preserves original case.
assert_eq(
    [
        { text: "FOO\\Loud::", matched: false },
        { text: "Bar",          matched: true  },
    ],
    highlightFqn("FOO\\Loud::Bar", "bar"),
    "matched span preserves source case"
);

// Multiple matches in one fqn (e.g. repeated word).
assert_eq(
    [
        { text: "x",  matched: false },
        { text: "AB", matched: true  },
        { text: "y",  matched: false },
        { text: "AB", matched: true  },
        { text: "z",  matched: false },
    ],
    highlightFqn("xABYABZ".replace(/Y/, "y").replace(/Z/, "z"), "AB"),
    "two matches with separator"
);

// No match → one unmatched span containing the whole fqn.
assert_eq(
    [{ text: "Foo\\Bar::baz", matched: false }],
    highlightFqn("Foo\\Bar::baz", "absent"),
    "no match -> single unmatched span"
);

// Empty fqn.
assert_eq(
    [{ text: "", matched: false }],
    highlightFqn("", "foo"),
    "empty fqn + non-empty query"
);

// Non-string fqn (defensive).
assert_eq(
    [{ text: "", matched: false }],
    highlightFqn(undefined, "foo"),
    "undefined fqn -> defensive empty"
);

report_done();
