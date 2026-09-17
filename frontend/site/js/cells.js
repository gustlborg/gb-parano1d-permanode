import { feeRateOf } from "./format.js";

// A block face is a 4x4 grid. Each lit cell is one page of block space (the
// chain's own unit - a transaction spans `page_count` pages), ordered by fee
// rate with the best-paying transaction first, the way mempool.space packs
// its block squares, and shaded by that fee rate. Coinbase / dev-payout
// pages carry no fee and sit dimmest at the end.
export const CELLS = 16;

// Opacity bands from the design handoff: confirmed blocks 0.36-0.85, the
// live mempool face 0.42-1.0.
const BAND = { cool: [0.36, 0.49], hot: [0.42, 0.58] };

// Flattens transactions into one normalized shade (0..1) per page.
export function pagesOf(txs) {
  const items = (txs || []).map((tx) => ({
    pages: Math.max(1, Number(tx.page_count) || 1),
    rate: feeRateOf(tx),
    free: !!(tx.coinbase || tx.development_payout),
  }));
  items.sort((a, b) => (a.free === b.free ? b.rate - a.rate : a.free ? 1 : -1));
  const rates = items.filter((i) => !i.free).map((i) => i.rate);
  const lo = rates.length ? Math.min(...rates) : 0;
  const hi = rates.length ? Math.max(...rates) : 0;
  const out = [];
  for (const it of items) {
    const t = it.free ? 0 : hi > lo ? (it.rate - lo) / (hi - lo) : 1;
    for (let p = 0; p < it.pages; p++) out.push(t);
  }
  return out;
}

// The 16 cell states for one face, starting `offset` pages into the list
// (offset 16, 32, ... are the "Queued +n" overflow faces of the mempool).
export function cellShades(pages, hot, offset = 0) {
  const [base, span] = hot ? BAND.hot : BAND.cool;
  const out = [];
  for (let i = 0; i < CELLS; i++) {
    const t = pages[offset + i];
    out.push(t === undefined ? null : base + t * span);
  }
  return out;
}

export function cellsHtml(shades) {
  const cells = shades
    .map((o) => (o === null ? "<i></i>" : `<i class="on" style="opacity:${o.toFixed(3)}"></i>`))
    .join("");
  return `<div class="cells">${cells}</div>`;
}

// Updates an existing grid in place so fill changes ride the cells' 0.8s
// transition instead of rebuilding the DOM.
export function applyCells(gridEl, shades) {
  const cells = gridEl.children;
  for (let i = 0; i < CELLS && i < cells.length; i++) {
    const o = shades[i];
    cells[i].classList.toggle("on", o !== null);
    cells[i].style.opacity = o === null ? "" : o.toFixed(3);
  }
}
