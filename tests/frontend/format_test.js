import {
    formatDuration,
    formatMemDelta,
    formatPercentOfParent,
    formatCount,
} from "../../viz/js/time.js";
import { assert_eq, assert_true, report_done } from "./lib/assert.js";

// ---- formatDuration ladder (§3.3.7.4) ------------------------------

// Spec examples explicitly listed in the duration table:
assert_eq("847 ns",    formatDuration(847),              "847 ns");
assert_eq("41.2 ns",   formatDuration(41.2),             "41.2 ns");
assert_eq("1.20 ns",   formatDuration(1.2),              "1.20 ns");
assert_eq("12.4 µs",   formatDuration(12400),            "12.4 µs");
assert_eq("187 µs",    formatDuration(187_000),          "187 µs");
assert_eq("999 µs",    formatDuration(999_000),          "999 µs");
assert_eq("1.20 ms",   formatDuration(1_200_000),        "1.20 ms");
assert_eq("187 ms",    formatDuration(187_000_000),      "187 ms");
assert_eq("999 ms",    formatDuration(999_000_000),      "999 ms");
assert_eq("1.20 s",    formatDuration(1_200_000_000),    "1.20 s");
assert_eq("12.4 s",    formatDuration(12_400_000_000),   "12.4 s");
assert_eq("59.9 s",    formatDuration(59_900_000_000),   "59.9 s");

// Sub-spec boundary cases that surfaced in the design D-5 ladder.
assert_eq("0 ns",      formatDuration(0),                "0");
assert_eq("999 ns",    formatDuration(999),              "999 ns");
assert_eq("1.00 µs",   formatDuration(1000),             "1 µs lower-boundary");
assert_eq("1.00 ms",   formatDuration(1_000_000),        "1 ms lower-boundary");
assert_eq("1.00 s",    formatDuration(1_000_000_000),    "1 s lower-boundary");

// m:ss range and cap.
assert_eq("1:00",      formatDuration(60_000_000_000),   "60 s -> 1:00");
assert_eq("2:14",      formatDuration(134_000_000_000),  "spec sample 2:14");
assert_eq("12:01",     formatDuration(721_000_000_000),  "spec sample 12:01");
assert_eq("99:59",     formatDuration((99 * 60 + 59) * 1_000_000_000), "99:59 boundary");
assert_eq("99:59",     formatDuration((100 * 60) * 1_000_000_000),     "above cap clamps");
assert_eq("99:59",     formatDuration(7_200_000_000_000), "2-hour clamps");

// Bad inputs.
assert_eq("—",         formatDuration(-1),               "negative");
assert_eq("—",         formatDuration(NaN),              "NaN");
assert_eq("—",         formatDuration(Infinity),         "Infinity");

// ---- formatMemDelta (§3.3.7.4) -------------------------------------

let m;
m = formatMemDelta(247);    assert_eq("+247 B", m.text, "+247 B");   assert_eq("positive", m.tone, "tone +");
m = formatMemDelta(-12);    assert_eq("-12 B",  m.text, "-12 B");    assert_eq("negative", m.tone, "tone -");
m = formatMemDelta(0);      assert_eq("±0 B",   m.text, "±0 B");     assert_eq("zero",     m.tone, "tone 0");
m = formatMemDelta(1024);   assert_eq("+1.00 KB", m.text, "+1.00 KB");
m = formatMemDelta(831_488); assert_eq("+812 KB", m.text, "+812 KB"); // 812 * 1024
m = formatMemDelta(-3_481);  assert_eq("-3.40 KB", m.text, "spec sample -3.40 KB"); // ~3.40 KB
m = formatMemDelta(1_258_291); assert_eq("+1.20 MB", m.text, "+1.20 MB"); // 1.20 * 1024 * 1024
m = formatMemDelta(-52_953_088);
assert_eq("-50.5 MB", m.text, "-50.5 MB");
m = formatMemDelta(2_469_606_195); // 2.30 GB
assert_eq("+2.30 GB", m.text, "+2.30 GB");

// Edge: NaN / Infinity → "—".
m = formatMemDelta(NaN);
assert_eq("—", m.text, "NaN -> em dash");
assert_eq("zero", m.tone, "NaN tone neutral");

// ---- formatPercentOfParent ----------------------------------------

let p;
p = formatPercentOfParent(0);      assert_eq("0%",    p.text, "0%");   assert_eq("zero",    p.tone, "0% tone");
p = formatPercentOfParent(0.04);   assert_eq("<0.1%", p.text, "<0.1%"); assert_eq("default", p.tone, "<0.1% tone");
p = formatPercentOfParent(0.4);    assert_eq("0.4%",  p.text, "0.4%");
p = formatPercentOfParent(0.99);   assert_eq("1.0%",  p.text, "0.99 -> 1.0% (sub-1 bracket boundary)");
p = formatPercentOfParent(4.27);   assert_eq("4.3%",  p.text, "4.3%");
// 9.95 is actually 9.94999999... in IEEE-754; toFixed(1) gives "9.9".
// JS's banker-ish rounding; the spec doesn't pin a direction.
p = formatPercentOfParent(9.95);   assert_eq("9.9%",  p.text, "9.95 floats down to 9.9");
p = formatPercentOfParent(10);     assert_eq("10%",   p.text, "10% (integer bracket)");
p = formatPercentOfParent(42.7);   assert_eq("43%",   p.text, "42.7 -> 43%");
p = formatPercentOfParent(100);    assert_eq("100%",  p.text, "100%");
p = formatPercentOfParent(-1);     assert_eq("—",     p.text, "negative -> —");
p = formatPercentOfParent(NaN);    assert_eq("—",     p.text, "NaN -> —");

// ---- formatCount ---------------------------------------------------

assert_eq("1",       formatCount(1),       "1");
assert_eq("42",      formatCount(42),      "42");
assert_eq("1,201",   formatCount(1_201),   "1,201");
assert_eq("248,932", formatCount(248_932), "248,932");
assert_eq("1,000,000", formatCount(1_000_000), "1,000,000");
assert_eq("0",       formatCount(0),       "0");
assert_eq("—",       formatCount(-1),      "negative -> —");
assert_eq("—",       formatCount(NaN),     "NaN -> —");

report_done();
