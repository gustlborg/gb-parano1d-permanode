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
and have been tested against a live mainnet node, including a fallback
decoder that recovers ~2.2% of blocks a node RPC bug would otherwise make
permanently unrecoverable (see below). The frontend covers the core views
(dashboard, blocks, transactions, addresses, live mempool, rich list).

## Layout

This is a Cargo workspace:

- `core/` — shared library: the SQLite schema and all read/write queries.
- `indexer/` — the binary that polls a node and fills the database.
- `api/` — a small JSON API (axum) that reads the database and also serves
  the static frontend, so a self-hoster only needs these two binaries plus
  the `frontend/site/` directory.
- `frontend/site/` — the explorer UI: plain HTML/CSS/JS (ES modules), no
  build step, no framework, no third-party requests (fonts are bundled
  under `fonts/`, both SIL OFL). Six views: dashboard with the animated
  block chain, block, transaction, address, live mempool, rich list. The
  design tokens and layout rules it follows are documented in
  `docs/design/README.md`.

## Building

Needs a C compiler and libclang (`clang`) in addition to Rust — the indexer
links the node's own `noid_chain` crate (via git, pinned to the node's
`v1.1.0` tag) to work around a node RPC bug (see below), and that crate's
storage dependency needs bindgen. On Ubuntu: `apt install clang`. If
bindgen fails with `'stdarg.h' file not found`, your GCC's own resource
headers aren't where clang expects them; point it there explicitly, e.g.
`BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/gcc/x86_64-linux-gnu/13/include" cargo build --release`
(adjust the GCC version to whatever `ls /usr/lib/gcc/x86_64-linux-gnu/*/include/stdarg.h` shows).

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

### Working around a node RPC bug (getBlock fallback)

The node's `getBlockDetails`/`getRecentTransactions` RPCs report no
transactions (`retained: null`) for roughly 1 in 45 canonical blocks —
every "marker" block of a multi-block commit (a catch-up suffix of ≥2
blocks, or a reorg that applies ≥2 blocks) — even though the node still
has the block body; it's just served through the wrong internal accessor.
Full writeup: `docs/ANLEITUNG-getblock-decoder.md`. Since the RPC still
serves the raw body through `paranoid_getBlock`, the indexer decodes those
blocks itself (linking the node's own crates so the field derivation stays
byte-identical to the node's own RPC) instead of recording a permanent
gap. This is on by default (`getblock_fallback = true`); a continuous
self-check (`decoder_selfcheck = true`) decodes every normal block a
second way too and logs/counts any disagreement, in case a future node
version changes the wire format. Gaps recorded before this existed, or
outside the ~42-block window the node still serves bodies for, cannot be
recovered — there is no RPC path to older bodies.

### Configuration

See `indexer/permanode.example.toml`. The two settings that matter most:

- `retention_days` — how long to keep full transaction detail before
  pruning it (0 = forever). Block headers are always kept regardless.
- `poll_interval_seconds` — how often to check the node for new blocks.

## License

AGPL-3.0 (see `LICENSE`) — proposed default, not yet finalized.
