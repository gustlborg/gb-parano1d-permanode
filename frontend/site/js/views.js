import { api } from "./api.js";
import { noid, int, shortHash, timeAgo, fullTime, escapeHtml, hashrate, seconds, isoToUnix, feeRateOf } from "./format.js";
import { pagesOf, cellShades, cellsHtml, applyCells } from "./cells.js";
import {
  CHAIN_BLOCKS,
  blockTileHtml,
  mempoolTilesHtml,
  updateMempoolTiles,
  slideTrack,
  scheduleFlashEnd,
  etaText,
} from "./chain.js";
import * as health from "./health.js";

const LIVE_REFRESH_MS = 1000;
const STATS_REFRESH_MS = 20_000;
const ALL_BLOCKS_LIMIT = 200;
// The protocol's maximum reorg depth is 17 blocks; from 18 on a block is final.
const FINAL_CONFIRMATIONS = 18;

function link(href, text, cls = "") {
  return `<a href="${href}" data-link${cls ? ` class="${cls}"` : ""}>${escapeHtml(text)}</a>`;
}
function addrLink(address, full = false) {
  return link(`/address/${address}`, full ? address : shortHash(address));
}
function page(inner) {
  return `<div class="wrap"><div class="page">${inner}</div></div>`;
}
function back() {
  return `<a class="back" href="/" data-link>← back to chain</a>`;
}
function hint(text, title) {
  return `<span class="hint" title="${escapeHtml(title)}">${text}</span>`;
}

// Browsers wrap a native title tooltip at literal newlines, so this stays
// narrow instead of one very wide line.
function receiverHint(nOutputs) {
  return `${nOutputs} outputs total, showing the first.
Extra outputs are often change back
to the sender, but the protocol
doesn't guarantee that.`;
}

function kindTag(tx) {
  if (tx.coinbase) return '<span class="tag">coinbase</span>';
  if (tx.development_payout) return '<span class="tag">dev payout</span>';
  return "";
}

// ---- time labels ----------------------------------------------------
// <span class="time-cell" data-ts> follows the table's relative/absolute
// toggle; <span class="ago" data-ts> is always relative (chain tiles,
// timestamps in detail rows). Both are re-rendered every second by the
// view's tick so the wait between blocks visibly grows instead of sitting
// frozen until the next rebuild.
function timeCell(ts) {
  return `<span class="time-cell time-col" data-ts="${ts}"></span>`;
}
function applyTimeFormat(root, absolute) {
  root.querySelectorAll(".time-cell").forEach((el) => {
    const ts = Number(el.dataset.ts);
    el.textContent = absolute ? fullTime(ts) : timeAgo(ts);
  });
  root.querySelectorAll(".ago").forEach((el) => {
    el.textContent = timeAgo(Number(el.dataset.ts));
  });
}
function wireTimeToggle(root, getAbsolute, setAbsolute) {
  const disposers = [];
  root.querySelectorAll(".time-toggle").forEach((el) => {
    const onClick = () => {
      setAbsolute(!getAbsolute());
      applyTimeFormat(root, getAbsolute());
    };
    el.addEventListener("click", onClick);
    disposers.push(() => el.removeEventListener("click", onClick));
  });
  return () => disposers.forEach((d) => d());
}
function timeHeader() {
  return `<span class="time-col"><span class="time-toggle" title="Click to switch between relative and absolute time">Time</span></span>`;
}
// Minimal mount for views whose only live element is the clock.
function tickingMount(root) {
  let absoluteTime = false;
  applyTimeFormat(root, absoluteTime);
  const disposeToggle = wireTimeToggle(root, () => absoluteTime, (v) => (absoluteTime = v));
  const timer = setInterval(() => applyTimeFormat(root, absoluteTime), LIVE_REFRESH_MS);
  return () => {
    clearInterval(timer);
    disposeToggle();
  };
}

