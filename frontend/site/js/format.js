// Thousands grouping with a narrow no-break space, so large NOID figures
// read as "5 454 023.7275" without ever wrapping mid-number.
function group(intStr) {
  return intStr.replace(/\B(?=(\d{3})+(?!\d))/g, " ");
}

export function noid(micronoid, unit = true) {
  if (micronoid === null || micronoid === undefined) return "-";
  const n = BigInt(micronoid);
  const neg = n < 0n;
  const abs = neg ? -n : n;
  const whole = group((abs / 1_000_000n).toString());
  const frac = (abs % 1_000_000n).toString().padStart(6, "0").replace(/0+$/, "");
  const num = (neg ? "−" : "") + (frac ? `${whole}.${frac}` : whole);
  return unit ? `${num} NOID` : num;
}

export function int(n) {
  if (n === null || n === undefined) return "-";
  return group(String(n));
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
  return `${Math.floor(s / 86400)}d ${String(Math.floor((s % 86400) / 3600)).padStart(2, "0")}h ago`;
}

export function hashrate(hs) {
  if (hs === null || hs === undefined) return "-";
  const units = ["H/s", "KH/s", "MH/s", "GH/s", "TH/s", "PH/s", "EH/s"];
  let i = 0;
  let v = hs;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i++;
  }
  return `${v.toFixed(2)} ${units[i]}`;
}

export function seconds(s) {
  if (s === null || s === undefined) return "-";
  return `${s.toFixed(1)} s`;
}

export function fullTime(unixSeconds) {
  if (!unixSeconds) return "-";
  return new Date(unixSeconds * 1000).toISOString().replace("T", " ").replace(".000Z", " UTC");
}

export function isoToUnix(iso) {
  const t = Date.parse(iso);
  return Number.isFinite(t) ? Math.floor(t / 1000) : 0;
}

// The node's own mempool ordering key (noid_chain::mempool::compute_fee_rate):
// fee divided by a weight of inputs + outputs + 4 per net new slot, in
// µNOID per weight unit. Mempool entries carry it already; for confirmed
// transactions we recompute it from the same inputs.
export function feeRateOf(tx) {
  if (typeof tx.fee_rate === "number") return tx.fee_rate;
  const nIn = Number(tx.n_inputs) || 0;
  const nOut = Number(tx.n_outputs) || 0;
  const weight = Math.max(1, nIn + nOut + 4 * Math.max(0, nOut - nIn));
  return Math.floor((Number(tx.fee_micronoid) || 0) / weight);
}

export function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}
