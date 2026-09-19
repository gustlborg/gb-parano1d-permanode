# Changelog

Notable changes per release. Commit history has the details.

## Unreleased

- Dashboard grows to twelve cards in two rows of six: UTXOs until
  halving (from `getStateInfo`), fees burned since genesis (mirrored
  emission schedule minus the circulating supply) with the last 24 hours
  from recorded blocks in the tooltip, transactions in the last 24 hours, addresses with
  a balance, database size on disk with the growth rate, and how long
  this instance has been recording.
- Blocks store `log_slots` so subsidies stay right across future
  expansions.
- Stat cards follow the design update: equal height across both rows,
  a little taller, and every card carries a round "i" in its corner
  (hover it for the explanation) instead of a dotted underline on the
  value.
- `ROADMAP.md`.
- **Economics page** (`/economics`, nav entry after Halving): twelve
  cards (net supply, total issued and burned, current reward, annualized
  issuance and inflation as labelled projections, live UTXOs, occupancy,
  growth multiplier, next expansion, qualifying headers, next
  development payout), an issued-vs-burned chart over block height on
  one NOID axis (log or linear; burned exact at the tip and walked back
  through the recorded blocks), state pressure and burn tiers, fee
  composition, state creation vs consolidation over 24 h / 7 d / 30 d
  from the records, the supply model and the development allocation
  with cumulative amounts per recipient. Backed by `/api/v1/economics`.
- Halving page: ten cards (growth multiplier, burn per net-new UTXO,
  next pressure threshold, a labelled estimate to the threshold added),
  a "How the halving works" explanation and the pressure tiers and
  consolidation rule in the consensus list.
- Transaction pages: a fee-breakdown card (total, miner-claimable,
  burned with a split bar and the components) replaces the two fee rows;
  the type row states the shape and net state change.
- `import-bodies --from-db FILE` fills gaps from another permanode's
  database or backup while this one keeps running; accepted only for
  blocks whose hash the own node reported and whose transactions add up.
  Outputs the sweep had marked as "spent in a gap" become ordinary spent
  outputs once their spend is on record, from an import or a backfill.
- Transaction pages split the fee into what the miner claimed and what
  consensus burned (base + per-input + per-output + tip vs. the
  state-growth fee on net-new UTXO slots at the parent block's state
  pressure), via `fee_breakdown` on `/api/v1/tx/{txid}`.
- Dashboard wording aligned with the protocol's economics: "Net supply"
  (issued minus burned, the node's figure) instead of "Circulating
  supply"; the burn tooltip lists the 1x/2x/4x/8x pressure tiers; the
  halving tooltip states the 12 582 912-UTXO threshold and the 10-of-18
  finalized-header rule; block reward explains the 90/5/5 split with the
  O(1) Network Fund and Parano1d Lab; fee floor is labelled as the node's
  relay policy, not the consensus minimum. Every dashboard card now has a
  tooltip.
- **Halving page** (`/halving`, in the nav between Block and
  Transaction): live-state occupancy against the 75 % expansion
  threshold, chart of live UTXOs over block height with a projection
  from recent growth (recent window or full scale), the finalized
  18-header trigger window as it stands on chain, reward tiers per state
  domain and the consensus rules behind them. Backed by a new
  `/api/v1/halving` endpoint that samples block headers incrementally.

## 0.1.16 - 2026-09-18

Everything since the first release, consolidated:

- **One binary.** Indexer and explorer run in a single process (`index`
  and `serve` subcommands to run either alone); the frontend is compiled
  in; `listen` and the new `donation_address` live in `permanode.toml`.
- **Explorer redesign.** Dashboard with the animated block chain and
  mempool faces (one cell per page, shaded by fee rate), block,
  transaction, address, live mempool and rich list pages, instant
  tooltips, favicon, footer and an `/about` page (independent community
  project, no warranty, data sources, license, donation address).
- **Address pages.** Net amount per transaction for the viewed address
  (not the transaction's total output), real counterparty instead of the
  address's own change, confirmations with a "final" mark from 18 on,
  page selector and 25/50/100/150/200 per page, full timestamps when the
  time column is switched to absolute, and a notice whenever the recorded
  figures cannot be complete - with the live balance from the node stated
  as authoritative regardless.
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

First public release: indexer with the `getBlock` fallback decoder for the
node's marker-block RPC bug, explorer, JSON API, rich list.