// ---- shared tables ----------------------------------------------------
function blocksHead() {
  return `<div class="thead cols-blocks"><span>Height</span>${timeHeader()}<span>Miner</span><span>Txs</span><span>Reward</span><span>Fees</span></div>`;
}
function blocksRows(blocks) {
  if (!blocks.length) return `<div class="trow cols-blocks"><span class="empty">No blocks recorded yet.</span></div>`;
  return blocks
    .map(
      (b) => `<div class="trow cols-blocks">
        <span>${link(`/block/${b.height}`, "#" + b.height)}${b.body_captured ? "" : ' <span class="tag warn" title="Body pruned by the node before this permanode could capture it">no body</span>'}</span>
        ${timeCell(b.timestamp)}
        <span>${addrLink(b.miner)}</span>
        <span>${b.tx_count}</span>
        <span>${noid(b.reward_micronoid)}</span>
        <span class="dim">${b.total_fees_micronoid !== null ? noid(b.total_fees_micronoid) : "-"}</span>
      </div>`
    )
    .join("");
}

function txRow(tx) {
  const sender = tx.input_owner ? addrLink(tx.input_owner) : "—";
  const receiver = tx.receiver ? addrLink(tx.receiver) : "—";
  const extra = tx.n_outputs > 1 ? ` ${hint(`+${tx.n_outputs - 1}`, receiverHint(tx.n_outputs))}` : "";
  return `<div class="trow cols-btx">
      <span class="with-tag">${link(`/tx/${tx.txid}`, shortHash(tx.txid))}${kindTag(tx)}</span>
      <span class="dim">${sender}</span>
      <span>${tx.n_inputs} → ${tx.n_outputs}</span>
      <span>${receiver}${extra}</span>
      <span>${noid(tx.output_sum_micronoid)}</span>
      <span class="dim">${noid(tx.fee_micronoid)}</span>
    </div>`;
}

// ---- dashboard --------------------------------------------------------
function avgBlockTime(stats) {
  const n = stats?.network;
  return n?.avg_block_time_10m_seconds ?? n?.avg_block_time_1h_seconds ?? null;
}

// Whole-supply figures don't fit a stat card at full µNOID precision, so
// this rounds to two decimals; every other amount keeps full precision.
function supply(micronoid) {
  if (micronoid === null || micronoid === undefined) return "-";
  const full = noid(micronoid, false);
  const [whole, frac = ""] = full.split(".");
  return frac ? `${whole}.${frac.slice(0, 2).padEnd(2, "0")}` : full;
}

function statCard(v, k, opts = {}) {
  const title = opts.hint ? ` title="${escapeHtml(opts.hint)}"` : "";
  const cls = opts.hint ? "v hint" : "v";
  return `<div class="stat"${opts.id ? ` id="${opts.id}"` : ""}><span class="${cls}"${title}>${v}</span><span class="k">${k}</span></div>`;
}

function dashboardStatsHtml(stats, mempool) {
  const n = stats?.network || {};
  const blockTimes = `10 min: ${seconds(n.avg_block_time_10m_seconds)} · 24 h: ${seconds(n.avg_block_time_24h_seconds)}
From this permanode's own recorded
blocks, not the node - a fresh install
won't have a 24h figure yet.`;
  return [
    statCard(supply(n.circulating_supply_micronoid), "Circulating supply", { hint: "in NOID" }),
    statCard(noid(n.block_reward_micronoid), "Block reward"),
    statCard(hashrate(n.estimated_hashrate_hs), "Network hashrate", {
      hint: "Rough estimate derived from the\ncurrent PoW target, not a\nmeasured network figure.",
    }),
    statCard(seconds(n.avg_block_time_1h_seconds), "Avg block time (1h)", { hint: blockTimes }),
    statCard(mempool ? int(mempool.size) : "-", "Mempool pending", { id: "stat-mempool" }),
    statCard(mempool ? noid(mempool.fee_floor) : "-", "Fee floor", { id: "stat-floor" }),
  ].join("");
}

function detailKey(summary) {
  return `${summary.height}:${summary.hash}`;
}

