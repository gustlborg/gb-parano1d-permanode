# Changelog

Notable changes per release. Commit history has the details.

## Unreleased

- **Reorg history is visible.** `/api/v1/orphans` lists the blocks a reorg
  replaced together with what took their height; `stats.orphaned_blocks`
  counts them; block responses carry `canonical` and `other_versions`, and
  a transaction that survived a reorg lists its `other_occurrences`.
  Replaced blocks can be opened by hash and report no confirmations.
- **`export --dir DIR`** writes the recorded history as CSV (blocks,
  transactions, inputs, outputs, addresses) with `tx_id` as the join key,
  since the protocol `txid` is not unique across reorgs.
- The economics response caches its 24h/7d/30d aggregates per tip
  (340 ms cold, 20 ms warm; they scan every recorded block).

## 0.2.0 - 2026-09-20

The explorer frontend is no longer part of this repository: the
permanode is the indexer and the JSON API, and the built-in page at `/`
is an index of that API. A frontend of your own goes into `site_dir`
(any static site; unknown paths fall back to `index.html`, scripts and
styles are served `no-cache` with an ETag and fonts and images with a
one-day lifetime, paths that would leave the directory are refused).
The public instance at noidexplorer.org runs its explorer that way.

- `import-bodies --from-db FILE` fills gaps from another permanode's
  database or backup while this one keeps running; accepted only for
  blocks whose hash the own node reported and whose transactions add up.
  Outputs the sweep had marked as "spent in a gap" become ordinary spent
  outputs once their spend is on record (from an import or a backfill,
  canonical blocks only), and stale gap entries whose block has a body
  are closed at start.
- `/api/v1/tx/{txid}` gains `fee_breakdown`: base, per-input and
  per-output fees, the state-growth burn at the parent block's pressure
  multiplier, tip and miner share (`core/src/fees.rs` mirrors the
  consensus fee model).
- `/api/v1/halving`: live-state occupancy against the expansion
  threshold, the 18 hard-finalized headers deciding the next block, a
  sampled header history, the pressure multiplier and thresholds.
- `/api/v1/economics`: issued (mirrored emission schedule) vs burned
  (issued minus the node's supply, walked back through the recorded
  blocks and estimated from the headers' mint counter before that), net
  supply, annualized issuance, state pressure, the minimum burn until the
  next expansion split into pressure bands, development allocation with
  next payout and cumulative amounts per recipient, and recorded state
  activity over 24 h / 7 d / 30 d.
- `/api/v1/stats`: transactions and burned fees of the last 24 hours,
  addresses with a balance, database size, emission and burn totals,
  state capacity and slots until the next expansion; `blocks.log_slots`
  is stored so subsidies stay right across expansions.
- Header samples and the finalized window are cached per tip; the
  economics response takes about 100 ms, the halving response 12 ms.

## 0.1.16 - 2026-09-18

Everything since the first release, consolidated. These versions also
shipped an explorer frontend compiled into the binary; it has since
moved to its own project and is not part of this repository.

- **One binary.** Indexer and API run in a single process (`index` and
  `serve` subcommands to run either alone); `listen` and the new
  `donation_address` live in `permanode.toml`.
- **Address data.** Net amount per transaction for the viewed address
  (not the transaction's total output), the real counterparty instead of
  the address's own change, confirmations (final from 18 on), bounded
  pagination up to 200 per page, and a notice whenever the recorded
  figures cannot be complete - with the live balance from the node
  authoritative regardless.
- **Complete live balances.** A sweep over the node's UTXO state
  (`getStateMap` + `getSlot`) on start and every 30 minutes assigns every
  UTXO to its owner, verifies its totals against the node, and flags
  recorded outputs that were spent inside gaps so recorded balances can
  never exceed live ones.
- **Robustness.** Every block written in one SQLite transaction with
  `synchronous=FULL`, serialized schema migrations, indexes for the
  unspent-output queries, RPC timeouts, a fresh database starting at the
  oldest body the node still serves, API input validation and bounded
  pagination, ETag revalidation for static files.
- **Operations.** README for operators (install, service, public
  instance, updating), `contrib/watchdog/` (Telegram/journal) and
  `contrib/backup/` (daily database copy), release tarball with the
  example config.

## 0.1.0 - 2026-09-17

First release: indexer with the `getBlock` fallback decoder for the
node's marker-block RPC bug, JSON API, rich list.
