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

function bytesText(b) {
  if (b == null) return "-";
  if (b >= 1024 ** 3) return `${(b / 1024 ** 3).toFixed(2)} GB`;
  if (b >= 1024 ** 2) return `${(b / 1024 ** 2).toFixed(1)} MB`;
  return `${Math.round(b / 1024)} KB`;
}

function daysText(seconds) {
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min`;
  if (seconds < 86400) return `${(seconds / 3600).toFixed(1)} h`;
  return `${(seconds / 86400).toFixed(1)} days`;
}

function dashboardStatsHtml(stats, mempool) {
  const n = stats?.network || {};
  const now = Date.now() / 1000;
  const recordedFor = stats?.oldest_retained_timestamp ? now - stats.oldest_retained_timestamp : null;
  const blockTimes = `10 min: ${seconds(n.avg_block_time_10m_seconds)} · 24 h: ${seconds(n.avg_block_time_24h_seconds)}
From this permanode's own recorded
blocks, not the node - a fresh install
won't have a 24h figure yet.`;
  const burnedTotal =
    n.burned_total_micronoid != null && n.emitted_total_micronoid != null
      ? `Since genesis: ${noid(n.burned_total_micronoid)} of
${noid(n.emitted_total_micronoid)} minted
(${((Number(n.burned_total_micronoid) / Number(n.emitted_total_micronoid)) * 100).toFixed(4)}%), from the emission
schedule minus the circulating supply.`
      : "";
  const perDay =
    stats?.db_bytes != null && recordedFor > 3600 ? `\n≈ ${bytesText((stats.db_bytes / recordedFor) * 86400)} per day at the current rate.` : "";
  return [
    statCard(supply(n.circulating_supply_micronoid), "Circulating supply", { hint: "in NOID" }),
    statCard(noid(n.block_reward_micronoid), "Block reward"),
    statCard(hashrate(n.estimated_hashrate_hs), "Network hashrate", {
      hint: "Rough estimate derived from the\ncurrent PoW target, not a\nmeasured network figure.",
    }),
    statCard(seconds(n.avg_block_time_1h_seconds), "Avg block time (1h)", { hint: blockTimes }),
    statCard(mempool ? int(mempool.size) : "-", "Mempool pending", { id: "stat-mempool" }),
    statCard(mempool ? noid(mempool.fee_floor) : "-", "Fee floor", { id: "stat-floor" }),
    statCard(n.slots_until_halving != null ? int(n.slots_until_halving) : "-", "UTXOs until halving", {
      hint:
        n.slots_until_halving != null
          ? `The block reward halves (${noid(n.block_reward_micronoid)} → ${noid(Math.floor(n.block_reward_micronoid / 2))})
when the live UTXO set expands, which
happens at ${n.halving_trigger_pct}% of its capacity of
${int(n.state_capacity)} slots. Live UTXOs now:
${int(n.active_slots)} (${((n.active_slots / n.state_capacity) * 100).toFixed(2)}%).`
          : "Not available from the node.",
    }),
    statCard(stats?.burned_fees_24h_micronoid != null ? noid(stats.burned_fees_24h_micronoid) : "-", "Burned fees (24h)", {
      hint: `Fees destroyed by consensus in the last
24 hours (0.0025 NOID per net-new UTXO
slot at today's occupancy; miners only
claim the rest), from recorded blocks.
${burnedTotal}`,
    }),
    statCard(stats ? int(stats.transactions_24h) : "-", "Transactions (24h)", {
      hint: "Transactions in the blocks of the last\n24 hours, from this permanode's records.",
    }),
    statCard(stats ? int(stats.addresses_with_balance) : "-", "Addresses with NOID", {
      hint: "Addresses holding at least one live UTXO,\nfrom the last sweep of the node's UTXO state.",
    }),
    statCard(bytesText(stats?.db_bytes), "Permanode storage", {
      hint: `This permanode's database on disk,\nincluding the write-ahead log.${perDay}`,
    }),
    statCard(recordedFor != null ? daysText(recordedFor) : "-", "Recorded history", {
      hint: stats?.oldest_retained_timestamp
        ? `Every block since ${fullTime(stats.oldest_retained_timestamp)}\nis on record here.`
        : "Nothing recorded yet.",
    }),
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
function donationHtml(address) {
  if (!address) return "";
  return `<span class="donate">${hint(
    `<span class="hi">Donations</span> ${link(`/address/${address}`, shortHash(address, 10, 8), "addr")}`,
    `This explorer is run by its operator at their\nown expense and is developed independently\nof the Parano1d project. Donations to this\naddress help keep it running:\n${address}`
  )}</span>`;
}

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
    ${live}
    <span>Tip <b>#${stats.last_processed_height ?? "-"}</b></span>
    ${hint(`Blocks <b>${int(stats.indexed_blocks)}</b>`, "Blocks this permanode has recorded since it\nstarted (see \"History since\") - not the\nchain's lifetime total.")}
    ${hint(`Transactions <b>${int(stats.indexed_transactions)}</b>`, "Transactions this permanode has recorded\nsince it started (see \"History since\") - not\nthe chain's lifetime total.")}
    ${hint(`Live UTXOs <b>${int(stats.live_utxos)}</b>`, "Only what this permanode has itself recorded\nas created and still unspent since it started\nindexing - not the network-wide total.")}
    ${n.active_slots != null ? hint(`Network UTXOs <b>${int(n.active_slots)}</b>`, "The node's own count across its entire\nhistory since genesis, for comparison.") : ""}
    ${hint(`History since <b>${stats.oldest_retained_timestamp ? fullTime(stats.oldest_retained_timestamp) : "-"}</b>`, "Oldest block with a recorded body - where\nthis permanode's transaction history begins.")}
    ${gaps}
    <span>Avg block time <b>${seconds(n.avg_block_time_1h_seconds)}</b></span>
    ${stats.decoder_mismatches > 0 ? hint(`Decoder mismatches <b>${stats.decoder_mismatches}</b>`, "Times the fallback decoder's output disagreed with the node's own getBlockDetails for a block both could decode - should be 0.") : ""}
    ${donationHtml(stats.donation_address)}`;
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
    // Outputs back to the viewed address are its own change, so the
    // counterparty is the first output that went somewhere else.
    const ownChange = tx.counterparty && tx.receiver !== tx.counterparty ? 1 : 0;
    const others = tx.n_outputs - ownChange;
    const extra = others > 1 ? ` ${hint(`+${others - 1}`, receiverHint(others))}` : "";
    party = (tx.counterparty ? addrLink(tx.counterparty) : "—") + extra;
  }
  // The net effect on this address (received minus spent, change cancels
  // out) - not the transaction's total output, which on a 1 -> 2 send is
  // mostly the sender's change.
  const delta = tx.address_delta_micronoid != null ? BigInt(tx.address_delta_micronoid) : null;
  const positive = delta != null ? delta >= 0n : incoming;
  const amount =
    delta != null
      ? `${positive ? "+" : "−"}${noid((delta < 0n ? -delta : delta).toString())}`
      : `${incoming ? "+" : "−"}${noid(tx.output_sum_micronoid)}`;
  return `<div class="trow cols-atx">
      <span class="with-tag">${link(`/tx/${tx.txid}`, shortHash(tx.txid))}${kindTag(tx)}</span>
      ${timeCell(tx.timestamp)}
      <span>${link(`/block/${tx.height}`, "#" + tx.height)}</span>
      <span class="dim">${party}</span>
      <span>${tx.n_inputs} → ${tx.n_outputs}</span>
      <span class="${positive ? "pos" : ""}">${amount}</span>
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

const PAGE_SIZES = [25, 50, 100, 150, 200];

// Navigates within the app (app.js re-renders on popstate).
function go(path) {
  history.pushState(null, "", path);
  window.dispatchEvent(new PopStateEvent("popstate"));
}

function addressUrl(address, pageNo, pageSize) {
  return `/address/${address}?page=${pageNo}${pageSize !== 25 ? `&size=${pageSize}` : ""}`;
}

// A click-to-open menu (native <select> lists open on mousedown and pick
// on mouseup in some browsers, which reads as "hold the button down").
function menuHtml(id, current, values, opts = {}) {
  const items = values
    .map((v) => `<button type="button" data-value="${v}"${v === current ? ' class="current"' : ""}>${v}</button>`)
    .join("");
  return `<span class="menu" id="${id}">
      <button type="button" class="menu-btn" aria-haspopup="listbox" aria-expanded="false" aria-label="${escapeHtml(opts.label || "")}">${current} <span class="caret">▾</span></button>
      <div class="menu-list" role="listbox">${items}</div>
    </span>`;
}

// Wires every .menu under root: click opens/closes, clicking an item calls
// onPick(menuId, value); a click elsewhere or Escape closes. Returns a
// dispose function.
function wireMenus(root, onPick) {
  const close = () => root.querySelectorAll(".menu.open").forEach((m) => {
    m.classList.remove("open");
    m.querySelector(".menu-btn").setAttribute("aria-expanded", "false");
  });
  const onClick = (e) => {
    const btn = e.target.closest(".menu-btn");
    const item = e.target.closest(".menu-list button");
    if (btn && root.contains(btn)) {
      const menu = btn.closest(".menu");
      const opening = !menu.classList.contains("open");
      close();
      if (opening) {
        menu.classList.add("open");
        btn.setAttribute("aria-expanded", "true");
        // Fixed positioning so the list escapes the card's overflow clip;
        // it opens upwards when there is no room below.
        const list = menu.querySelector(".menu-list");
        const r = btn.getBoundingClientRect();
        const h = list.offsetHeight;
        const below = r.bottom + 4 + h <= window.innerHeight - 8;
        list.style.top = `${Math.round(below ? r.bottom + 4 : r.top - 4 - h)}px`;
        list.style.left = `${Math.round(Math.min(r.left, window.innerWidth - list.offsetWidth - 8))}px`;
        list.style.minWidth = `${Math.round(r.width)}px`;
        list.querySelector("button.current")?.scrollIntoView({ block: "nearest" });
      }
      return;
    }
    if (item && root.contains(item)) {
      const menu = item.closest(".menu");
      close();
      onPick(menu.id, Number(item.dataset.value));
      return;
    }
    close();
  };
  const onKey = (e) => {
    if (e.key === "Escape") close();
  };
  document.addEventListener("click", onClick);
  document.addEventListener("keydown", onKey);
  window.addEventListener("scroll", close);
  window.addEventListener("resize", close);
  return () => {
    document.removeEventListener("click", onClick);
    document.removeEventListener("keydown", onKey);
    window.removeEventListener("scroll", close);
    window.removeEventListener("resize", close);
  };
}

// "← newer · page [menu] / N · older →" on the left, the page length on
// the right. Both menus navigate on pick (wired in the view's mount).
function pagerHtml(address, pageNo, pageSize, totalPages) {
  const pages = Array.from({ length: totalPages }, (_, i) => i + 1);
  return `<div class="pager">
      <span class="pager-nav">
        ${pageNo > 1 ? link(addressUrl(address, pageNo - 1, pageSize), "← newer") : '<span class="dim">← newer</span>'}
        <span>page ${menuHtml("page-menu", pageNo, pages, { label: "Page" })} / ${totalPages}</span>
        ${pageNo < totalPages ? link(addressUrl(address, pageNo + 1, pageSize), "older →") : '<span class="dim">older →</span>'}
      </span>
      <span class="pager-size">per page ${menuHtml("size-menu", pageSize, PAGE_SIZES, { label: "Transactions per page" })}</span>
    </div>`;
}

export async function addressView(address, pageNo = 1, pageSize = 25) {
  if (!PAGE_SIZES.includes(pageSize)) pageSize = 25;
  const [result, stats] = await Promise.all([api.address(address, pageNo, pageSize), api.stats().catch(() => null)]);
  const rows = result.transactions.map((tx) => addressTxRow(tx, address)).join("");
  const totalPages = Math.max(1, Math.ceil(result.total / result.page_size));
  const b = result.balance;
  const liveKnown = result.live_balance_micronoid !== null && result.live_balance_micronoid !== undefined;
  const liveHint = "The sum of every UTXO this address holds\nright now, read live from the node's\nconsensus state - correct regardless of\nwhat this permanode has recorded.";
  const recHint = "From this permanode's own recorded\nhistory only - transactions it has\nitself seen since it started running.\nOutputs the node no longer holds are\nexcluded even if the spend fell into a gap.";

  // One notice whenever the recorded figures cannot be complete: the
  // address was active before this permanode started (inputs spent from
  // outputs it never saw created, or live UTXOs older than its records),
  // or some of its outputs were spent inside gaps.
  const olderSpent = BigInt(b.sent_from_unrecorded_micronoid || "0");
  const predates = olderSpent > 0n || (liveKnown && result.live_utxo_count > b.confirmed_utxos);
  const inGaps = b.spent_in_gap_utxos > 0;
  const since = stats?.oldest_retained_timestamp ? fullTime(stats.oldest_retained_timestamp) : "it started";
  const reasons = [];
  if (predates) reasons.push(`it was already active before this permanode began recording (${since})`);
  if (olderSpent > 0n) reasons.push(`${noid(olderSpent.toString())} spent from here came from older outputs whose arrival is not on record`);
  if (inGaps)
    reasons.push(
      `${int(b.spent_in_gap_utxos)} recorded output${b.spent_in_gap_utxos === 1 ? "" : "s"} (${noid(b.spent_in_gap_micronoid)}) ${
        b.spent_in_gap_utxos === 1 ? "was" : "were"
      } spent in blocks whose bodies this permanode never had, so the spending transactions are unknown here and those outputs are excluded from the recorded balance`
    );
  const historyNote = reasons.length
    ? `<p class="note info"><b>This permanode does not hold this address's complete history:</b> ${reasons.join("; ")}.
       "Total received", "Total sent" and the recorded figures cover recorded activity only and will not add up to the balance.
       <b>The live balance and live UTXOs above are correct regardless.</b> They are read directly from the node's consensus
       state, which every node in the network verifies and which admits no double spend or unbacked coin - no missing
       history can change what an address holds right now.</p>`
    : "";
  const note =
    result.total === 0
      ? `<p class="note warn">This permanode has recorded no transaction activity for this address since it
         started running - the "recorded" figures are genuinely zero, not missing data. The live balance
         comes straight from the node's current state, so it is accurate even without a history to show.</p>`
      : `<p class="note">${int(result.total)} transaction${result.total === 1 ? "" : "s"} recorded involving this address.</p>${historyNote}`;

  const html = page(`
    ${back()}
    <div class="card pad stack wide">
      <div class="stack tight">
        <span class="label">Address</span>
        <span class="addr-full">${escapeHtml(address)}</span>
      </div>
      <div class="stats inner">
        <div class="stat"><span class="v pos hint" title="${escapeHtml(liveHint)}">${liveKnown ? noid(result.live_balance_micronoid, false) : "?"}</span><span class="k">Current balance (live, all UTXOs)</span></div>
        <div class="stat"><span class="v">${liveKnown ? int(result.live_utxo_count) : "?"}</span><span class="k">UTXOs (live, all)</span></div>
        <div class="stat"><span class="v hint" title="${escapeHtml(recHint)}">${noid(b.confirmed_balance_micronoid, false)}</span><span class="k">Recorded balance</span></div>
        <div class="stat"><span class="v">${int(b.confirmed_utxos)}</span><span class="k">Recorded UTXOs</span></div>
        <div class="stat"><span class="v hint" title="Sum of recorded outputs to this address\nsince this permanode began recording -\nnot the address's lifetime total.">${noid(b.total_received_micronoid, false)}</span><span class="k">Total received</span></div>
        <div class="stat"><span class="v hint" title="Sum of the inputs this address spent in\nrecorded transactions since this permanode\nbegan recording - may include coins it\nreceived before that.">${noid(b.total_sent_micronoid, false)}</span><span class="k">Total sent</span></div>
      </div>
      ${note}
    </div>
    <div class="card">
      <div class="tbl-scroll">
        <div class="thead cols-atx"><span>Txid</span>${timeHeader()}<span>Block</span><span>Counterparty</span><span>In → out</span><span>Amount</span><span>Fee</span></div>
        ${rows || '<div class="trow cols-atx"><span class="empty">No transactions recorded.</span></div>'}
      </div>
      ${result.total > PAGE_SIZES[0] ? pagerHtml(address, pageNo, pageSize, totalPages) : ""}
    </div>
    <div class="card" id="live-utxos">
      <div class="card-head"><h2>Live UTXOs</h2><button type="button" class="ghost" id="load-live-utxos">load from node →</button></div>
      <div id="live-utxos-body"><p class="note" style="padding:18px 22px">Not loaded by default to keep this page light. The individual unspent outputs behind the live balance above, straight from the node.</p></div>
    </div>`);

  function mount(root) {
    const disposeTick = tickingMount(root);
    const disposeMenus = wireMenus(root, (menuId, value) => {
      if (menuId === "page-menu") go(addressUrl(address, value, pageSize));
      if (menuId === "size-menu") go(addressUrl(address, 1, value));
    });
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
      disposeMenus();
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
function everyText(seconds) {
  if (!seconds) return "";
  if (seconds % 3600 === 0) return `every ${seconds / 3600}h`;
  if (seconds % 60 === 0) return `every ${seconds / 60}m`;
  return `every ${seconds}s`;
}

export async function richlistView() {
  const [entries, stats] = await Promise.all([api.richlist(), api.stats().catch(() => null)]);
  const every = everyText(stats?.balance_sweep_interval_seconds);
  const refresh = everyText(stats?.address_refresh_interval_seconds);
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
        <div class="thead cols-rich"><span>#</span><span>Address</span><span>Balance</span><span>UTXOs</span><span>Updated${
          every ? ` <span class="hint" title="Every address is re-read from the node's UTXO state ${every}${refresh ? `; addresses with recorded activity also ${refresh}` : ""}.">(${every})</span>` : ""
        }</span></div>
        ${rows || '<div class="trow cols-rich"><span class="empty">No addresses known yet.</span></div>'}
      </div>
    </div>`);
  return { html, mount: tickingMount };
}

// ---- about ------------------------------------------------------------
const SOURCE_URL = "https://github.com/gustlborg/gb-parano1d-permanode";

export async function aboutView() {
  const stats = await api.stats().catch(() => null);
  const since = stats?.oldest_retained_timestamp ? fullTime(stats.oldest_retained_timestamp) : "its first start";
  const gaps = stats ? int(stats.gaps) : "-";
  const donation = stats?.donation_address;
  const html = page(`
    ${back()}
    <div class="card pad prose">
      <h1 class="title">About this explorer</h1>
      <h2>Independent</h2>
      <p>This site is an instance of <b>parano1d-permanode</b>, an independent, non-commercial community project.
         It is run by its operator as a hobby, at their own expense, and is <b>not affiliated with, endorsed by or
         maintained by the Parano1d project or its developers</b>.</p>
      <h2>No warranty</h2>
      <p>Everything shown here is provided as is, <b>without any warranty of accuracy, completeness or availability,
         and without liability for any use made of it</b>. Do not rely on it for financial or legal decisions. The
         authoritative record of the Parano1d network is the consensus state of the network itself, as verified by
         your own node.</p>
      <h2>Where the data comes from</h2>
      <p>Block headers and transactions are recorded from the operator's own Parano1d node as blocks arrive. The
         node itself discards transaction bodies after a few minutes, so the recorded history begins at
         <b>${since}</b> and can contain gaps where a block's body was unavailable in time (currently
         <b>${gaps}</b>). Recorded totals for an address only cover activity since then. Live balances, UTXOs
         and mempool contents are read from the node's current state at the time of your request and are
         independent of the recorded history.</p>
      <h2>Source code</h2>
      <p>parano1d-permanode is free software under the GNU AGPL-3.0-or-later. Source, releases and documentation:
         <a href="${SOURCE_URL}" rel="noopener">${SOURCE_URL}</a>. Anyone running a Parano1d node can run their own
         instance.</p>
      ${
        donation
          ? `<h2>Support</h2>
             <p>Donations help the operator keep this instance running:<br><span class="mono">${link(`/address/${donation}`, donation)}</span></p>`
          : ""
      }
    </div>`);
  return { html, mount: tickingMount };
}

export function notFoundHtml(msg) {
  return page(`${back()}<div class="card"><p class="error">${escapeHtml(msg || "Not found.")}</p></div>`);
}