export async function homeView() {
  let summaries = await api.blocks(25);
  let mempool = await api.mempool().catch(() => null);
  let stats = await api.stats().catch(() => null);
  const details = new Map();

  const chainSummaries = () => summaries.slice(0, CHAIN_BLOCKS);
  async function loadDetails() {
    const wanted = chainSummaries();
    await Promise.all(
      wanted.map(async (s) => {
        const k = detailKey(s);
        if (!details.has(k)) details.set(k, await api.blockByHeight(s.height).catch(() => null));
      })
    );
    const keep = new Set(wanted.map(detailKey));
    for (const k of details.keys()) if (!keep.has(k)) details.delete(k);
  }
  await loadDetails();

  const trackHtml = (flashHeight) =>
    mempoolTilesHtml(mempool) +
    chainSummaries()
      .map((s) => blockTileHtml(s, details.get(detailKey(s)), s.height === flashHeight))
      .join("");

  const html = `
    <section class="chain">
      <div class="chain-head">
        <h2>Chain</h2>
        <span class="eta" id="eta">${etaText(summaries[0]?.timestamp, avgBlockTime(stats))}</span>
      </div>
      <div class="chain-viewport">
        <div class="chain-scroll"><div class="chain-track" id="chain-track">${trackHtml(null)}</div></div>
        <div class="chain-fade"></div>
      </div>
    </section>
    <div class="wrap">
      <section class="stats" id="stats">${dashboardStatsHtml(stats, mempool)}</section>
      <section class="page">
        <div class="card">
          <div class="card-head"><h2>Recent blocks</h2>${link("/blocks", "all blocks →")}</div>
          <div class="tbl-scroll">${blocksHead()}<div id="recent-blocks">${blocksRows(summaries)}</div></div>
        </div>
      </section>
    </div>`;

  function mount(root) {
    const track = root.querySelector("#chain-track");
    const eta = root.querySelector("#eta");
    let tip = summaries[0]?.height ?? null;
    let tipHash = summaries[0]?.hash ?? null;
    let absoluteTime = false;
    let timeToggleDisposer = () => {};
    let inflight = false;

    function refreshTimeToggle() {
      timeToggleDisposer();
      applyTimeFormat(root, absoluteTime);
      timeToggleDisposer = wireTimeToggle(root, () => absoluteTime, (v) => (absoluteTime = v));
    }
    function renderEta() {
      eta.textContent = etaText(summaries[0]?.timestamp, avgBlockTime(stats));
    }
    function renderStats() {
      const el = root.querySelector("#stats");
      if (el) el.innerHTML = dashboardStatsHtml(stats, mempool);
    }
    refreshTimeToggle();

    const timer = setInterval(async () => {
      applyTimeFormat(root, absoluteTime);
      renderEta();
      if (inflight) return;
      inflight = true;
      try {
        const [newSummaries, newMempool] = await Promise.all([api.blocks(25), api.mempool()]);
        health.reportOk();
        mempool = newMempool;
        summaries = newSummaries;
        const newTip = summaries[0]?.height ?? null;
        const newTipHash = summaries[0]?.hash ?? null;
        // A reorg swaps the tip hash without raising the height - rebuild
        // the chain then too, just without the "new block" slide.
        if (newTip !== tip || newTipHash !== tipHash) {
          const advanced = newTip !== null && (tip === null || newTip > tip);
          tip = newTip;
          tipHash = newTipHash;
          await loadDetails();
          track.innerHTML = trackHtml(advanced ? newTip : null);
          if (advanced) {
            slideTrack(track);
            scheduleFlashEnd(track);
          }
          const table = root.querySelector("#recent-blocks");
          if (table) table.innerHTML = blocksRows(summaries);
          refreshTimeToggle();
        } else if (!updateMempoolTiles(track, mempool)) {
          track.innerHTML = trackHtml(null);
          applyTimeFormat(root, absoluteTime);
        }
        const pending = root.querySelector("#stat-mempool .v");
        if (pending) pending.textContent = int(mempool.size);
        const floor = root.querySelector("#stat-floor .v");
        if (floor) floor.textContent = noid(mempool.fee_floor);
      } catch {
        health.reportFail();
      } finally {
        inflight = false;
      }
    }, LIVE_REFRESH_MS);

    const statsTimer = setInterval(async () => {
      try {
        stats = await api.stats();
        renderStats();
      } catch {
        /* the 1s poll already tracks reachability */
      }
    }, STATS_REFRESH_MS);

    return () => {
      clearInterval(timer);
      clearInterval(statsTimer);
      timeToggleDisposer();
    };
  }

  return { html, mount };
}

export async function blocksView() {
  const blocks = await api.blocks(ALL_BLOCKS_LIMIT);
  const html = page(`
    ${back()}
    <div class="card">
      <div class="card-head"><h2>Latest ${blocks.length} blocks</h2></div>
      <div class="tbl-scroll">${blocksHead()}${blocksRows(blocks)}</div>
    </div>`);
  return { html, mount: tickingMount };
}

