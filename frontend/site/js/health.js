// Shared "is the API reachable" state behind the status bar's live dot.
// Every poller reports its outcome here; the dot dims on the first failed
// poll and a "connection lost" note appears after LOST_AFTER in a row.
const LOST_AFTER = 3;
let fails = 0;
const listeners = new Set();

function emit() {
  for (const fn of listeners) fn(state());
}

export function state() {
  return { fails, degraded: fails > 0, lost: fails >= LOST_AFTER };
}

export function reportOk() {
  if (fails === 0) return;
  fails = 0;
  emit();
}

export function reportFail() {
  fails++;
  emit();
}

export function onChange(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}
