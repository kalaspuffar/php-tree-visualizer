// Pure time formatters for the trace-list rows.
//
//   formatWall(ns)             — auto-scaled wall-time string
//                                 ("1.54 s", "345.6 ms", "123.4 µs", "5 ns")
//   formatRelative(epochSec)   — relative-time label
//                                 ("just now", "3m ago", "in 5s")
//
// INV-3 from SPECIFICATION.md §2.3 — `start_time_ns` is
// CLOCK_REALTIME; never mix with CLOCK_MONOTONIC. The PHP API
// already converted start_time_ns into ISO timestamps, so the
// frontend only deals with epoch seconds derived from start_time.

const NS_PER_US =        1_000;
const NS_PER_MS =    1_000_000;
const NS_PER_S  = 1_000_000_000;
const NS_PER_M  = 60 * NS_PER_S;
const NS_PER_MAX_DISPLAY = (99 * 60 + 59) * NS_PER_S; // 99:59 cap

const BYTES_PER_KB = 1024;
const BYTES_PER_MB = 1024 * 1024;
const BYTES_PER_GB = 1024 * 1024 * 1024;

/**
 * Format a nanosecond wall-time value. Picks the most senior unit
 * with a non-trivial value; rounds to a sensible decimal count for
 * readability.
 *
 * @param {number} ns
 * @returns {string}
 */
export function formatWall(ns) {
    if (!Number.isFinite(ns) || ns < 0) return "—";
    if (ns >= NS_PER_S)  return (ns / NS_PER_S).toFixed(2)  + " s";
    if (ns >= NS_PER_MS) return (ns / NS_PER_MS).toFixed(1) + " ms";
    if (ns >= NS_PER_US) return (ns / NS_PER_US).toFixed(1) + " µs";
    return Math.round(ns) + " ns";
}

/**
 * Format a relative time. Uses the most senior unit with a non-zero
 * value; future times use the "in {N}…" form. The "now" reference is
 * configurable for testability.
 *
 * @param {number} epochSec    Target time in epoch seconds.
 * @param {number} [nowSec]    Reference time; defaults to Date.now()/1000.
 * @returns {string}
 */
export function formatRelative(epochSec, nowSec) {
    if (!Number.isFinite(epochSec)) return "";
    const reference = Number.isFinite(nowSec) ? nowSec : Date.now() / 1000;
    const diff = reference - epochSec; // > 0 = in the past

    const abs = Math.abs(diff);
    const sign = diff >= 0 ? "ago" : "in";

    if (abs < 5)              return sign === "ago" ? "just now" : "in a moment";
    if (abs < 60)             return formatUnit(abs, 1, "s", sign);
    if (abs < 3600)           return formatUnit(abs, 60, "m", sign);
    if (abs < 86_400)         return formatUnit(abs, 3600, "h", sign);
    return formatUnit(abs, 86_400, "d", sign);
}

function formatUnit(secs, divisor, unit, direction) {
    const n = Math.floor(secs / divisor);
    return direction === "ago" ? `${n}${unit} ago` : `in ${n}${unit}`;
}

/**
 * Full-ladder duration formatter for the call-tree row (§3.3.7.4).
 *
 * Ranges:
 *   x <   1 µs  → "{N} ns"          3 sig figs (or integer)
 *   x <   1 ms  → "{N} µs"          3 sig figs
 *   x <   1 s   → "{N} ms"          3 sig figs
 *   x <  60 s   → "{N} s"           3 sig figs
 *   x <  ~100 m → "{m}:{ss}"        zero-padded seconds
 *   x ≥ 99:59   → "99:59"           cap display
 *
 * Negative or non-finite values render as "—".
 *
 * @param {number} ns
 * @returns {string}
 */