// ---- status bar -------------------------------------------------------
export function tickerHtml(stats) {
  const live = `<span class="live" id="live"><span class="dot"></span>live</span><span class="conn-lost" id="conn-lost"></span>`;
  if (!stats) return live;
  const n = stats.network || {};
  const gaps =
    stats.gaps > 0 || stats.gaps_resolved > 0
      ? hint(
          `Gaps <b>${int(stats.gaps)}</b>${stats.gaps_resolved > 0 ? ` <span class="dim">+${int(stats.gaps_resolved)} recovered</span>` : ""}`,
          "Blocks whose body the node pruned before this permanode could capture it - permanently gone.\nRecovered: gaps the getBlock fallback decoder filled in afterwards."
        )
      : "";
  return `
    <span>Tip <b>#${stats.last_processed_height ?? "-"}</b></span>
    <span>Blocks <b>${int(stats.indexed_blocks)}</b></span>
    <span>Transactions <b>${int(stats.indexed_transactions)}</b></span>
    ${hint(`Live UTXOs <b>${int(stats.live_utxos)}</b>`, "Only what this permanode has itself recorded\nas created and still unspent since it started\nindexing - not the network-wide total.")}
    ${n.active_slots != null ? hint(`Network UTXOs <b>${int(n.active_slots)}</b>`, "The node's own count across its entire\nhistory since genesis, for comparison.") : ""}
    <span>History since <b>${stats.oldest_retained_timestamp ? fullTime(stats.oldest_retained_timestamp) : "-"}</b></span>
    ${gaps}
    <span>Avg block time <b>${seconds(n.avg_block_time_1h_seconds)}</b></span>
    ${stats.decoder_mismatches > 0 ? hint(`Decoder mismatches <b>${stats.decoder_mismatches}</b>`, "Times the fallback decoder's output disagreed with the node's own getBlockDetails for a block both could decode - should be 0.") : ""}
    ${live}`;
}

// ---- block ------------------------------------------------------------
function kvRow(k, v, cls = "") {
  return `<div><span class="k">${k}</span><span class="v${cls ? " " + cls : ""}">${v}</span></div>`;
}

export async function blockView(idParam) {
  const isHeight = /^\d+$/.test(idParam);
  const block = isHeight ? await api.blockByHeight(idParam) : await api.blockByHash(idParam);
  if (!block) return notFoundHtml(`Block ${idParam} not found (or not canonical).`);

  const shades = cellShades(pagesOf(block.transactions), false);
  const rows = [
    kvRow("Hash", block.hash),
    kvRow("Parent", link(`/block/${block.prev_hash}`, block.prev_hash), "hi"),
    kvRow("Timestamp", `${fullTime(block.timestamp)} (<span class="ago" data-ts="${block.timestamp}"></span>)`, "t2"),
    kvRow(
      "Confirmations",
      block.confirmations == null
        ? "-"
        : block.confirmations >= FINAL_CONFIRMATIONS
        ? `${int(block.confirmations)} <span class="tag" title="Beyond the protocol's maximum reorg depth of 17 blocks">final</span>`
        : `${block.confirmations} <span class="dim">(final at ${FINAL_CONFIRMATIONS})</span>`,
      "t2"
    ),
    kvRow("Miner", addrLink(block.miner, true)),
    kvRow("Proof class", block.proof_class ?? "-", "t2"),
    kvRow("Reward", noid(block.reward_micronoid), "t2"),
    kvRow("Total fees", block.total_fees_micronoid !== null ? noid(block.total_fees_micronoid) : "-", "t2"),
    block.body_captured
      ? kvRow("Body captured", "yes", "pos")
      : kvRow("Body captured", '<span class="tag warn">no — pruned by the node before capture</span>'),
    kvRow("State root", block.state_root, "t3"),
    kvRow("Tx root", block.tx_root, "t3"),
    kvRow("Nonce", block.nonce_hex, "t3"),
    kvRow("Difficulty target", block.difficulty_target, "t3"),
  ].join("");
  const txRows = block.transactions.map(txRow).join("");

  const html = page(`
    ${back()}
    <div class="card pad split">
      <div class="visual">
        <div class="face big">${cellsHtml(shades)}</div>
        <span class="face-caption" title="Each lit cell is one page of block space. Cells are ordered with the highest fee rate first; brighter = higher fee rate.">packed by size, shaded by fee rate</span>
      </div>
      <div class="grow">
        <h1 class="title block">Block <em>#${block.height}</em></h1>
        <div class="kv">${rows}</div>
      </div>
    </div>
    <div class="card">
      <div class="card-head"><h2>Transactions (${block.transactions.length})</h2></div>
      <div class="tbl-scroll">
        <div class="thead cols-btx"><span>Txid</span><span>Sender</span><span>In → out</span><span>Receiver</span><span>Amount</span><span>Fee</span></div>
        ${txRows || '<div class="trow cols-btx"><span class="empty">No transactions recorded for this block.</span></div>'}
      </div>
    </div>`);
  return { html, mount: tickingMount };
}

