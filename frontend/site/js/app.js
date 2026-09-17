import { api } from "./api.js";
import { homeView, blockView, txView, addressView, tickerHtml, notFoundHtml } from "./views.js";

const app = document.getElementById("app");
const ticker = document.getElementById("ticker");

async function renderTicker() {
  try {
    ticker.innerHTML = tickerHtml(await api.stats());
  } catch {
    ticker.innerHTML = "";
  }
}

async function search(query) {
  query = query.trim();
  if (!query) return;
  if (/^\d+$/.test(query)) return navigate(`/block/${query}`);
  if (query.startsWith("o1")) return navigate(`/address/${query}`);
  // Unknown hex string: try block hash, then fall back to tx.
  const block = await api.blockByHash(query);
  if (block) return navigate(`/block/${query}`);
  return navigate(`/tx/${query}`);
}

async function render() {
  const path = location.pathname;
  const params = new URLSearchParams(location.search);
  app.innerHTML = '<div class="loading">Loading…</div>';

  try {
    let html;
    if (path === "/" || path === "") {
      html = await homeView();
    } else if (path.startsWith("/block/")) {
      html = await blockView(decodeURIComponent(path.slice("/block/".length)));
    } else if (path.startsWith("/tx/")) {
      html = await txView(decodeURIComponent(path.slice("/tx/".length)));
    } else if (path.startsWith("/address/")) {
      const page = parseInt(params.get("page") || "1", 10) || 1;
      html = await addressView(decodeURIComponent(path.slice("/address/".length)), page);
    } else {
      html = notFoundHtml(`Unknown page: ${path}`);
    }
    app.innerHTML = html;
  } catch (e) {
    console.error(e);
    app.innerHTML = notFoundHtml(`Failed to load: ${e.message}`);
  }
}

function navigate(path) {
  history.pushState(null, "", path);
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

render();
renderTicker();
setInterval(renderTicker, 20_000);
