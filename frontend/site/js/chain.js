import { CELLS, pagesOf, cellShades, cellsHtml, applyCells } from "./cells.js";
import { int } from "./format.js";

export const CHAIN_BLOCKS = 14;
// Tile width + arrow column; the slide animation moves the track by exactly
// one of these so the new block appears to push the chain along.
export const SLIDE_PX = 192;
const SLIDE_MS = 3400;
const FLASH_MS = 2400;
const QUEUED_MAX = 3;

function reducedMotion() {
  return window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

export function blockTileHtml(summary, detail, flash) {
  const h = summary.height;
  const gap = !summary.body_captured;
  const shades = cellShades(pagesOf(detail?.transactions || []), false);
  const sub = gap ? "no body" : `${summary.tx_count} tx`;
  return `<div class="chain-item">
      <a class="tile${gap ? " gap" : ""}" href="/block/${h}" data-link>
        <div class="face${flash ? " flash" : ""}" data-flash-height="${flash ? h : ""}">${cellsHtml(shades)}</div>
        <div class="tile-label"><span class="t">#${h}</span><span class="s"><span class="ago" data-ts="${summary.timestamp}"></span> · ${sub}</span></div>
      </a>
      <div class="chain-arrow">←</div>
    </div>`;
}

// One face per 16 pages of pending transactions: "Mempool" first, then
// "Queued +1", "Queued +2" ... for whatever doesn't fit the first face.
export function mempoolShades(info) {
  const pages = pagesOf(info?.txs || []);
  const faces = Math.max(1, Math.min(1 + QUEUED_MAX, Math.ceil(pages.length / CELLS)));
  const out = [];
  for (let i = 0; i < faces; i++) {
    out.push({ shades: cellShades(pages, true, i * CELLS), pages: Math.max(0, Math.min(CELLS, pages.length - i * CELLS)) });
  }
  return out;
}

export function mempoolTilesHtml(info) {
  const size = info ? int(info.size) : "-";
  return mempoolShades(info)
    .map((f, i) => {
      const title = i === 0 ? "Mempool" : `Queued +${i}`;
      const sub = i === 0 ? `${size} pending` : `${f.pages} more page${f.pages === 1 ? "" : "s"}`;
      return `<div class="chain-item">
          <a class="tile hot" href="/mempool" data-link data-mempool-face="${i}">
            <div class="face hot">${cellsHtml(f.shades)}<div class="scan"></div></div>
            <div class="tile-label"><span class="t">${title}</span><span class="s">${sub}</span></div>
          </a>
          <div class="chain-arrow">←</div>
        </div>`;
    })
    .join("");
}

// Refreshes the mempool face(s) in place. Returns false if the number of
// faces changed, in which case the caller has to rebuild the track.
export function updateMempoolTiles(track, info) {
  const faces = mempoolShades(info);
  const tiles = track.querySelectorAll("[data-mempool-face]");
  if (tiles.length !== faces.length) return false;
  tiles.forEach((tile, i) => {
    applyCells(tile.querySelector(".cells"), faces[i].shades);
    const sub = tile.querySelector(".tile-label .s");
    if (sub) sub.textContent = i === 0 ? `${int(info.size)} pending` : `${faces[i].pages} more page${faces[i].pages === 1 ? "" : "s"}`;
  });
  return true;
}

// WAAPI rather than a CSS transition so the animation survives the track
// being re-rendered underneath it.
export function slideTrack(track) {
  if (!track || !track.animate || reducedMotion()) return;
  requestAnimationFrame(() => {
    track.animate(
      [{ transform: `translateX(-${SLIDE_PX}px)` }, { transform: "translateX(0)" }],
      { duration: SLIDE_MS, easing: "cubic-bezier(.16,.86,.28,1)" }
    );
  });
}

// The freshly found block glows for FLASH_MS, then the face's own 2.4s
// transition eases it back to the resting border.
export function scheduleFlashEnd(track) {
  const face = track.querySelector(".face.flash");
  if (!face) return;
  setTimeout(() => face.classList.remove("flash"), FLASH_MS);
}

export function etaText(tipTimestamp, avgSeconds) {
  if (!tipTimestamp) return "";
  const avg = avgSeconds || 20;
  const eta = Math.round(avg - (Date.now() / 1000 - tipTimestamp));
  return eta > 0 ? `next block in ~${eta}s` : `next block due · ${-eta}s over`;
}