// ---- transaction ------------------------------------------------------
export async function txView(txid) {
  const tx = await api.tx(txid);
  if (!tx) return notFoundHtml(`Transaction ${txid} not found.`);

  const inputs =
    tx.inputs
      .map((i) => `<div class="io-row"><span class="dim">slot ${i.slot_index}</span><span>${noid(i.amount_micronoid)}</span></div>`)
      .join("") || '<div class="io-row"><span class="dim">none (coinbase)</span><span></span></div>';
  const outputs = tx.outputs
    .map((o) => `<div class="io-row"><span>${addrLink(o.owner)}</span><span>${noid(o.amount_micronoid)}</span></div>`)
    .join("");
  const receivers =
    tx.outputs.map((o) => `${addrLink(o.owner, true)} (${noid(o.amount_micronoid, false)})`).join("<br>") || "—";
  const type = tx.coinbase
    ? '<span class="tag">coinbase</span> block reward'
    : tx.development_payout
    ? '<span class="tag">dev payout</span> development payout'
    : "transfer";
  const status = !tx.block.canonical
    ? '<span class="status bad" title="The block containing this transaction was later replaced by a reorg">orphaned</span>'
    : tx.confirmations >= FINAL_CONFIRMATIONS
    ? `<span class="status" title="${FINAL_CONFIRMATIONS} or more confirmations - beyond the protocol's maximum reorg depth">final · ${int(tx.confirmations)} confirmations</span>`
    : `<span class="status" title="Final at ${FINAL_CONFIRMATIONS} confirmations">confirmed · ${tx.confirmations ?? "?"} confirmation${tx.confirmations === 1 ? "" : "s"}</span>`;

  const rows = [
    kvRow("Txid", tx.txid),
    kvRow("Block", link(`/block/${tx.block.height}`, "#" + tx.block.height), "hi"),
    kvRow("Time", `${fullTime(tx.block.timestamp)} (<span class="ago" data-ts="${tx.block.timestamp}"></span>)`, "t2"),
    kvRow("Type", type, "t2"),
    kvRow("Sender", tx.input_owner ? addrLink(tx.input_owner, true) : "—"),
    kvRow(`Receiver${tx.outputs.length > 1 ? "s" : ""}`, receivers),
    kvRow("Fee", noid(tx.fee_micronoid), "t2"),
    kvRow("Fee rate", `${int(feeRateOf({ ...tx, n_inputs: tx.inputs.length, n_outputs: tx.outputs.length }))} µNOID/wu`, "t2"),
    kvRow("Input sum", noid(tx.input_sum_micronoid), "t2"),
    kvRow("Output sum", noid(tx.output_sum_micronoid), "t2"),
    kvRow("Pages", tx.page_count, "t2"),
    kvRow("Epoch anchor", tx.epoch_anchor, "t3"),
  ].join("");

  const html = page(`
    ${back()}
    <div class="card pad stack">
      <div class="title-row"><h1 class="title">Transaction</h1>${status}</div>
      <div class="kv">${rows}</div>
    </div>
    <div class="io">
      <div class="card"><div class="card-head"><h2>Inputs (${tx.inputs.length})</h2></div>${inputs}</div>
      <div class="card"><div class="card-head"><h2>Outputs (${tx.outputs.length})</h2></div>${outputs}</div>
    </div>
    <div class="card">
      <div class="card-head"><h2>Merkle path (receipt data)</h2></div>
      <div class="hash-list">${tx.page_hashes.map((h) => `<div class="io-row"><span>${h}</span></div>`).join("") || '<div class="io-row"><span class="dim">none</span></div>'}</div>
    </div>`);
  return { html, mount: tickingMount };
}

