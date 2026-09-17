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

Status: the indexer (`indexer/`) works and has been tested against a live
mainnet node. A block-explorer frontend, based on mempool.space's design and
adapted to Parano1d, is planned next and not started yet.

## The indexer

```sh
cd indexer
cargo build --release
cp permanode.example.toml permanode.toml   # edit if your node RPC isn't the default
./target/release/parano1d-permanode-indexer --config permanode.toml
```

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
