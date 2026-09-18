// Instant tooltips. Native `title` bubbles only show up after the
// browser's own delay (around a second), so every element with a title
// gets its text moved into data-tip on first hover and a positioned panel
// shows immediately instead. Newlines in the text become line breaks.
const tip = document.createElement("div");
tip.className = "tip";
tip.setAttribute("role", "tooltip");
document.body.appendChild(tip);
let current = null;

function textOf(el) {
  if (el.dataset.tip === undefined && el.title) {
    el.dataset.tip = el.title;
    el.removeAttribute("title");
  }
  return el.dataset.tip || "";
}

function show(el) {
  const text = textOf(el);
  if (!text) return;
  current = el;
  tip.textContent = text;
  tip.classList.add("show");
  const r = el.getBoundingClientRect();
  const margin = 8;
  tip.style.left = "0px";
  tip.style.top = "0px";
  const w = tip.offsetWidth;
  const h = tip.offsetHeight;
  let left = r.left + r.width / 2 - w / 2;
  left = Math.max(margin, Math.min(left, window.innerWidth - w - margin));
  let top = r.bottom + 6;
  if (top + h > window.innerHeight - margin) top = r.top - h - 6;
  tip.style.left = `${Math.round(left)}px`;
  tip.style.top = `${Math.round(top)}px`;
}

function hide() {
  current = null;
  tip.classList.remove("show");
}

document.addEventListener("mouseover", (e) => {
  const el = e.target.closest("[title], [data-tip]");
  if (!el) {
    if (current) hide();
    return;
  }
  if (el !== current) show(el);
});
document.addEventListener("mouseout", (e) => {
  if (current && !current.contains(e.relatedTarget)) hide();
});
document.addEventListener("scroll", hide, true);
