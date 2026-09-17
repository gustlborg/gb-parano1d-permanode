import { treemap } from "./treemap.js";

const GAP = 1;

function feeRateOf(tx) {
  if (typeof tx.fee_rate === "number") return tx.fee_rate; // mempool tx
  const size = Math.max(1, tx.page_count || 1);
  return tx.fee_micronoid / size; // confirmed tx: approximate rate
}

function sizeOf(tx) {
  return Math.max(1, tx.page_count || 1);
}

// Renders `txs` as a packed square of rectangles into `canvas`, area
// proportional to transaction size, shade proportional to fee rate. This is
// the same visual idea mempool.space's block view uses (size + fee-rate
// packed squares) - reimplemented from scratch with a plain treemap rather
// than their WebGL renderer, since Parano1d's block sizes don't need that.
export function renderBlockSquare(canvas, txs, opts = {}) {
  const ctx = canvas.getContext("2d");
  const dpr = window.devicePixelRatio || 1;
  const cssW = canvas.clientWidth || canvas.width;
  const cssH = canvas.clientHeight || canvas.height;
  canvas.width = cssW * dpr;
  canvas.height = cssH * dpr;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, cssW, cssH);

  if (!txs || txs.length === 0) {
    ctx.fillStyle = "rgba(255,255,255,0.04)";
    ctx.fillRect(0, 0, cssW, cssH);
    if (opts.emptyLabel) {
      ctx.fillStyle = "#4a5068";
      ctx.font = "11px sans-serif";
      ctx.textAlign = "center";
      ctx.fillText(opts.emptyLabel, cssW / 2, cssH / 2 + 4);
    }
    return;
  }

  const items = txs
    .map((tx) => ({ value: sizeOf(tx), tx }))
    .sort((a, b) => b.value - a.value);
  treemap(items, 0, 0, cssW, cssH);

  const rates = items.map((it) => feeRateOf(it.tx));
  const maxRate = Math.max(...rates, 1);
  const minRate = Math.min(...rates, 0);

  for (const it of items) {
    const r = it._rect;
    const rate = feeRateOf(it.tx);
    const t = maxRate > minRate ? (rate - minRate) / (maxRate - minRate) : 0.5;
    ctx.fillStyle = shade(it.tx.coinbase, it.tx.development_payout, t);
    const w = Math.max(0, r.w - GAP);
    const h = Math.max(0, r.h - GAP);
    ctx.fillRect(r.x + GAP / 2, r.y + GAP / 2, w, h);
    if (opts.onRect) opts.onRect(it.tx, r);
  }
}

function shade(coinbase, devPayout, t) {
  if (coinbase) return "#3a6b52";
  if (devPayout) return "#6b5a3a";
  // low fee -> dim accent, high fee -> bright accent
  const lo = [31, 122, 112]; // --accent-dim
  const hi = [53, 208, 192]; // --accent
  const c = lo.map((v, i) => Math.round(v + (hi[i] - v) * t));
  return `rgb(${c[0]},${c[1]},${c[2]})`;
}

// Hit-tests a click against the rects assigned by the last renderBlockSquare
// call, by re-running the same layout (cheap for the tx counts involved).
export function pickTx(canvas, txs, clientX, clientY) {
  const rect = canvas.getBoundingClientRect();
  const x = clientX - rect.left;
  const y = clientY - rect.top;
  const items = txs
    .map((tx) => ({ value: sizeOf(tx), tx }))
    .sort((a, b) => b.value - a.value);
  treemap(items, 0, 0, rect.width, rect.height);
  for (const it of items) {
    const r = it._rect;
    if (x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h) return it.tx;
  }
  return null;
}
