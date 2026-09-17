import { api } from "./api.js";
import { noid, shortHash, timeAgo, fullTime, escapeHtml, hashrate, seconds } from "./format.js";
import { renderBlockSquare, pickTx } from "./blocksquare.js";

function link(href, text) {
  return `<a href="${href}" data-link>${escapeHtml(text)}</a>`;
}

// Browsers wrap a native title tooltip at literal newlines, so this stays
// narrow instead of one very wide line.
function receiverHint(nOutputs) {
  return `${nOutputs} outputs total, showing the first.
Extra outputs are often change back
to the sender, but the protocol
doesn't guarantee that.`;
}

// A <td class="time-cell" data-ts="..."> whose text is filled in by
// applyTimeFormat() right after insertion, so it always matches whatever
// mode the table's "Time" header is currently toggled to.
function timeCell(ts) {
  return `<td class="time-cell" data-ts="${ts}"></td>`;
}

// Applies the current relative/absolute mode to every .time-cell under
// `root`. Call this after inserting or replacing any HTML that contains
// timeCell() output, including after a live-refresh rebuild.
function applyTimeFormat(root, absolute) {
  root.querySelectorAll(".time-cell").forEach((td) => {
    const ts = Number(td.dataset.ts);
    td.textContent = absolute ? fullTime(ts) : timeAgo(ts);
  });
}

// Wires every "Time" column header under `root` to toggle all .time-cell
// text between relative and absolute on click. Returns a dispose function.
// State lives in the closure so it survives table rebuilds as long as the
// caller re-applies it (see applyTimeFormat) and re-wires after replacing
// a header element.
function wireTimeToggle(root, getAbsolute, setAbsolute) {
  const disposers = [];
  root.querySelectorAll(".time-toggle").forEach((th) => {
    const onClick = () => {
      setAbsolute(!getAbsolute());
      applyTimeFormat(root, getAbsolute());
    };
    th.addEventListener("click", onClick);
    disposers.push(() => th.removeEventListener("click", onClick));
  });
  return () => disposers.forEach((d) => d());
}

function blocksTable(blocks) {
  const rows = blocks
    .map(
      (b) => `<tr>
        <td>${link(`/block/${b.height}`, "#" + b.height)}</td>
        ${timeCell(b.timestamp)}
        <td class="mono">${link(`/address/${b.miner}`, shortHash(b.miner))}</td>
        <td>${b.tx_count}</td>
        <td>${noid(b.reward_micronoid)}</td>
        <td>${b.total_fees_micronoid !== null ? noid(b.total_fees_micronoid) : "-"}</td>
        <td>${b.body_captured ? "" : '<span class="badge gap">no body</span>'}</td>
      </tr>`
    )
    .join("");
  return `<div class="table-scroll"><table>
      <thead><tr><th>Height</th><th class="time-toggle" title="Click to toggle relative/absolute time">Time</th><th>Miner</th><th>Txs</th><th>Reward</th><th>Fees</th><th></th></tr></thead>
      <tbody>${rows}</tbody>
    </table></div>`;
}

function navigateToTx(txid) {
  history.pushState(null, "", `/tx/${txid}`);
  window.dispatchEvent(new PopStateEvent("popstate"));
}

// Wires a canvas to draw `txs` (a fixed snapshot or a live-refreshed
// getter) and click-navigate to the tx under the pointer. Returns
// {draw, dispose} - callers must call dispose() when the view unmounts, or
// the resize listener piles up against a detached canvas forever.
function wireSquare(canvas, getTxs, opts = {}) {
  const draw = () => renderBlockSquare(canvas, getTxs(), opts);
  draw();
  const onClick = (e) => {
    const tx = pickTx(canvas, getTxs(), e.clientX, e.clientY);
    const id = tx && (tx.txid || tx.tx_hash);
    if (id) navigateToTx(id);
  };
  canvas.addEventListener("click", onClick);
  window.addEventListener("resize", draw);
  const dispose = () => {
    canvas.removeEventListener("click", onClick);
    window.removeEventListener("resize", draw);
  };
  return { draw, dispose };
}

