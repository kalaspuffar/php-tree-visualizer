// In-tree search — pure functions.
//
//   findMatches(rows, query): number[]   — flat-list indices of
//                                          rows whose fqn (case-
//                                          insensitive) contains
//                                          the query.
//   highlightFqn(fqn, query): {text, matched}[]
//                                         — segments for the row
//                                          builder to render as a
//                                          chain of plain + matched
//                                          spans.
//
// No DOM access. The orchestrator (detail.js) owns the current-
// match index + the virtualizer.scrollToIndex calls.

/**
 * @param {Array<{ fqn?: string }>} rows
 * @param {string} query
 * @returns {number[]}
 */
export function findMatches(rows, query) {
    if (typeof query !== "string" || query === "") return [];
    const q = query.toLowerCase();
    const out = [];
    for (let i = 0; i < rows.length; i++) {
        const fqn = rows[i]?.fqn;
        if (typeof fqn !== "string") continue;
        if (fqn.toLowerCase().includes(q)) {
            out.push(i);
        }
    }
    return out;
}

/**
 * Split `fqn` into segments alternating plain + matched, preserving
 * the original case in the text. Empty / non-matching query returns
 * a single unmatched segment containing the whole fqn.
 *
 * @param {string} fqn
 * @param {string} query
 * @returns {Array<{ text: string, matched: boolean }>}
 */
export function highlightFqn(fqn, query) {
    if (typeof fqn !== "string") return [{ text: "", matched: false }];
    if (typeof query !== "string" || query === "") {
        return [{ text: fqn, matched: false }];
    }

    const lowerFqn = fqn.toLowerCase();
    const lowerQ = query.toLowerCase();
    const segments = [];
    let i = 0;
    while (i < fqn.length) {
        const next = lowerFqn.indexOf(lowerQ, i);
        if (next < 0) {
            segments.push({ text: fqn.slice(i), matched: false });
            break;
        }
        if (next > i) {
            segments.push({ text: fqn.slice(i, next), matched: false });
        }
        segments.push({ text: fqn.slice(next, next + lowerQ.length), matched: true });
        i = next + lowerQ.length;
    }
    if (segments.length === 0) {
        // Reached only when fqn === "" and query is non-empty.
        segments.push({ text: fqn, matched: false });
    }
    return segments;
}
