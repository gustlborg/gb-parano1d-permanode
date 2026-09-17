# parano1d-permanode

A Parano1d node keeps full transaction bodies only for a short window
(a few minutes) before pruning them. Headers stay forever, the actual
transaction history does not. This program runs next to your own node,
records every transaction permanently before the node discards it, and
serves a block explorer over that history.

One binary, one config file, one SQLite database. Anyone running a
Parano1d node can run it; it only talks to the node's local RPC.

## Quick start

1. Have a Parano1d node running and fully synced, with its RPC on the
   default `127.0.0.1:9601`.
2. Download the latest `parano1d-permanode` binary from the releases page
   (Linux x86_64) or build it yourself (see below).
3. Run it in a directory of your choice:

   ```sh
   mkdir -p ~/permanode && cd ~/permanode
   ./parano1d-permanode
   ```

   The first start writes a commented `permanode.toml` next to the binary
   and a `permanode.sqlite3` database, then starts indexing and serving.

4. Open `http://127.0.0.1:8420/`.

Recording starts from the oldest block the node can still serve a body
for (about 40 blocks back). History from before the first start is gone,
the node itself no longer has it either.

## What you get

- **Indexer**: polls the node, stores every canonical block's header and
  all transactions (sender, inputs, outputs, amounts, fee, coinbase /
  development-payout flags, the Merkle-path data for the protocol's
  inclusion receipts) and logs chain reorganizations instead of
  overwriting them.
- **Explorer**: dashboard with the live block chain and mempool, block,
  transaction and address pages, live mempool, rich list. Plain
  HTML/CSS/JS, no build step, no third-party requests (fonts are bundled,
  SIL OFL). Compiled into the binary.
- **JSON API** under `/api/v1/` (`stats`, `blocks`, `block/height/{h}`,
  `block/hash/{h}`, `tx/{txid}`, `address/{a}`, `address/{a}/utxos`,
  `mempool`, `richlist`, `gaps`).
- **Live balances for every address**: the node's UTXO state is swept
  periodically (`paranoid_getStateMap` + `paranoid_getSlot`), so the rich
  list and address balances are complete and verified against the node's
  own totals, not reconstructed from partial history.

## Configuration

`permanode.toml` (see `permanode/permanode.example.toml` for every key):

- `rpc_url` — the node's RPC, default `http://127.0.0.1:9601`.
- `listen` — where the explorer listens, default `127.0.0.1:8420`. Keep it
  on loopback for a public instance and put a TLS reverse proxy in front.
- `retention_days` — how long to keep transaction detail (0 = forever,
  the default). Block headers are always kept.
- `poll_interval_seconds` — how often to check the node (default 5).
  Keep this well below the node's body window.
- `scan_slots_every_cycles` — how often the UTXO sweep runs (0 disables it).

Subcommands: `parano1d-permanode index` runs only the indexer,
`parano1d-permanode serve` only the explorer over an existing database.
The default runs both in one process.

## Running as a service

```ini
[Unit]
Description=Parano1d permanode (indexer + explorer)
After=network-online.target

[Service]
WorkingDirectory=/home/you/permanode
ExecStart=/home/you/permanode/parano1d-permanode
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
```

The indexer must not fall behind the node's pruning: an outage longer
than roughly 14 minutes leaves permanent gaps for that time (they are
listed under `/api/v1/gaps` and counted in the status bar).

For a public instance, a reverse proxy with TLS, e.g. Caddy:

```
explorer.example.org {
    reverse_proxy 127.0.0.1:8420
}
```

## Building

Rust (stable), a C compiler and libclang (`apt install clang`). The
indexer links the node's own `noid_chain` crate (git, pinned to the
node's `v1.1.0` tag) for the block decoder described below; that crate's
storage dependency needs bindgen.

```sh
cargo build --release
./target/release/parano1d-permanode --help
```

If bindgen fails with `'stdarg.h' file not found`, point it at your
GCC's resource headers:
`BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/gcc/x86_64-linux-gnu/13/include" cargo build --release`
(adjust the GCC version to what `ls /usr/lib/gcc/x86_64-linux-gnu/*/include/stdarg.h` shows).

Layout: `core/` is the SQLite schema and queries, `permanode/` the binary
(indexer, node RPC client, block decoder, explorer server), `frontend/site/`
the explorer UI, `docs/` design notes and the node bug report below.

## Working around a node RPC bug

The node's `getBlockDetails` / `getRecentTransactions` return no
transactions (`retained: null`) for roughly 1 in 45 canonical blocks: every
"marker" block of a multi-block commit, even though the node still holds
the body. Full analysis and a reproduction script:
`docs/node-rpc-marker-bug/`. The raw body is still served by
`paranoid_getBlock`, so the indexer decodes those blocks itself, linking
the node's own crates so the derived fields stay byte-identical to the
node's RPC. This is on by default (`getblock_fallback`); a continuous
self-check (`decoder_selfcheck`) decodes every normal block a second way
and counts any disagreement, in case a future node version changes the
wire format.

## Monitoring

`contrib/watchdog/` has a small stdlib-only Python watchdog with systemd
units: it checks the services, the node, the indexer's lag, the public
site and the disk every two minutes and reports changes plus a daily
heartbeat over Telegram (or just the journal). An indexer that silently
stops loses history the network will not hand out again, so run
something like it.

## Crash safety

SQLite in WAL mode with `synchronous=FULL`: every commit is fsynced, and
each block is written in one transaction, so a power cut leaves either the
whole block or nothing. For backups use
`sqlite3 permanode.sqlite3 ".backup copy.sqlite3"` while it runs, or a
filesystem snapshot of the directory.

## License

AGPL-3.0-or-later, see `LICENSE`.