const STRIP_BLOCK_COUNT = 8;
const LIVE_REFRESH_MS = 1000;

function networkMetricsHtml(stats) {
  const n = stats?.network;
  if (!n) return "";
  return `
    <div class="panel">
      <h2>Network</h2>
      <div class="mempool-stats">
        <div class="stat"><div class="v">${noid(n.circulating_supply_micronoid)}</div><div class="k">Circulating supply</div></div>
        <div class="stat"><div class="v">${noid(n.block_reward_micronoid)}</div><div class="k">Block reward</div></div>
        <div class="stat"><div class="v hint" title="Rough estimate derived from the
current PoW target, not a
measured network figure.">${hashrate(n.estimated_hashrate_hs)}</div><div class="k">Network hashrate (est.)</div></div>
        <div class="stat"><div class="v hint" title="From this permanode's own recorded
blocks, not the node - a fresh
install won't have a 24h figure
yet.">${seconds(n.avg_block_time_10m_seconds)}</div><div class="k">Avg block time (10m)</div></div>
        <div class="stat"><div class="v">${seconds(n.avg_block_time_1h_seconds)}</div><div class="k">Avg block time (1h)</div></div>
        <div class="stat"><div class="v">${seconds(n.avg_block_time_24h_seconds)}</div><div class="k">Avg block time (24h)</div></div>
      </div>
    </div>`;
}

function stripTilesHtml(mempoolInfo, stripBlocks, stripSummaries) {
  return [
    `<div class="block-tile mempool" id="mempool-tile">
       <a class="square" href="/mempool" data-link><canvas id="mempool-canvas"></canvas></a>
       <div class="label"><strong>Mempool</strong><br>${mempoolInfo ? mempoolInfo.size : "-"} pending</div>
     </div>`,
    ...stripBlocks.map((b, i) => {
      const summary = stripSummaries[i];
      if (!b) return "";
      const cls = b.body_captured ? "block-tile" : "block-tile gap";
      return `<div class="${cls}">
          <a class="square" href="/block/${b.height}" data-link><canvas id="block-canvas-${b.height}"></canvas></a>
          <div class="label"><strong>#${b.height}</strong><br>${timeAgo(summary.timestamp)}</div>
        </div>`;
    }),
  ].join('<span class="chain-arrow">←</span>');
}

async function fetchStrip(summaries) {
  const stripSummaries = summaries.slice(0, STRIP_BLOCK_COUNT);
  const stripBlocks = await Promise.all(
    stripSummaries.map((b) => api.blockByHeight(b.height).catch(() => null))
  );
  return { stripSummaries, stripBlocks };
}

