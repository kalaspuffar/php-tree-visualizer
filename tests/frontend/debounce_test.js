import { debounce } from "../../viz/js/debounce.js";
import { assert_eq, report_done } from "./lib/assert.js";

// Wire fake timers via globalThis so debounce uses our scheduler.
// The implementation calls bare `setTimeout`/`clearTimeout`, which
// in ES modules resolve through globalThis. Stash the originals and
// restore after the tests so any future cleanup runs cleanly.

const originalSet = globalThis.setTimeout;
const originalClear = globalThis.clearTimeout;

let now = 0;
let nextId = 1;
const pending = new Map(); // id -> { at, fn }

globalThis.setTimeout = (fn, ms) => {
    const id = nextId++;
    pending.set(id, { at: now + ms, fn });
    return id;
};
globalThis.clearTimeout = (id) => {
    pending.delete(id);
};

function tick(ms) {
    now += ms;
    // Fire any timer whose `at` is <= now, in insertion order.
    const ready = [...pending.entries()]
        .filter(([, t]) => t.at <= now)
        .sort((a, b) => a[1].at - b[1].at);
    for (const [id, t] of ready) {
        pending.delete(id);
        t.fn();
    }
}

// ---- rapid calls produce one invocation with the last args ---------

{
    let received = null;
    const fn = debounce((...args) => { received = args; }, 250);
    fn("a");
    tick(50);
    fn("b");
    tick(50);
    fn("c");
    // Not enough time has elapsed since the last call.
    tick(249);
    assert_eq(null, received, "no firing before 250 ms after last call");
    tick(1);
    assert_eq(["c"], received, "last args win");
}

// ---- single call after a delay fires once --------------------------

{
    let count = 0;
    const fn = debounce(() => { count++; }, 100);
    fn();
    tick(100);
    assert_eq(1, count, "fires exactly once");
    tick(500);
    assert_eq(1, count, "doesn't fire spontaneously later");
}

// ---- subsequent call after the fire window starts fresh ------------

{
    let count = 0;
    const fn = debounce(() => { count++; }, 100);
    fn();
    tick(100);
    assert_eq(1, count, "first call fired");
    fn();
    tick(99);
    assert_eq(1, count, "no second fire before window elapses");
    tick(1);
    assert_eq(2, count, "second call fires");
}

// ---- explicit clearTimeout via re-call cancels prior fire ----------

{
    let count = 0;
    const fn = debounce(() => { count++; }, 100);
    fn();
    tick(99);
    fn();           // resets timer
    tick(99);
    assert_eq(0, count, "still not fired (timer kept resetting)");
    tick(1);
    assert_eq(1, count, "fires once 100ms after the last call");
}

// Restore timers so any future test additions get a clean scheduler.
globalThis.setTimeout = originalSet;
globalThis.clearTimeout = originalClear;

report_done();
