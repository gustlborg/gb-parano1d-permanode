import { api } from "./api.js";
import { noid, int, escapeHtml } from "./format.js";

// The halving page. Parano1d has no height-based halvings: the reward
// halves when the live state expands, which consensus triggers once a
// strict majority of the 18 hard-finalized headers (tip-35 ..= tip-18)
// report at least 75% occupancy of the current 2^log_slots domain. So the
// page is about live UTXOs against that threshold - never about a date.

const W = 1000;
const H = 300;
const PL = 64;
const PR = 58;
const PT = 18;
const PB = 12;
const IW = W - PL - PR;
const IH = H - PT - PB;
const RECENT_BACK = 60_000;
const RECENT_AHEAD = 30_000;
const RATE_WINDOW = 10_000;
const REFRESH_MS = 5000;
const FLOOR_MICRONOID = 1_000_000;

function rewardAt(logSlots) {
  const halvings = Math.max(0, logSlots - 24);
  return Math.max(FLOOR_MICRONOID, Math.floor(50_000_000 / 2 ** halvings));
}

function short(v) {
  if (v >= 1e7) return `${(v / 1e6).toFixed(0)}M`;
  if (v >= 1e6) return `${(v / 1e6).toFixed(1)}M`;
  if (v >= 1e3) return `${(v / 1e3).toFixed(0)}k`;
  return `${Math.round(v)}`;
}

function pct(x, digits = 2) {
  return `${(x * 100).toFixed(digits)}%`;
}

// Net-new live slots per block over the most recent RATE_WINDOW blocks of
// sampled history (0 if the state shrank).
function growthRate(history, tip) {
  const recent = history.filter(([h]) => h >= tip - RATE_WINDOW);
  if (recent.length < 2) return 0;
  const [h0, a0] = recent[0];
  const [h1, a1] = recent[recent.length - 1];
  return h1 > h0 ? Math.max(0, (a1 - a0) / (h1 - h0)) : 0;
}

function chartHtml(d, mode) {
  const tip = d.tip;
  const rate = growthRate(d.history, tip);
  const remaining = Math.max(0, d.threshold - d.active_slots);
  const crossH = rate > 0 ? tip + Math.ceil(remaining / rate) : null;
  const activeAt = (h) => {
    // linear interpolation on the sampled history
    const pts = d.history;
    if (h <= pts[0][0]) return pts[0][1];
    for (let i = 1; i < pts.length; i++) {
      if (pts[i][0] >= h) {
        const [h0, a0] = pts[i - 1];
        const [h1, a1] = pts[i];
        return h1 === h0 ? a1 : a0 + ((a1 - a0) * (h - h0)) / (h1 - h0);
      }
    }
    return pts[pts.length - 1][1];
  };
  let x0;
  let x1;
  let yMax;
  if (mode === "full") {
    x0 = 0;
    x1 = crossH ?? tip + RECENT_AHEAD;
    yMax = d.threshold * 1.22;
  } else {
    x0 = Math.max(0, tip - RECENT_BACK);
    x1 = tip + RECENT_AHEAD;
    const inRange = d.history.filter(([h]) => h >= x0).map(([, a]) => a);
    yMax = Math.max(d.active_slots + (x1 - tip) * rate, ...inRange, 1) * 1.18;
  }
  const px = (h) => PL + ((h - x0) / (x1 - x0)) * IW;
  const py = (v) => PT + IH - (Math.min(v, yMax) / yMax) * IH;

  const hist = d.history.filter(([h]) => h >= x0 && h <= tip).map(([h, a]) => [px(h), py(a)]);
  if (!hist.length || hist[0][0] > px(x0) + 1) hist.unshift([px(x0), py(activeAt(x0))]);
  hist.push([px(tip), py(d.active_slots)]);
  const proj = [
    [px(tip), py(d.active_slots)],
    [px(x1), py(d.active_slots + (x1 - tip) * rate)],
  ];
  const path = (pts) => pts.map((p, i) => `${i ? "L" : "M"}${p[0].toFixed(1)} ${p[1].toFixed(1)}`).join(" ");
  const area = `${path(hist)} L ${px(tip).toFixed(1)} ${PT + IH} L ${PL} ${PT + IH} Z`;

  const gridY = [];
  for (let i = 0; i <= 4; i++) {
    const v = (yMax / 4) * i;
    gridY.push({ y: py(v), label: short(v), pct: pct(v / d.capacity, v / d.capacity < 0.02 ? 2 : 0) });
  }
  const gridX = [];
  for (let i = 0; i <= 5; i++) {
    const h = Math.round(x0 + ((x1 - x0) / 5) * i);
    gridX.push({ x: px(h), label: `#${short(h)}` });
  }
  const thrVisible = d.threshold <= yMax;
  const thrY = py(d.threshold);

  return `<svg class="hchart" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" aria-label="Live UTXOs over block height">
      <defs>
        <linearGradient id="utxoFill" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stop-color="var(--accent)" stop-opacity="0.26"></stop>
          <stop offset="100%" stop-color="var(--accent)" stop-opacity="0"></stop>
        </linearGradient>
      </defs>
      ${gridY.map((g) => `<line x1="${PL}" x2="${W - PR}" y1="${g.y.toFixed(1)}" y2="${g.y.toFixed(1)}" stroke="var(--border-row)" stroke-width="1"></line>`).join("")}
      <path d="${area}" fill="url(#utxoFill)"></path>
      <path d="${path(proj)}" fill="none" stroke="var(--accent)" stroke-opacity="0.34" stroke-width="2" stroke-dasharray="6 6" vector-effect="non-scaling-stroke"></path>
      <path d="${path(hist)}" fill="none" stroke="var(--accent)" stroke-width="2.4" stroke-linejoin="round" stroke-linecap="round" vector-effect="non-scaling-stroke"></path>
      ${thrVisible ? `<line x1="${PL}" x2="${W - PR}" y1="${thrY.toFixed(1)}" y2="${thrY.toFixed(1)}" stroke="var(--accent-2)" stroke-width="1.4" stroke-dasharray="5 5" vector-effect="non-scaling-stroke"></line>` : ""}
      <line x1="${px(tip).toFixed(1)}" x2="${px(tip).toFixed(1)}" y1="${PT}" y2="${PT + IH}" stroke="var(--arrow)" stroke-width="1" vector-effect="non-scaling-stroke"></line>
    </svg>
    <span class="hnow" style="left:${((px(tip) / W) * 100).toFixed(2)}%;top:${((py(d.active_slots) / H) * 100).toFixed(2)}%"><i></i></span>
    ${gridY.map((g) => `<span class="hy" style="top:${((g.y / H) * 100).toFixed(2)}%">${g.label}</span><span class="hy right" style="top:${((g.y / H) * 100).toFixed(2)}%">${g.pct}</span>`).join("")}
    ${gridX.map((g) => `<span class="hx" style="left:${((g.x / W) * 100).toFixed(2)}%">${g.label}</span>`).join("")}
    ${thrVisible ? `<span class="hy right thr" style="top:${((thrY / H) * 100).toFixed(2)}%">75%</span>` : ""}`;
}