export function formatDuration(ns) {
    if (!Number.isFinite(ns) || ns < 0) return "—";
    if (ns === 0) return "0 ns";

    if (ns < NS_PER_US) {
        return formatThreeSigFigs(ns) + " ns";
    }
    if (ns < NS_PER_MS) {
        return formatThreeSigFigs(ns / NS_PER_US) + " µs";
    }
    if (ns < NS_PER_S) {
        return formatThreeSigFigs(ns / NS_PER_MS) + " ms";
    }
    if (ns < NS_PER_M) {
        return formatThreeSigFigs(ns / NS_PER_S) + " s";
    }
    if (ns >= NS_PER_MAX_DISPLAY) {
        return "99:59";
    }
    const totalSec = Math.floor(ns / NS_PER_S);
    const m = Math.floor(totalSec / 60);
    const s = totalSec % 60;
    return `${m}:${s.toString().padStart(2, "0")}`;
}

/**
 * Render 3 significant figures, dropping trailing zeros only when
 * the value's magnitude allows it (the spec examples preserve the
 * decimal form, e.g. "1.20 µs" not "1.2 µs").
 *
 * Examples from §3.3.7.4 we have to match:
 *   847    → "847"
 *   41.2   → "41.2"
 *   1.20   → "1.20"
 *   12.4   → "12.4"
 *   187    → "187"
 *   999    → "999"
 *   59.9   → "59.9"
 */
function formatThreeSigFigs(x) {
    if (x >= 100) return Math.round(x).toString();
    if (x >= 10)  return x.toFixed(1);
    return x.toFixed(2);
}

/**
 * Memory-delta formatter for the Mem column (§3.3.7.4).
 *
 * Returns a { text, tone } pair so the row builder can apply the
 * right CSS class (positive = --fg, negative = --danger, zero =
 * --fg-subtle).
 *
 * Sign rule: positive →  "+", negative → "-", zero → "±".
 * Ladder by absolute value: B / KB / MB / GB, 3 significant figures.
 *
 * @param {number} bytes
 * @returns {{ text: string, tone: "positive"|"negative"|"zero" }}
 */
export function formatMemDelta(bytes) {
    if (!Number.isFinite(bytes)) return { text: "—", tone: "zero" };
    if (bytes === 0) return { text: "±0 B", tone: "zero" };

    const tone = bytes > 0 ? "positive" : "negative";
    const sign = bytes > 0 ? "+" : "-";
    const abs = Math.abs(bytes);

    let value, unit;
    if (abs < BYTES_PER_KB)      { value = abs;                 unit = "B";  }
    else if (abs < BYTES_PER_MB) { value = abs / BYTES_PER_KB;  unit = "KB"; }
    else if (abs < BYTES_PER_GB) { value = abs / BYTES_PER_MB;  unit = "MB"; }
    else                          { value = abs / BYTES_PER_GB;  unit = "GB"; }

    // Bytes are always integer; KB/MB/GB use 3 sig figs.
    const text = unit === "B"
        ? `${sign}${Math.round(value)} ${unit}`
        : `${sign}${formatThreeSigFigs(value)} ${unit}`;
    return { text, tone };
}

/**
 * Percent-of-parent formatter for the %Parent column (§3.3.7.4).
 *
 * Returns a { text, tone } pair. `pct` is a percentage value
 * (0–100), not a fraction.
 *
 *   pct = 0       → "0%"      tone "zero"
 *   pct < 0.1     → "<0.1%"
 *   pct < 1       → "0.X%"
 *   pct < 10      → "X.X%"
 *   pct ≥ 10      → "XX%"
 *
 * @param {number} pct
 * @returns {{ text: string, tone: "default"|"zero" }}
 */
export function formatPercentOfParent(pct) {
    if (!Number.isFinite(pct) || pct < 0) return { text: "—", tone: "zero" };
    if (pct === 0)   return { text: "0%",    tone: "zero" };
    if (pct < 0.1)   return { text: "<0.1%", tone: "default" };
    if (pct < 1)     return { text: pct.toFixed(1) + "%", tone: "default" };
    if (pct < 10)    return { text: pct.toFixed(1) + "%", tone: "default" };
    return { text: Math.round(pct) + "%", tone: "default" };
}

/**
 * Count formatter with the en-US thousands separator (§3.3.7.4).
 * Locale is pinned to en-US so the output is deterministic across
 * browsers and the operator's OS locale doesn't surprise them.
 *
 * @param {number} n
 * @returns {string}
 */
export function formatCount(n) {
    if (!Number.isFinite(n) || n < 0) return "—";
    return Math.floor(n).toLocaleString("en-US");
}