// ---- address ----------------------------------------------------------
function addressTxRow(tx, viewedAddress) {
  // The address page only lists transactions where the viewed address is
  // either the sender or (at least) one of the receivers, so "not the
  // sender" reliably means "incoming" here.
  const incoming = tx.input_owner !== viewedAddress;
  let party;
  if (tx.coinbase || tx.development_payout) {
    party = "—";
  } else if (incoming) {
    party = tx.input_owner ? addrLink(tx.input_owner) : "—";
  } else {
    const extra = tx.n_outputs > 1 ? ` ${hint(`+${tx.n_outputs - 1}`, receiverHint(tx.n_outputs))}` : "";
    party = (tx.receiver ? addrLink(tx.receiver) : "—") + extra;
  }
  const amount = incoming ? `+${noid(tx.output_sum_micronoid)}` : `−${noid(tx.output_sum_micronoid)}`;
  return `<div class="trow cols-atx">
      <span class="with-tag">${link(`/tx/${tx.txid}`, shortHash(tx.txid))}${kindTag(tx)}</span>
      ${timeCell(tx.timestamp)}
      <span>${link(`/block/${tx.height}`, "#" + tx.height)}</span>
      <span class="dim">${party}</span>
      <span>${tx.n_inputs} → ${tx.n_outputs}</span>
      <span class="${incoming ? "pos" : ""}">${amount}</span>
      <span class="dim">${noid(tx.fee_micronoid)}</span>
    </div>`;
}

function liveUtxosBody(utxos) {
  const rows = utxos
    .map(
      (u) => `<div class="trow cols-utxo"><span>${u.slot_index}</span><span>${noid(u.value)}</span><span class="dim">${u.creation_id}</span></div>`
    )
    .join("");
  return `<div class="tbl-scroll">
      <div class="thead cols-utxo"><span>Slot</span><span>Amount</span><span>Creation ID</span></div>
      ${rows || '<div class="trow cols-utxo"><span class="empty">No unspent outputs.</span></div>'}
    </div>`;
}