function cardsHtml(d) {
  const occ = d.active_slots / d.capacity;
  const remaining = Math.max(0, d.threshold - d.active_slots);
  const nextReward = noid(rewardAt(d.log_slots + 1));
  const card = (v, k, sub) => `<div class="stat"><span class="v">${v}</span><span class="k">${k}</span><span class="sub">${sub}</span></div>`;
  return [
    card(`2^${d.log_slots}`, "State domain", `${int(d.capacity)} slots`),
    card(int(d.active_slots), "Live UTXOs", "active slots"),
    card(pct(occ, 3), "State occupancy", `threshold ${d.trigger_pct}%`),
    card(int(d.threshold), "Live UTXOs for expansion", `${d.trigger_pct}% of 2^${d.log_slots}`),
    card(int(remaining), "Until the threshold", "net-new slots"),
    card(`${noid(d.block_reward_micronoid, false)} → ${nextReward}`, "Reward at expansion", `2^${d.log_slots} → 2^${d.log_slots + 1}`),
  ].join("");
}

function windowHtml(d) {
  const qualifying = d.window.filter((w) => w.qualifies).length;
  const pips = d.window
    .map(
      (w) =>
        `<i class="${w.qualifies ? "on" : ""}" title="#${w.height}: ${int(w.active_slot_count)} live UTXOs (${pct(w.active_slot_count / d.capacity, 3)})"></i>`
    )
    .join("");
  return `<div class="trigger-head">
      <span class="label">Finalized trigger window</span>
      <span class="mono hi-text">${qualifying} / ${d.window_required} qualifying headers</span>
      <span class="mono dim2">${d.window_size} hard-finalized headers (#${d.window[0]?.height ?? "-"} to #${d.window[d.window.length - 1]?.height ?? "-"}) · ≥ ${d.window_required} with ≥ ${d.trigger_pct}% occupancy required</span>
    </div>
    <div class="pips">${pips}</div>`;
}

function tiersHtml(d) {
  const rows = [];
  for (let n = 24; n <= 32; n++) {
    const cls = n === d.log_slots ? "current" : n === d.log_slots + 1 ? "next" : "";
    const tag = n === d.log_slots ? '<span class="tag">current</span>' : n === d.log_slots + 1 ? '<span class="tag">next</span>' : "";
    const reward = n >= 30 ? "1 NOID (floor)" : noid(rewardAt(n));
    rows.push(`<div class="tier ${cls}"><span>2^${n}</span><span>${int(2 ** n)} ${tag}</span><span>${reward}</span></div>`);
  }
  return rows.join("");
}

