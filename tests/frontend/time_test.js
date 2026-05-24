import { formatWall, formatRelative } from "../../viz/js/time.js";
import { assert_eq, report_done } from "./lib/assert.js";

// ---- formatWall boundary values ------------------------------------

assert_eq("1 ns",       formatWall(1),                  "1 ns");
assert_eq("999 ns",     formatWall(999),                "999 ns");
assert_eq("1.0 µs",     formatWall(1_000),              "1 µs");
assert_eq("999.9 µs",   formatWall(999_900),            "just under ms");
assert_eq("1.0 ms",     formatWall(1_000_000),          "1 ms");
assert_eq("345.6 ms",   formatWall(345_600_000),        "345.6 ms");
assert_eq("999.9 ms",   formatWall(999_900_000),        "just under s");
assert_eq("1.00 s",     formatWall(1_000_000_000),      "1 s");
assert_eq("1.54 s",     formatWall(1_543_210_000),      "spec sample 1.54 s");
assert_eq("—",          formatWall(-1),                 "negative -> em dash");
assert_eq("—",          formatWall(NaN),                "NaN -> em dash");
assert_eq("—",          formatWall(Infinity),           "Infinity -> em dash");

// ---- formatRelative — reference-based, deterministic --------------

const NOW = 1_700_000_000;

assert_eq("just now",  formatRelative(NOW,        NOW), "delta 0 -> just now");
assert_eq("just now",  formatRelative(NOW - 2,    NOW), "delta 2 s -> just now");
assert_eq("5s ago",    formatRelative(NOW - 5,    NOW), "5 s ago");
assert_eq("59s ago",   formatRelative(NOW - 59,   NOW), "59 s ago");
assert_eq("1m ago",    formatRelative(NOW - 60,   NOW), "1 m ago");
assert_eq("5m ago",    formatRelative(NOW - 300,  NOW), "5 m ago");
assert_eq("59m ago",   formatRelative(NOW - 3599, NOW), "59 m ago");
assert_eq("1h ago",    formatRelative(NOW - 3600, NOW), "1 h ago");
assert_eq("23h ago",   formatRelative(NOW - 23 * 3600, NOW), "23 h ago");
assert_eq("1d ago",    formatRelative(NOW - 25 * 3600, NOW), "25 h promotes to 1d");
assert_eq("1d ago",    formatRelative(NOW - 86_400, NOW), "1 d ago");
assert_eq("7d ago",    formatRelative(NOW - 7 * 86_400, NOW), "7 d ago");

// Future times use the "in {N}…" form.
assert_eq("in a moment", formatRelative(NOW + 2, NOW), "2 s future -> in a moment");
assert_eq("in 30s",      formatRelative(NOW + 30, NOW), "30 s future");
assert_eq("in 5m",       formatRelative(NOW + 300, NOW), "5 m future");

// Default reference is Date.now() — just sanity-check the shape.
const out = formatRelative(Date.now() / 1000 - 60);
assert_eq("1m ago", out, "default reference uses Date.now()");

report_done();
