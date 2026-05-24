// Debounce helper.
//
//   const onChange = debounce((q) => fetchTraces(q), 250);
//   inputEl.addEventListener("input", (e) => onChange(e.target.value));
//
// The most recent call within `ms` wins; earlier scheduled calls are
// cancelled. Uses the host's setTimeout / clearTimeout so tests can
// substitute via globalThis.

/**
 * @template {(...args: any[]) => any} F
 * @param {F} fn
 * @param {number} ms
 * @returns {(...args: Parameters<F>) => void}
 */
export function debounce(fn, ms) {
    let timer = null;
    return function debounced(...args) {
        if (timer !== null) {
            clearTimeout(timer);
        }
        timer = setTimeout(() => {
            timer = null;
            fn.apply(this, args);
        }, ms);
    };
}
