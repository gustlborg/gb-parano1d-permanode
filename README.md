# parano1d-permanode

A Parano1d node keeps full transaction bodies only for a short window
(a few minutes) before pruning them. Headers stay forever, the actual
transaction history does not. This program runs next to your own node,
records every transaction permanently before the node discards it, and
serves that history - together with live figures from the node - as a
JSON API that a block explorer or any other tool can be built on.

One binary, one config file, one SQLite database. Anyone running a
Parano1d node can run it; it only talks to the node's local RPC. The
explorer frontend at <https://noidexplorer.org> is a separate, private
project that runs on top of this API; the permanode itself ships with a
plain index page of the API and can serve any static frontend you point
it at (`site_dir`).

## Requirements

- A Parano1d node (v1.1.0 or later), fully synced, with its JSON-RPC on
  the default `127.0.0.1:9601`. Install it first following the official
  guide: <https://docs.parano1d.org/operate/node>. The permanode must run
  on the same machine (or reach the RPC over a private, authenticated
  channel - the node's RPC has no authentication and must never be
  exposed publicly).
- Linux x86_64 for the release binary (glibc 2.34 or newer: Ubuntu 22.04+,
  Debian 12+). Other platforms: build from source, see below.
- Almost no resources of its own: about 20 MB of memory and roughly
  10 MB of disk per day at today's transaction volume.

## Install

