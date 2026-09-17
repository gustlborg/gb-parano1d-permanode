import { api } from "./api.js";
import { noid, shortHash, timeAgo, fullTime, escapeHtml } from "./format.js";

function link(href, text) {
  return `<a href="${href}" data-link>${escapeHtml(text)}</a>`;
}

function blockTile(b) {
  const href = `/block/${b.height}`;
  const cls = b.body_captured ? "block-tile" : "block-tile gap";
  return `<a class="${cls}" href="${href}" data-link title="height ${b.height}">
      <div class="h">#${b.height}</div>
      <div class="n">${b.tx_count} tx</div>
    </a>`;
}

function blocksTable(blocks) {
  const rows = blocks
    .map(
      (b) => `<tr>
        <td>${link(`/block/${b.height}`, "#" + b.height)}</td>
        <td>${timeAgo(b.timestamp)}</td>
        <td class="mono">${link(`/address/${b.miner}`, shortHash(b.miner))}</td>
        <td>${b.tx_count}</td>
        <td>${noid(b.reward_micronoid)}</td>
        <td>${b.total_fees_micronoid !== null ? noid(b.total_fees_micronoid) : "-"}</td>
        <td>${b.body_captured ? "" : '<span class="badge gap">no body</span>'}</td>
      </tr>`
    )
    .join("");
  return `<table>
      <thead><tr><th>Height</th><th>Time</th><th>Miner</th><th>Txs</th><th>Reward</th><th>Fees</th><th></th></tr></thead>
      <tbody>${rows}</tbody>
    </table>`;
}

export async function homeView() {
  const [stats, blocks] = await Promise.all([api.stats(), api.blocks(25)]);
  const tiles = blocks.map(blockTile).join("");
  return `
    ${tickerHtml(stats)}
    <div class="block-grid">${tiles}</div>
    <div class="panel">
      <h2>Recent blocks</h2>
      ${blocksTable(blocks)}
    </div>`;
}

export function tickerHtml(stats) {
  if (!stats) return "";
  const oldest = stats.oldest_retained_timestamp ? fullTime(stats.oldest_retained_timestamp) : "-";
  return `
    <span>Indexed tip: <strong>#${stats.last_processed_height ?? "-"}</strong></span>
    <span>Blocks recorded: <strong>${stats.indexed_blocks}</strong></span>
    <span>Transactions: <strong>${stats.indexed_transactions}</strong></span>
    <span>History since: <strong>${oldest}</strong></span>
    ${stats.gaps > 0 ? `<span>Gaps: <strong class="mono">${stats.gaps}</strong></span>` : ""}`;
}

function txRow(tx) {
  const kind = tx.coinbase
    ? '<span class="badge coinbase">coinbase</span>'
    : tx.development_payout
    ? '<span class="badge dev">dev payout</span>'
    : "";
  const sender = tx.input_owner ? link(`/address/${tx.input_owner}`, shortHash(tx.input_owner)) : "-";
  return `<tr>
      <td class="mono">${link(`/tx/${tx.txid}`, shortHash(tx.txid))} ${kind}</td>
      <td class="mono">${sender}</td>
      <td>${tx.n_inputs} → ${tx.n_outputs}</td>
      <td>${noid(tx.output_sum_micronoid)}</td>
      <td>${noid(tx.fee_micronoid)}</td>
    </tr>`;
}

export async function blockView(idParam) {
  const isHeight = /^\d+$/.test(idParam);
  const block = isHeight ? await api.blockByHeight(idParam) : await api.blockByHash(idParam);
  if (!block) return notFoundHtml(`Block ${idParam} not found (or not canonical).`);

  const txRows = block.transactions.map(txRow).join("");
  return `
    <div class="panel">
      <h2>Block #${block.height}</h2>
      <dl class="kv">
        <dt>Hash</dt><dd class="mono">${block.hash}</dd>
        <dt>Parent</dt><dd class="mono">${link(`/block/${block.prev_hash}`, block.prev_hash)}</dd>
        <dt>Timestamp</dt><dd>${fullTime(block.timestamp)} (${timeAgo(block.timestamp)})</dd>
        <dt>Miner</dt><dd class="mono">${link(`/address/${block.miner}`, block.miner)}</dd>
        <dt>Proof class</dt><dd>${block.proof_class ?? "-"}</dd>
        <dt>Reward</dt><dd>${noid(block.reward_micronoid)}</dd>
        <dt>Total fees</dt><dd>${block.total_fees_micronoid !== null ? noid(block.total_fees_micronoid) : "-"}</dd>
        <dt>Body captured</dt><dd>${block.body_captured ? "yes" : '<span class="badge gap">no — pruned before capture</span>'}</dd>
        <dt>State root</dt><dd class="mono">${block.state_root}</dd>
        <dt>Tx root</dt><dd class="mono">${block.tx_root}</dd>
        <dt>Nonce</dt><dd class="mono">${block.nonce_hex}</dd>
        <dt>Difficulty target</dt><dd class="mono">${block.difficulty_target}</dd>
      </dl>
    </div>
    <div class="panel">
      <h2>Transactions (${block.transactions.length})</h2>
      <table>
        <thead><tr><th>Txid</th><th>Sender</th><th>In → Out</th><th>Amount</th><th>Fee</th></tr></thead>
        <tbody>${txRows || '<tr><td colspan="5">No transactions recorded for this block.</td></tr>'}</tbody>
      </table>
    </div>`;
}