export async function homeView() {
  let summaries = await api.blocks(25);
  let mempoolInfo = await api.mempool().catch(() => null);
  let stats = await api.stats().catch(() => null);
  let { stripSummaries, stripBlocks } = await fetchStrip(summaries);

  const html = `
    <div class="chain-strip" id="chain-strip">${stripTilesHtml(mempoolInfo, stripBlocks, stripSummaries)}</div>
    <div id="network-metrics">${networkMetricsHtml(stats)}</div>
    <div class="panel">
      <h2>Recent blocks</h2>
      <div id="recent-blocks-table">${blocksTable(summaries)}</div>
    </div>`;

  function mount(root) {
    let blockDisposers = [];
    let absoluteTime = false;
    let timeToggleDisposer = () => {};

    function refreshTimeToggle() {
      timeToggleDisposer();
      applyTimeFormat(root, absoluteTime);
      timeToggleDisposer = wireTimeToggle(root, () => absoluteTime, (v) => (absoluteTime = v));
    }

    function wireMempoolTile() {
      const c = root.querySelector("#mempool-canvas");
      return c ? wireSquare(c, () => mempoolInfo?.txs || [], { emptyLabel: "empty" }).dispose : () => {};
    }
    function wireBlockTiles() {
      blockDisposers.forEach((d) => d());
      blockDisposers = stripBlocks
        .filter(Boolean)
        .map((b) => {
          const c = root.querySelector(`#block-canvas-${b.height}`);
          return c ? wireSquare(c, () => b.transactions).dispose : null;
        })
        .filter(Boolean);
    }

    let mempoolDisposer = wireMempoolTile();
    wireBlockTiles();
    refreshTimeToggle();

    const timer = setInterval(async () => {
      try {
        const [newSummaries, newMempool, newStats] = await Promise.all([
          api.blocks(25),
          api.mempool(),
          api.stats().catch(() => null),
        ]);
        const tipChanged = newSummaries[0]?.height !== summaries[0]?.height;
        mempoolInfo = newMempool;
        stats = newStats;
        const metrics = root.querySelector("#network-metrics");
        if (metrics) metrics.innerHTML = networkMetricsHtml(stats);

        if (tipChanged) {
          summaries = newSummaries;
          ({ stripSummaries, stripBlocks } = await fetchStrip(summaries));

          mempoolDisposer();
          const strip = root.querySelector("#chain-strip");
          if (strip) strip.innerHTML = stripTilesHtml(mempoolInfo, stripBlocks, stripSummaries);
          mempoolDisposer = wireMempoolTile();
          wireBlockTiles();

          const table = root.querySelector("#recent-blocks-table");
          if (table) table.innerHTML = blocksTable(summaries);
          refreshTimeToggle();
        } else {
          const c = root.querySelector("#mempool-canvas");
          if (c) renderBlockSquare(c, mempoolInfo.txs, { emptyLabel: "empty" });
          const label = root.querySelector("#mempool-tile .label");
          if (label) label.innerHTML = `<strong>Mempool</strong><br>${mempoolInfo.size} pending`;
        }
      } catch {
        /* node/API momentarily unreachable - keep last known view, try again next tick */
      }
    }, LIVE_REFRESH_MS);

    return () => {
      clearInterval(timer);
      mempoolDisposer();
      blockDisposers.forEach((d) => d());
      timeToggleDisposer();
    };
  }

  return { html, mount };
}

