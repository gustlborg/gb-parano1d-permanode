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
- Explanations on stat cards moved from a dotted underline on the value
  to a round "i" in the card's corner; hover it for the tooltip.
- `ROADMAP.md`.
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
