export function noid(micronoid) {
  if (micronoid === null || micronoid === undefined) return "-";
  const n = typeof micronoid === "string" ? BigInt(micronoid) : BigInt(micronoid);
  const whole = n / 1_000_000n;
  const frac = (n % 1_000_000n).toString().padStart(6, "0").replace(/0+$/, "");
  return frac ? `${whole}.${frac} NOID` : `${whole} NOID`;
}

export function shortHash(h, lead = 8, tail = 6) {
  if (!h) return "-";
  if (h.length <= lead + tail + 1) return h;
  return `${h.slice(0, lead)}…${h.slice(-tail)}`;
}

export function timeAgo(unixSeconds) {
  if (!unixSeconds) return "-";
  const s = Math.max(0, Math.floor(Date.now() / 1000) - unixSeconds);
  // The trailing sub-unit is zero-padded to a fixed width so that, combined
  // with right-aligning the column, "m"/"h" land in the same place on every
  // row - right-alignment only keeps things lined up if everything to the
  // right of them is the same width, and an unpadded "5s" vs "45s" isn't.
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}m ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

export function fullTime(unixSeconds) {
  if (!unixSeconds) return "-";
  return new Date(unixSeconds * 1000).toISOString().replace("T", " ").replace(".000Z", " UTC");
}

export function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}