export async function addressView(address, pageNo = 1) {
  const result = await api.address(address, pageNo, 25);
  const rows = result.transactions.map((tx) => addressTxRow(tx, address)).join("");
  const totalPages = Math.max(1, Math.ceil(result.total / result.page_size));
  const b = result.balance;
  const liveKnown = result.live_balance_micronoid !== null && result.live_balance_micronoid !== undefined;
  const liveHint = "Read live from the node's current\nstate, independent of anything this\npermanode has recorded - the true\nbalance right now.";
  const recHint = "From this permanode's own recorded\nhistory only - transactions it has\nitself seen since it started running.";

  const note =
    result.total === 0
      ? `<p class="note warn">This permanode has recorded no transaction activity for this address since it
         started running - the "recorded" figures are genuinely zero, not missing data. The live balance
         comes straight from the node's current state, so it is accurate even without a history to show.</p>`
      : `<p class="note">${int(result.total)} transaction${result.total === 1 ? "" : "s"} recorded involving this address.</p>`;

  const html = page(`
    ${back()}
    <div class="card pad stack wide">
      <div class="stack tight">
        <span class="label">Address</span>
        <span class="addr-full">${escapeHtml(address)}</span>
      </div>
      <div class="stats inner">
        <div class="stat"><span class="v pos hint" title="${escapeHtml(liveHint)}">${liveKnown ? noid(result.live_balance_micronoid, false) : "?"}</span><span class="k">Current balance (live)</span></div>
        <div class="stat"><span class="v">${liveKnown ? int(result.live_utxo_count) : "?"}</span><span class="k">Current UTXOs (live)</span></div>
        <div class="stat"><span class="v hint" title="${escapeHtml(recHint)}">${noid(b.confirmed_balance_micronoid, false)}</span><span class="k">Recorded balance</span></div>
        <div class="stat"><span class="v">${int(b.confirmed_utxos)}</span><span class="k">Recorded UTXOs</span></div>
        <div class="stat"><span class="v">${noid(b.total_received_micronoid, false)}</span><span class="k">Total received</span></div>
        <div class="stat"><span class="v">${noid(b.total_sent_micronoid, false)}</span><span class="k">Total sent</span></div>
      </div>
      ${note}
    </div>
    <div class="card">
      <div class="tbl-scroll">
        <div class="thead cols-atx"><span>Txid</span>${timeHeader()}<span>Block</span><span>Counterparty</span><span>In → out</span><span>Amount</span><span>Fee</span></div>
        ${rows || '<div class="trow cols-atx"><span class="empty">No transactions recorded.</span></div>'}
      </div>
      ${
        totalPages > 1
          ? `<div class="pager">
              ${pageNo > 1 ? link(`/address/${address}?page=${pageNo - 1}`, "← newer") : ""}
              <span>page ${pageNo} / ${totalPages}</span>
              ${pageNo < totalPages ? link(`/address/${address}?page=${pageNo + 1}`, "older →") : ""}
            </div>`
          : ""
      }
    </div>
    <div class="card" id="live-utxos">
      <div class="card-head"><h2>Live UTXOs</h2><button type="button" class="ghost" id="load-live-utxos">load from node →</button></div>
      <div id="live-utxos-body"><p class="note" style="padding:18px 22px">Not loaded by default to keep this page light. The individual unspent outputs behind the live balance above, straight from the node.</p></div>
    </div>`);

  function mount(root) {
    const disposeTick = tickingMount(root);
    const loadBtn = root.querySelector("#load-live-utxos");
    const body = root.querySelector("#live-utxos-body");
    const head = root.querySelector("#live-utxos .card-head h2");
    const onLoad = async () => {
      loadBtn.disabled = true;
      body.innerHTML = '<p class="loading">Loading…</p>';
      try {
        const utxoResult = await api.addressUtxos(address);
        if (utxoResult.live_utxos !== null) {
          body.innerHTML = liveUtxosBody(utxoResult.live_utxos);
          head.textContent = `Live UTXOs (${utxoResult.live_utxos.length})`;
          loadBtn.textContent = "reload →";
        } else {
          body.innerHTML = '<p class="error">Could not reach the node.</p>';
        }
      } catch (e) {
        body.innerHTML = `<p class="error">Failed to load: ${escapeHtml(e.message)}</p>`;
      } finally {
        loadBtn.disabled = false;
      }
    };
    loadBtn.addEventListener("click", onLoad);
    return () => {
      disposeTick();
      loadBtn.removeEventListener("click", onLoad);
    };
  }

  return { html, mount };
}

// ---- mempool ----------------------------------------------------------
function mempoolStatsHtml(info) {
  const rates = info.txs.map((t) => t.fee_rate);
  const lo = rates.length ? Math.min(...rates) : 0;
  const hi = rates.length ? Math.max(...rates) : 0;
  const range = !rates.length ? "—" : lo === hi ? int(lo) : `${int(lo)} – ${int(hi)}`;
  return `
    <div class="stat"><span class="v">${int(info.size)} tx</span><span class="k">Pending txs</span></div>
    <div class="stat"><span class="v">${noid(info.fee_floor)}</span><span class="k">Fee floor</span></div>
    <div class="stat"><span class="v hint" title="µNOID per weight unit (inputs + outputs + 4 per net new slot) - the node's own mempool priority key">${range}</span><span class="k">Fee rate range</span></div>`;
}

function seenText(admittedHeight, tip) {
  if (!tip || !admittedHeight) return "—";
  const n = Math.max(0, tip - admittedHeight);
  return n === 0 ? "this block" : `${n} block${n === 1 ? "" : "s"} ago`;
}

function mempoolRows(info, tip) {
  const rates = info.txs.map((t) => t.fee_rate);
  const max = rates.length ? Math.max(...rates) : 0;
  const rows = info.txs
    .slice()
    .sort((a, b) => b.fee_rate - a.fee_rate)
    .map((t) => {
      const rel = max > 0 ? t.fee_rate / max : 0;
      const cls = rel >= 0.8 ? "pos" : rel >= 0.4 ? "hi" : "t3";
      return `<div class="trow cols-mem">
        <span>${link(`/tx/${t.tx_hash}`, shortHash(t.tx_hash))}</span>
        <span>${t.n_inputs} → ${t.n_outputs}</span>
        <span>${noid(t.fee_micronoid)}</span>
        <span class="${cls}">${int(t.fee_rate)} µNOID/wu</span>
        <span class="dim" title="admitted at #${t.admitted_height}">${seenText(t.admitted_height, tip)}</span>
      </div>`;
    })
    .join("");
  return rows || '<div class="trow cols-mem"><span class="empty">Mempool is empty.</span></div>';
}

