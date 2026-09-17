// Slice-and-dice treemap: packs a list of weighted items into a rectangle,
// splitting along whichever axis is currently longer so the result stays
// roughly square-ish. Mutates each item with a `_rect = {x,y,w,h}`.
//
// This is what mempool.space's block visualization is imitating (many
// differently-sized rectangles, area proportional to transaction size,
// packed to fill a square) - not a byte-for-byte port of their renderer,
// but the same visual idea, implemented independently.
export function treemap(items, x, y, w, h) {
  if (items.length === 0) return items;
  if (items.length === 1) {
    items[0]._rect = { x, y, w, h };
    return items;
  }

  const total = items.reduce((a, b) => a + b.value, 0) || 1;
  let acc = 0;
  let splitIdx = 1;
  for (let i = 0; i < items.length; i++) {
    acc += items[i].value;
    if (acc >= total / 2) {
      splitIdx = i + 1;
      break;
    }
  }
  splitIdx = Math.min(Math.max(splitIdx, 1), items.length - 1);

  const left = items.slice(0, splitIdx);
  const right = items.slice(splitIdx);
  const leftTotal = left.reduce((a, b) => a + b.value, 0);
  const frac = total > 0 ? leftTotal / total : 0.5;

  if (w >= h) {
    const leftW = w * frac;
    treemap(left, x, y, leftW, h);
    treemap(right, x + leftW, y, w - leftW, h);
  } else {
    const leftH = h * frac;
    treemap(left, x, y, w, leftH);
    treemap(right, x, y + leftH, w, h - leftH);
  }
  return items;
}