function rowsHtml(d, rate) {
  const qualifying = d.window.filter((w) => w.qualifies).length;
  const kv = (k, v, cls = "t2") => `<div><span class="k">${k}</span><span class="v ${cls}">${v}</span></div>`;
  return [
    kv("Expansion trigger", `≥ ${d.trigger_pct}% occupancy in ≥ ${d.window_required} of ${d.window_size} hard-finalized headers`),
    kv("Finalized window", `${qualifying} / ${d.window_required} qualifying headers in the current ${d.window_size}-header window`),
    kv("Reorgable tip", "does not count - only hard-finalized headers (18 confirmations and deeper)", "t3"),
    kv("Expansion", `first child block after the trigger is met: 2^${d.log_slots} → 2^${d.log_slots + 1}`, "hi"),
    kv("Domain range", "2^24 up to 2^32 · reward floor 1 NOID from 2^30 on"),
    kv("State-growth fee", "net-new live slots pay a growth fee whose multiplier rises with state pressure"),
    kv("Fee use", "the growth component is burned", "pos"),
    kv("Estimate", rate > 0 ? `projection from recent UTXO growth (${rate.toFixed(3)} net-new slots per block over the last ${int(RATE_WINDOW)} blocks) - not a protocol date` : "the live state is not growing right now - no projection", "t3"),
  ].join("");
}

export async function halvingView() {
  let d = await api.halving();
  let mode = "recent";

  const html = `<div class="wrap"><div class="page">
    <a class="back" href="/" data-link>← back to chain</a>
    <div class="card pad stack wide" id="halving">
      <div class="stack tight">
        <div class="title-row base">
          <h1 class="title block">Next State Expansion <em>/ Reward Halving</em></h1>
          <span class="scale-tabs" id="scale-tabs"><button type="button" class="scale-tab active" data-mode="recent">Recent growth</button><button type="button" class="scale-tab" data-mode="full">Full scale</button></span>
        </div>
        <p class="note">Reward halvings are triggered by sustained live-state occupancy, not by block height.</p>
      </div>
      <div class="stack tight" id="progress"></div>
      <div class="chart-box">
        <div class="chart" id="chart"></div>
        <div class="chart-legend">
          <span>Live UTXOs · right: state occupancy</span>
          <span class="legend-items">
            <span class="t2"><i class="sw solid"></i>measured</span>
            <span><i class="sw dashed"></i>estimate from recent growth</span>
            <span class="pos"><i class="sw thr"></i>expansion threshold</span>
          </span>
          <span>Block height</span>
        </div>
      </div>
      <div class="stats inner six" id="halving-cards"></div>
      <div class="trigger" id="trigger"></div>
    </div>
    <div class="card">
      <div class="card-head"><h2>Reward tiers</h2></div>
      <div class="tier thead-like"><span>Domain</span><span>Slots</span><span>Block reward</span></div>
      <div id="tiers"></div>
    </div>
    <div class="card pad stack">
      <span class="label">Consensus &amp; emission</span>
      <div class="kv wide" id="rows"></div>
    </div>
  </div></div>`;

  function mount(root) {
    const progress = root.querySelector("#progress");
    const chart = root.querySelector("#chart");
    const cards = root.querySelector("#halving-cards");
    const trigger = root.querySelector("#trigger");
    const tiers = root.querySelector("#tiers");
    const rows = root.querySelector("#rows");
    const tabs = root.querySelector("#scale-tabs");

    function render() {
      const prog = Math.min(1, d.active_slots / d.threshold);
      progress.innerHTML = `<div class="progress"><i style="width:${Math.max(0.4, prog * 100).toFixed(3)}%"></i></div>
        <div class="progress-labels">
          <span>Live State · ${int(d.active_slots)} UTXOs</span>
          <span class="hi">${pct(prog)} to the threshold</span>
          <span>${d.trigger_pct}% · ${int(d.threshold)}</span>
        </div>`;
      chart.innerHTML = chartHtml(d, mode);
      cards.innerHTML = cardsHtml(d);
      trigger.innerHTML = windowHtml(d);
      tiers.innerHTML = tiersHtml(d);
      rows.innerHTML = rowsHtml(d, growthRate(d.history, d.tip));
    }
    render();

    const onTab = (e) => {
      const btn = e.target.closest(".scale-tab");
      if (!btn) return;
      mode = btn.dataset.mode;
      tabs.querySelectorAll(".scale-tab").forEach((b) => b.classList.toggle("active", b === btn));
      chart.innerHTML = chartHtml(d, mode);
    };
    tabs.addEventListener("click", onTab);

    let inflight = false;
    const timer = setInterval(async () => {
      if (inflight) return;
      inflight = true;
      try {
        const fresh = await api.halving();
        if (fresh.tip !== d.tip || fresh.active_slots !== d.active_slots) {
          d = fresh;
          render();
        }
      } catch {
        /* keep the last state */
      } finally {
        inflight = false;
      }
    }, REFRESH_MS);

    return () => {
      clearInterval(timer);
      tabs.removeEventListener("click", onTab);
    };
  }

  return { html, mount };
}