export async function txView(txid) {
  const tx = await api.tx(txid);
  if (!tx) return notFoundHtml(`Transaction ${txid} not found.`);

  const inputs = tx.inputs
    .map((i) => `<li><span>slot ${i.slot_index}</span><span>${noid(i.amount_micronoid)}</span></li>`)
    .join("") || "<li>none (coinbase)</li>";
  const outputs = tx.outputs
    .map(
      (o) => `<li><span>${link(`/address/${o.owner}`, shortHash(o.owner))}</span><span>${noid(o.amount_micronoid)}</span></li>`
    )
    .join("");

  return `
    <div class="panel">
      <h2>Transaction</h2>
      <dl class="kv">
        <dt>Txid</dt><dd class="mono">${tx.txid}</dd>
        <dt>Block</dt><dd>${link(`/block/${tx.block.height}`, "#" + tx.block.height)}
          ${tx.block.canonical ? "" : '<span class="badge orphaned">block later orphaned</span>'}</dd>
        <dt>Time</dt><dd>${fullTime(tx.block.timestamp)}</dd>
        <dt>Type</dt><dd>${
          tx.coinbase ? '<span class="badge coinbase">coinbase (block reward)</span>' : tx.development_payout ? '<span class="badge dev">development payout</span>' : "transfer"
        }</dd>
        <dt>Sender</dt><dd class="mono">${tx.input_owner ? link(`/address/${tx.input_owner}`, tx.input_owner) : "-"}</dd>
        <dt>Fee</dt><dd>${noid(tx.fee_micronoid)}</dd>
        <dt>Input sum</dt><dd>${noid(tx.input_sum_micronoid)}</dd>
        <dt>Output sum</dt><dd>${noid(tx.output_sum_micronoid)}</dd>
        <dt>Epoch anchor</dt><dd class="mono">${tx.epoch_anchor}</dd>
      </dl>
    </div>
    <div class="panel io-cols">
      <div><h3>Inputs</h3><ul class="io-list">${inputs}</ul></div>
      <div><h3>Outputs</h3><ul class="io-list">${outputs}</ul></div>
    </div>
    <div class="panel">
      <h2>Merkle path (receipt data)</h2>
      <ul class="io-list mono">${tx.page_hashes.map((h) => `<li>${h}</li>`).join("")}</ul>
    </div>`;
}

export async function addressView(address, page = 1) {
  const result = await api.address(address, page, 25);
  const rows = result.transactions.map(txRow).join("");
  const totalPages = Math.max(1, Math.ceil(result.total / result.page_size));
  return `
    <div class="panel">
      <h2>Address</h2>
      <p class="mono">${address}</p>
      <p>${result.total} transaction(s) recorded involving this address.</p>
    </div>
    <div class="panel">
      <table>
        <thead><tr><th>Txid</th><th>Sender</th><th>In → Out</th><th>Amount</th><th>Fee</th></tr></thead>
        <tbody>${rows || '<tr><td colspan="5">No transactions found.</td></tr>'}</tbody>
      </table>
      <div class="pager">
        ${page > 1 ? link(`/address/${address}?page=${page - 1}`, "← newer") : ""}
        <span>page ${page} / ${totalPages}</span>
        ${page < totalPages ? link(`/address/${address}?page=${page + 1}`, "older →") : ""}
      </div>
    </div>`;
}

export function notFoundHtml(msg) {
  return `<div class="error">${escapeHtml(msg || "Not found.")}</div>`;
}
