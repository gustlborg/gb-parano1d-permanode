# parano1d-permanode

A Parano1d node only keeps full transaction bodies for a short window (18
blocks by protocol, roughly the last few minutes) before pruning them —
headers stay forever, but the actual transaction history does not. This
project fills that gap: a small companion program you run next to your own
Parano1d node that watches every new block as it arrives and records the
transactions permanently, in a compact form, before the node discards them.

It is meant to be installed by anyone running a Parano1d node, not just on
one central server. Point it at your own node's local RPC and it builds up
its own local, permanent transaction history for as long as it runs.

Status: the indexer, the API server and a first explorer frontend all work
and have been tested against a live mainnet node. The frontend covers the
core views (blocks, transactions, addresses) but is not feature-complete.

## Layout

This is a Cargo workspace:

- `core/` — shared library: the SQLite schema and all read/write queries.
- `indexer/` — the binary that polls a node and fills the database.
- `api/` — a small JSON API (axum) that reads the database and also serves
  the static frontend, so a self-hoster only needs these two binaries plus
  the `frontend/site/` directory.
- `frontend/site/` — the explorer UI: plain HTML/CSS/JS (ES modules), no
  build step, no framework. Visually inspired by mempool.space's dark theme
  and block-grid visualization, but written independently — mempool.space's
  actual codebase is ~750 files, much of it tied to Bitcoin/Lightning/Liquid
  features that have no Parano1d equivalent, and its name and logos are
  trademarked regardless of the code license.

## Running it

```sh
cargo build --release

# 1. the indexer, next to your own already-running Parano1d node
cd run   # or any directory you want the database and config in
cp ../indexer/permanode.example.toml permanode.toml   # edit if your node RPC isn't the default
../target/release/parano1d-permanode-indexer --config permanode.toml

# 2. the API + frontend, pointed at that same database
../target/release/permanode-api \
  --db-path permanode.sqlite3 \
  --site-dir ../frontend/site \
  --listen 127.0.0.1:8420
```

Then open `http://127.0.0.1:8420/`. `--listen` defaults to loopback only;
change it if you want it reachable from elsewhere, e.g. behind your own
reverse proxy.

It expects a Parano1d node already running and reachable on its RPC port
(default `127.0.0.1:9601`, no separate setup needed on the node side beyond
running it normally). It creates/uses a SQLite database and starts recording
from the current chain tip forward — it cannot recover transaction history
from before it was first started, since the node itself no longer has that
data either.

### What gets recorded

Per transaction: timestamp, block height, block hash and parent hash, txid,
sender (input owner, null for coinbase/dev-payout), inputs, outputs and
amounts, fee, a coinbase/development-payout flag, the raw Merkle-path data
needed to reconstruct the protocol's own inclusion receipts later, and a
canonical/orphaned status history (chain reorganizations are logged, not
silently overwritten, so a transaction that was briefly included in a block
that later got reorged out remains visible as such).

### Configuration

See `indexer/permanode.example.toml`. The two settings that matter most:

- `retention_days` — how long to keep full transaction detail before
  pruning it (0 = forever). Block headers are always kept regardless.
- `poll_interval_seconds` — how often to check the node for new blocks.

## License

AGPL-3.0 (see `LICENSE`) — proposed default, not yet finalized.
