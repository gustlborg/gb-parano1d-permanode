import { api } from "./api.js";
import {
  homeView,
  blocksView,
  blockView,
  txView,
  addressView,
  mempoolView,
  richlistView,
  aboutView,
  tickerHtml,
  notFoundHtml,
} from "./views.js";
import * as health from "./health.js";
import "./tooltip.js";

const app = document.getElementById("app");
const ticker = document.getElementById("ticker");
const navEl = document.getElementById("nav");
let viewCleanup = null;
let currentView = "home";

// The nav doubles as "current selection": Block/Transaction/Address point
// at whatever was last opened in this session (Block falls back to the
// tip), so the active entry always leads somewhere sensible.
const selected = { block: null, tx: null, address: null };

function navItems() {
  return [
    { key: "home", label: "Dashboard", href: "/" },
    { key: "block", label: "Block", href: selected.block ? `/block/${selected.block}` : "/block/latest" },
    { key: "tx", label: "Transaction", href: selected.tx ? `/tx/${selected.tx}` : null },
    { key: "address", label: "Address", href: selected.address ? `/address/${selected.address}` : null },
    { key: "mempool", label: "Mempool", href: "/mempool" },
    { key: "richlist", label: "Rich list", href: "/richlist" },
  ];
}

function renderNav() {
  navEl.innerHTML = navItems()
    .map((n) => {
      const active = n.key === currentView ? ' class="active"' : "";
      return n.href
        ? `<a href="${n.href}" data-link data-view="${n.key}"${active}>${n.label}</a>`
        : `<span data-view="${n.key}"${active} title="Open ${n.key === "address" ? "an address" : "a transaction"} first">${n.label}</span>`;
    })
    .join("");
}

function renderLive(state) {
  const live = document.getElementById("live");
  const lost = document.getElementById("conn-lost");
  if (live) live.classList.toggle("degraded", state.degraded);
  if (lost) lost.textContent = state.lost ? "connection lost — retrying" : "";
}

async function renderTicker() {
  try {
    ticker.innerHTML = tickerHtml(await api.stats());
    health.reportOk();
  } catch {
    if (!document.getElementById("live")) ticker.innerHTML = tickerHtml(null);
    health.reportFail();
  }
  renderLive(health.state());
}

async function search(query) {
  query = query.trim();
  if (!query) return;
  if (/^\d+$/.test(query)) return navigate(`/block/${query}`);
  if (query.startsWith("o1")) return navigate(`/address/${query}`);
  // Unknown hex string: try block hash, then fall back to tx.
  const block = await api.blockByHash(query).catch(() => null);
  if (block) return navigate(`/block/${query}`);
  return navigate(`/tx/${query}`);
}

async function render() {
  if (viewCleanup) {
    viewCleanup();
    viewCleanup = null;
  }

  const path = location.pathname;
  const params = new URLSearchParams(location.search);
  app.innerHTML = '<div class="loading">Loading…</div>';

  try {
    let result;
    if (path === "/" || path === "") {
      currentView = "home";
      result = await homeView();
    } else if (path === "/blocks") {
      currentView = "block";
      result = await blocksView();
    } else if (path === "/mempool") {
      currentView = "mempool";
      result = await mempoolView();
    } else if (path === "/richlist") {
      currentView = "richlist";
      result = await richlistView();
    } else if (path === "/about") {
      currentView = "about";
      result = await aboutView();
    } else if (path.startsWith("/block/")) {
      currentView = "block";
      let id = decodeURIComponent(path.slice("/block/".length));
      if (id === "latest") {
        const [tip] = await api.blocks(1);
        id = String(tip.height);
        history.replaceState(null, "", `/block/${id}`);
      }
      result = await blockView(id);
      if (result && typeof result === "object") selected.block = id;
    } else if (path.startsWith("/tx/")) {
      currentView = "tx";
      const id = decodeURIComponent(path.slice("/tx/".length));
      result = await txView(id);
      if (result && typeof result === "object") selected.tx = id;
    } else if (path.startsWith("/address/")) {
      currentView = "address";
      const pageNo = parseInt(params.get("page") || "1", 10) || 1;
      const pageSize = parseInt(params.get("size") || "25", 10) || 25;
      const address = decodeURIComponent(path.slice("/address/".length));
      result = await addressView(address, pageNo, pageSize);
      selected.address = address;
    } else {
      currentView = "";
      result = notFoundHtml(`Unknown page: ${path}`);
    }

    if (result && typeof result === "object" && "html" in result) {
      app.innerHTML = result.html;
      if (result.mount) viewCleanup = result.mount(app) || null;
    } else {
      app.innerHTML = result;
    }
  } catch (e) {
    console.error(e);
    app.innerHTML = notFoundHtml(`Failed to load: ${e.message}`);
  }
  renderNav();
}

function navigate(path) {
  history.pushState(null, "", path);
  window.scrollTo(0, 0);
  render();
}

document.addEventListener("click", (e) => {
  const a = e.target.closest("a[data-link]");
  if (!a) return;
  const url = new URL(a.href);
  if (url.origin !== location.origin) return;
  e.preventDefault();
  navigate(url.pathname + url.search);
});

window.addEventListener("popstate", render);

document.getElementById("search-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const input = document.getElementById("search-input");
  search(input.value);
  input.value = "";
});

health.onChange(renderLive);
renderNav();
render();
renderTicker();
setInterval(renderTicker, 20_000);