export function tickerHtml(stats) {
  if (!stats) return "";
  const oldest = stats.oldest_retained_timestamp ? fullTime(stats.oldest_retained_timestamp) : "-";
  return `
    <span>Indexed tip: <strong>#${stats.last_processed_height ?? "-"}</strong></span>
    <span>Blocks recorded: <strong>${stats.indexed_blocks}</strong></span>
    <span>Transactions: <strong>${stats.indexed_transactions}</strong></span>
    <span class="hint" title="Only what this permanode has itself recorded
as created and still unspent since it started
indexing - not the network-wide total.">Live UTXOs (recorded): <strong>${stats.live_utxos}</strong></span>
    ${stats.network_active_slots != null ? `<span class="hint" title="The node's own count across its entire
history since genesis, for comparison.">Live UTXOs (network): <strong>${stats.network_active_slots}</strong></span>` : ""}
    <span>History since: <strong>${oldest}</strong></span>
    ${stats.gaps > 0 ? `<span class="hint" title="Body still missing outside the node's getBlock serving window - permanently gone.">Gaps: <strong class="mono">${stats.gaps}</strong></span>` : ""}
    ${stats.gaps_resolved > 0 ? `<span class="hint" title="Blocks the getBlock fallback decoder recovered after an initial gap - see project docs on the node RPC bug this works around.">Gaps recovered: <strong class="mono">${stats.gaps_resolved}</strong></span>` : ""}
    ${stats.decoder_mismatches > 0 ? `<span class="hint" title="Times the fallback decoder's output disagreed with the node's own getBlockDetails for a block both could decode - should be 0.">Decoder mismatches: <strong class="mono">${stats.decoder_mismatches}</strong></span>` : ""}
    <span>${link("/mempool", "Live mempool →")}</span>`;
}

function txRow(tx) {
  const kind = tx.coinbase
    ? '<span class="badge coinbase">coinbase</span>'
    : tx.development_payout
    ? '<span class="badge dev">dev payout</span>'
    : "";
  const sender = tx.input_owner ? link(`/address/${tx.input_owner}`, shortHash(tx.input_owner)) : "-";
  const receiver = tx.receiver ? link(`/address/${tx.receiver}`, shortHash(tx.receiver)) : "-";
  const extra = tx.n_outputs > 1 ? ` <span class="hint" title="${receiverHint(tx.n_outputs)}">+${tx.n_outputs - 1} more</span>` : "";
  return `<tr>
      <td class="mono">${link(`/tx/${tx.txid}`, shortHash(tx.txid))} ${kind}</td>
      <td class="mono">${sender}</td>
      <td>${tx.n_inputs} → ${tx.n_outputs}</td>
      <td class="mono">${receiver}${extra}</td>
      <td>${noid(tx.output_sum_micronoid)}</td>
      <td>${noid(tx.fee_micronoid)}</td>
    </tr>`;
}

export async function blockView(idParam) {
  const isHeight = /^\d+$/.test(idParam);
  const block = isHeight ? await api.blockByHeight(idParam) : await api.blockByHash(idParam);
  if (!block) return notFoundHtml(`Block ${idParam} not found (or not canonical).`);

  const txRows = block.transactions.map(txRow).join("");
  const html = `
    <div class="panel">
      <h2>Block #${block.height}</h2>
      <div class="tx-square-large"><canvas id="block-square"></canvas></div>
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
      <h2>Transactions (${block.transactions.length}), packed by size, shaded by fee rate</h2>
      <div class="table-scroll"><table>
        <thead><tr><th>Txid</th><th>Sender</th><th>In → Out</th><th>Receiver</th><th>Amount</th><th>Fee</th></tr></thead>
        <tbody>${txRows || '<tr><td colspan="6">No transactions recorded for this block.</td></tr>'}</tbody>
      </table></div>
    </div>`;

  function mount(root) {
    const c = root.querySelector("#block-square");
    if (!c) return undefined;
    return wireSquare(c, () => block.transactions, { emptyLabel: "no transactions" }).dispose;
  }

  return { html, mount };
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
  const receiverSummary = tx.outputs
    .map((o) => `${link(`/address/${o.owner}`, o.owner)} (${noid(o.amount_micronoid)})`)
    .join("<br>") || "-";

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
        <dt>Receiver${tx.outputs.length > 1 ? "s" : ""}</dt><dd class="mono">${receiverSummary}</dd>
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

function addressTxRow(tx) {
  const kind = tx.coinbase
    ? '<span class="badge coinbase">coinbase</span>'
    : tx.development_payout
    ? '<span class="badge dev">dev payout</span>'
    : "";
  const sender = tx.input_owner ? link(`/address/${tx.input_owner}`, shortHash(tx.input_owner)) : "-";
  const receiver = tx.receiver ? link(`/address/${tx.receiver}`, shortHash(tx.receiver)) : "-";
  const extra = tx.n_outputs > 1 ? ` <span class="hint" title="${receiverHint(tx.n_outputs)}">+${tx.n_outputs - 1} more</span>` : "";
  return `<tr>
      <td class="mono">${link(`/tx/${tx.txid}`, shortHash(tx.txid))} ${kind}</td>
      ${timeCell(tx.timestamp)}
      <td>${link(`/block/${tx.height}`, "#" + tx.height)}</td>
      <td class="mono">${sender}</td>
      <td>${tx.n_inputs} → ${tx.n_outputs}</td>
      <td class="mono">${receiver}${extra}</td>
      <td>${noid(tx.output_sum_micronoid)}</td>
      <td>${noid(tx.fee_micronoid)}</td>
    </tr>`;
}

export async function addressView(address, page = 1) {
  const result = await api.address(address, page, 25);
  const rows = result.transactions.map(addressTxRow).join("");
  const totalPages = Math.max(1, Math.ceil(result.total / result.page_size));
  const b = result.balance;
  const html = `
    <div class="panel">
      <h2>Address</h2>
      <p class="mono">${address}</p>
      <div class="mempool-stats">
        <div class="stat"><div class="v">${noid(b.confirmed_balance_micronoid)}</div><div class="k">Confirmed balance</div></div>
        <div class="stat"><div class="v">${b.confirmed_utxos}</div><div class="k">Confirmed UTXOs</div></div>
        <div class="stat"><div class="v">${noid(b.total_received_micronoid)}</div><div class="k">Total received</div></div>
        <div class="stat"><div class="v">${noid(b.total_sent_micronoid)}</div><div class="k">Total sent</div></div>
      </div>
      <p>${result.total} transaction(s) recorded involving this address.</p>
    </div>
    <div class="panel">
      <div class="table-scroll"><table>
        <thead><tr><th>Txid</th><th class="time-toggle" title="Click to toggle relative/absolute time">Time</th><th>Block</th><th>Sender</th><th>In → Out</th><th>Receiver</th><th>Amount</th><th>Fee</th></tr></thead>
        <tbody>${rows || '<tr><td colspan="8">No transactions found.</td></tr>'}</tbody>
      </table></div>
      <div class="pager">
        ${page > 1 ? link(`/address/${address}?page=${page - 1}`, "← newer") : ""}
        <span>page ${page} / ${totalPages}</span>
        ${page < totalPages ? link(`/address/${address}?page=${page + 1}`, "older →") : ""}
      </div>
    </div>`;

  function mount(root) {
    let absoluteTime = false;
    applyTimeFormat(root, absoluteTime);
    return wireTimeToggle(root, () => absoluteTime, (v) => (absoluteTime = v));
  }

  return { html, mount };
}

export async function mempoolView() {
  let info = await api.mempool();
  const html = `
    <div class="panel">
      <h2>Live mempool</h2>
      <div class="tx-square-large"><canvas id="mempool-square-large"></canvas></div>
      <div class="mempool-stats" id="mempool-stats">${mempoolStatsHtml(info)}</div>
    </div>
    <div class="panel">
      <h2>Pending transactions</h2>
      <div class="table-scroll"><table id="mempool-table">${mempoolTableHtml(info)}</table></div>
    </div>`;

  function mount(root) {
    const c = root.querySelector("#mempool-square-large");
    const wired = c ? wireSquare(c, () => info.txs, { emptyLabel: "mempool is empty" }) : null;
    const timer = setInterval(async () => {
      try {
        info = await api.mempool();
        if (wired) wired.draw();
        const stats = root.querySelector("#mempool-stats");
        if (stats) stats.innerHTML = mempoolStatsHtml(info);
        const table = root.querySelector("#mempool-table");
        if (table) table.innerHTML = mempoolTableHtml(info);
      } catch {
        /* keep last known view */
      }
    }, LIVE_REFRESH_MS);
    return () => {
      clearInterval(timer);
      if (wired) wired.dispose();
    };
  }

  return { html, mount };
}

function mempoolStatsHtml(info) {
  const rates = info.txs.map((t) => t.fee_rate);
  const min = rates.length ? Math.min(...rates) : 0;
  const max = rates.length ? Math.max(...rates) : 0;
  return `
    <div class="stat"><div class="v">${info.size}</div><div class="k">Pending txs</div></div>
    <div class="stat"><div class="v">${noid(info.fee_floor)}</div><div class="k">Fee floor</div></div>
    <div class="stat"><div class="v">${min} – ${max}</div><div class="k">Fee rate range</div></div>`;
}

function mempoolTableHtml(info) {
  const rows = info.txs
    .slice()
    .sort((a, b) => b.fee_rate - a.fee_rate)
    .map(
      (t) => `<tr>
        <td class="mono">${link(`/tx/${t.tx_hash}`, shortHash(t.tx_hash))}</td>
        <td>${t.n_inputs} → ${t.n_outputs}</td>
        <td>${noid(t.fee_micronoid)}</td>
        <td>${t.fee_rate}</td>
      </tr>`
    )
    .join("");
  return `<thead><tr><th>Txid</th><th>In → Out</th><th>Fee</th><th>Fee rate</th></tr></thead>
    <tbody>${rows || '<tr><td colspan="4">Mempool is empty.</td></tr>'}</tbody>`;
}

export function notFoundHtml(msg) {
  return `<div class="error">${escapeHtml(msg || "Not found.")}</div>`;
}