Replace `0.2.0` with the latest version from the
[releases page](https://github.com/gustlborg/gb-parano1d-permanode/releases):

```sh
V=0.2.0
curl -sSLO https://github.com/gustlborg/gb-parano1d-permanode/releases/download/v$V/parano1d-permanode-$V-linux-x86_64.tar.gz
curl -sSLO https://github.com/gustlborg/gb-parano1d-permanode/releases/download/v$V/SHA256SUMS
sha256sum --check SHA256SUMS          # must print: ... OK
tar -xzf parano1d-permanode-$V-linux-x86_64.tar.gz
sudo install -m 0755 parano1d-permanode /usr/local/bin/
```

Try it once in a directory of your choice:

```sh
mkdir -p ~/permanode && cd ~/permanode
parano1d-permanode
```

The first start writes a commented `permanode.toml` and a
`permanode.sqlite3` database into that directory, starts recording from
the oldest block the node can still serve a body for (about 40 blocks
back), sweeps every address's balance, and serves the API on
<http://127.0.0.1:8420/>. Stop it with Ctrl+C and set it up as a service
so it never stops again: history from before the first start is gone,
the node itself no longer has it either.

## Run as a service

A dedicated system user, the data under `/var/lib/permanode`, and
systemd keeping it alive:

```sh
sudo useradd --system --home-dir /var/lib/permanode --create-home --shell /usr/sbin/nologin permanode
sudo install -d -o permanode -g permanode -m 0750 /var/lib/permanode
cd /var/lib/permanode
sudo -u permanode timeout 5 parano1d-permanode || true    # first run writes permanode.toml and the database here
sudoedit -u permanode /var/lib/permanode/permanode.toml     # optional: listen, donation_address, retention_days ...
```

`/etc/systemd/system/parano1d-permanode.service`:

```ini
[Unit]
Description=Parano1d permanode (indexer + API)
Wants=network-online.target
After=network-online.target parano1d.service

[Service]
Type=simple
User=permanode
Group=permanode
WorkingDirectory=/var/lib/permanode
ExecStart=/usr/local/bin/parano1d-permanode --config /var/lib/permanode/permanode.toml
Restart=always
RestartSec=3
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/permanode
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now parano1d-permanode
sudo journalctl -u parano1d-permanode -f
```

The log shows every block as it is recorded, gaps, reorgs, and the sweep's
check against the node ("exact match with the node at #…"). The indexer
must not fall behind the node's pruning: an outage longer than roughly
14 minutes leaves permanent gaps for that time (listed under
`/api/v1/gaps` and counted in `stats`).

## Configuration

`permanode.toml` (see `permanode/permanode.example.toml` for every key):

- `rpc_url` — the node's RPC, default `http://127.0.0.1:9601`.
- `db_path` — the SQLite database, default `permanode.sqlite3` in the
  working directory.
- `listen` — where the API listens, default `127.0.0.1:8420`. Keep it
  on loopback for a public instance and put a TLS reverse proxy in front.
- `site_dir` — a directory with a static frontend (`index.html` plus
  assets) to serve in place of the built-in API index. Unknown paths get
  `index.html`, so a client-side router works; scripts and styles are
  sent with `no-cache` plus an ETag and fonts and images with a one-day
  lifetime, so an updated frontend shows up immediately even behind a
  CDN. Leave it unset to serve the API only.
- `retention_days` — how long to keep transaction detail (0 = forever,
  the default). Block headers are always kept.
- `poll_interval_seconds` — how often to check the node (default 5).
  Keep this well below the node's body window.
- `scan_slots_every_cycles` — how often the UTXO sweep runs, in polls
  (default 360, about 30 minutes; 0 disables it).
- `donation_address` — returned by `/api/v1/stats` for a frontend to
  show; leave empty for none.

Subcommands: `parano1d-permanode index` runs only the indexer,
`parano1d-permanode serve` only the API over an existing database.
The default runs both in one process; if either half dies the process
exits so systemd restarts both together.

## Public instance

Keep `listen` on loopback and publish it through a reverse proxy with TLS,
e.g. Caddy (automatic Let's Encrypt certificates):

```
explorer.example.org {
    reverse_proxy 127.0.0.1:8420
}
```

Firewall everything except SSH, 80/443 and the node's P2P port 9600.
The node's RPC port 9601 must stay on loopback. Consider a rate limit at
the proxy or CDN for `/api/`; the API caps list sizes (200 blocks, 200
transactions per address page) and validates every id, but a public
endpoint still deserves one. If you put Cloudflare in front with Bot
Fight Mode, note that it blocks default library user agents such as
Python's; API clients then need a descriptive `User-Agent`.

## Updating

```sh
# download and verify the new tarball as above, then:
sudo systemctl stop parano1d-permanode
sudo install -m 0755 parano1d-permanode /usr/local/bin/
sudo systemctl start parano1d-permanode
```

Database migrations run automatically on start. Never delete the database
during an update - it is the whole point.

## What you get

- **Indexer**: polls the node, stores every canonical block's header and
  all transactions (sender, inputs, outputs, amounts, fee, coinbase /
  development-payout flags, the Merkle-path data for the protocol's
  inclusion receipts) and logs chain reorganizations instead of
  overwriting them.
- **JSON API** under `/api/v1/`: `stats` (indexer state, chain and
  network figures, emission and burn totals), `blocks`,
  `block/height/{h}`, `block/hash/{h}`, `tx/{txid}` with the fee split
  into miner share and consensus burn, `address/{a}` (recorded and live
  balance, transactions, notices when the recorded figures cannot be
  complete), `address/{a}/utxos`, `mempool`, `richlist`, `gaps`,
  `halving` (live-state occupancy against the expansion threshold, the
  finalized trigger window, sampled header history) and `economics`
  (issued vs burned over height, state pressure and burn tiers, minimum
  burn to the next expansion, development allocation, recorded state
  activity). Amounts are µNOID; totals that can exceed 2^53 are decimal
  strings. The root page lists the endpoints.
- **Frontend of your choice**: none is built in; `site_dir` serves any
  static site over the API with sensible caching. Final data (blocks and
  transactions 18 confirmations deep) is served with a one-hour cache
  lifetime, everything else uncached.
- **Live balances for every address**: the node's UTXO state is swept
  periodically (`paranoid_getStateMap` + `paranoid_getSlot`), so the rich
  list and address balances are complete and verified against the node's
  own totals, not reconstructed from partial history.

## How it works

- **Blocks and transactions** come from `getBlockDetails` every poll
  (default 5 s), with the `getBlock` decoder as fallback (see below). A
  block is written as one transaction; reorgs mark the old block orphaned
  and store the replacement, nothing is overwritten. Blocks and
  transactions are final at 18 confirmations (the protocol's maximum
  reorg depth is 17); responses carry the confirmation count.
- **Balances** come from two sources, shown side by side. *Recorded*
  figures are computed from the transactions this permanode has stored,
  so they only cover activity since its first start. *Live* figures come
  straight from the node's current UTXO state and are always complete.
  Whenever the recorded figures cannot be complete for an address (it
  was active before the permanode started, or some of its outputs were
  spent inside gaps), the address response says so; the live balance is
  authoritative regardless.
- **The UTXO sweep** reads every live UTXO of the node (`getStateMap` to
  find the populated state segments, `getSlot` for each slot in them) on
  the first poll after start and then every `scan_slots_every_cycles`
  polls (default 360, about 30 minutes), assigns each UTXO to its owner
  and stores balance and UTXO count per address. Every run checks its own
  total against the node's count and logs the result; a shortfall at an
  unchanged tip is logged as a warning. The same pass reconciles the
  recorded history: a recorded output the node no longer holds, with no
  recorded transaction spending it, was spent in a block whose body this
  permanode never had; it is flagged and dropped from the recorded
  balance, and the address response says so. This is what makes the rich
  list and the address balances complete for addresses that never appear
  in the recorded history. The individual UTXOs are not stored;
  `address/{a}/utxos` loads them from the node on request.
- **Gaps** are heights whose body the node had already pruned when the
  indexer got to them (for example after an outage longer than the
  node's serving window, or on a node that just synced from a snapshot).
  They are listed under `/api/v1/gaps` and counted in `stats`; heights
  still inside the serving window are retried automatically.
  Older ones can be filled from another permanode, see below.

## Filling gaps from another permanode

Any permanode that stayed online has the bodies yours missed. Copy its
database (or one of its backups - same format), then:

```
parano1d-permanode -c permanode.toml import-bodies --from-db other-permanode.sqlite3
```

This can run while your permanode is running. Only gaps are touched, and
a body is accepted only for a block whose hash your own node reported and
whose transactions add up (inputs, outputs, fees, coinbase). Imported
blocks show `body_source = import` in the database and are logged as
"recovered via import".

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

Layout: `core/` is the SQLite schema, queries and the mirrored consensus
rules (emission schedule, fee model), `permanode/` the binary (indexer,
node RPC client, block decoder, API server), `frontend/site/` the
built-in index page (replace it before building to compile a frontend
into the binary), `docs/` the node bug report below.

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
units: every two minutes it checks that the services are active, the
node answers, the indexer is not lagging behind the node, blocks keep
arriving, the public instance is reachable and in sync, and disk and memory
have headroom. It reports every change (new problem, resolved problem)
and one daily heartbeat over Telegram, or only to the journal if no bot
is configured. Setup, including how to create the Telegram bot and find
the chat id, is in `contrib/watchdog/README.md`. An indexer that silently
stops loses history the network will not hand out again, so run
something like it.

## Backups and crash safety

SQLite in WAL mode with `synchronous=FULL`: every commit is fsynced, and
each block is written in one transaction, so a power cut leaves either the
whole block or nothing. Back the database up regularly while it runs, e.g.
`sqlite3 permanode.sqlite3 ".backup copy.sqlite3"` or with the daily
systemd timer in `contrib/backup/` (Python stdlib, no CLI needed), and
keep a copy off the machine: the database is the one thing that cannot be
re-downloaded from the network.

## Troubleshooting

- `RPC call … failed to send`: the node is not running or `rpc_url` is
  wrong. The indexer keeps retrying every poll.
- Gaps right after installing: the node was still syncing, or synced from
  a snapshot and only holds bodies from that point. Bodies inside the
  node's window are retried automatically; older ones can be imported
  from another permanode's database (`import-bodies`, see above).
- `Address already in use`: something else listens on `listen`; change
  the port or stop the other program.
- Building from source fails in bindgen: see Building below.

## License

AGPL-3.0-or-later, see `LICENSE`.
