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

const NS_PER_US =     1_000;
const NS_PER_MS = 1_000_000;
const NS_PER_S  = 1_000_000_000;

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