export async function mempoolView() {
  let info = await api.mempool();
  let tipSummary = (await api.blocks(1).catch(() => []))[0] || null;
  let stats = await api.stats().catch(() => null);

  const html = page(`
    ${back()}
    <div class="card pad split">
      <div class="face big hot" id="mempool-face">${cellsHtml(cellShades(pagesOf(info.txs), true))}<div class="scan"></div></div>
      <div class="grow mem">
        <div class="title-row base"><h1 class="title">Live mempool</h1><span class="eta" id="eta">${etaText(tipSummary?.timestamp, avgBlockTime(stats))}</span></div>
        <div class="stats inner three" id="mempool-stats">${mempoolStatsHtml(info)}</div>
      </div>
    </div>
    <div class="card">
      <div class="card-head"><h2>Pending transactions</h2></div>
      <div class="tbl-scroll">
        <div class="thead cols-mem"><span>Txid</span><span>In → out</span><span>Fee</span><span>Fee rate</span><span>Seen</span></div>
        <div id="mempool-rows">${mempoolRows(info, tipSummary?.height)}</div>
      </div>
    </div>`);

  function mount(root) {
    const face = root.querySelector("#mempool-face .cells");
    const eta = root.querySelector("#eta");
    let inflight = false;
    const timer = setInterval(async () => {
      eta.textContent = etaText(tipSummary?.timestamp, avgBlockTime(stats));
      if (inflight) return;
      inflight = true;
      try {
        const [newInfo, blocks] = await Promise.all([api.mempool(), api.blocks(1)]);
        health.reportOk();
        info = newInfo;
        tipSummary = blocks[0] || tipSummary;
        applyCells(face, cellShades(pagesOf(info.txs), true));
        root.querySelector("#mempool-stats").innerHTML = mempoolStatsHtml(info);
        root.querySelector("#mempool-rows").innerHTML = mempoolRows(info, tipSummary?.height);
      } catch {
        health.reportFail();
      } finally {
        inflight = false;
      }
    }, LIVE_REFRESH_MS);
    const statsTimer = setInterval(async () => {
      try {
        stats = await api.stats();
      } catch {
        /* the 1s poll already tracks reachability */
      }
    }, STATS_REFRESH_MS);
    return () => {
      clearInterval(timer);
      clearInterval(statsTimer);
    };
  }

  return { html, mount };
}

// ---- rich list --------------------------------------------------------
export async function richlistView() {
  const entries = await api.richlist();
  const top = entries.length ? Number(entries[0].live_balance_micronoid) : 0;
  const rows = entries
    .map((e, i) => {
      const width = top > 0 ? Math.max(4, Math.round((Number(e.live_balance_micronoid) / top) * 90)) : 4;
      return `<div class="trow cols-rich">
        <span class="rank">${i + 1}</span>
        <span>${link(`/address/${e.address}`, e.address)}</span>
        <span class="bal"><span>${noid(e.live_balance_micronoid)}</span><span class="bar" style="width:${width}px"></span></span>
        <span>${int(e.live_utxo_count)}</span>
        <span class="dim"><span class="ago" data-ts="${isoToUnix(e.fetched_at)}"></span></span>
      </div>`;
    })
    .join("");

  const html = page(`
    ${back()}
    <div class="card">
      <div class="card-title">
        <h1 class="title">Rich list</h1>
        <p class="note">Every address with a live balance in the node's current state, sorted by that balance.
          Found by sweeping the node's UTXO set directly and refreshed periodically in the background, not on every page view.</p>
      </div>
      <div class="tbl-scroll">
        <div class="thead cols-rich"><span>#</span><span>Address</span><span>Balance</span><span>UTXOs</span><span>Updated</span></div>
        ${rows || '<div class="trow cols-rich"><span class="empty">No addresses known yet.</span></div>'}
      </div>
    </div>`);
  return { html, mount: tickingMount };
}

export function notFoundHtml(msg) {
  return page(`${back()}<div class="card"><p class="error">${escapeHtml(msg || "Not found.")}</p></div>`);
}
