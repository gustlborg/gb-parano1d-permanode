# `getBlockDetails` / `getRecentTransactions` return `retained: null` for the intermediate blocks of every multi-block commit, although the node still holds the body

**Affected version:** parano1d v1.1.0 (tag `v1.1.0`, commit `8f3195e`), mainnet, Linux x86_64, `--mode node`, no local mining.
**Observed:** 2026-09-17, node running continuously since 2026-09-11.
**Severity (from an explorer/indexer point of view):** every ~45th canonical block (2.2 % of all heights since the v1.1.0 restart) reports no transactions over RPC — coinbase, dev payout and user transactions of that block cannot be listed or verified through `getBlockDetails`/`getRecentTransactions`, and the official GUI shows the same blocks as empty. The data is *not* lost; it is stored in MDBX and served by `getBlock`, only the details RPC refuses it.

---

## 1. Symptom

For a canonical height `H` well inside the 42-block serving window:

```
paranoid_getBlockDetails [108338]  ->  { header: {...hash: "ebc12af1…"}, retained: null }
paranoid_getBlock        [108338]  ->  "b3…" (540 bytes, 1 transaction)
paranoid_getBlockDetails [108337]  ->  retained: { transactions: [1 tx], ... }   (neighbour is fine)
paranoid_getBlockDetails [108339]  ->  retained: { transactions: [1 tx], ... }   (neighbour is fine)
```

Decoding the wire header out of the `getBlock` bytes (`noid_chain/src/wire.rs`, little-endian) and comparing with `getBlockHeader [108338]` gives identical `prev_hash`, `state_root`, `tx_root`, `timestamp` and `height` — i.e. the served body **is** the canonical block, not a stale fork body.

`repro_retained_null.py` (attached) scans the last 42 heights against a running node and lists every height where `getBlock` returns bytes but `getBlockDetails.retained` is `null`. On our node at tip 108365 it found five such heights at once: `108332, 108338, 108344, 108345, 108357`.

The docs (`docs/reference/rpc.md:31`) name only one reason for `null`: "Old block bodies return `null` after the 42-block retention window". These heights are 8–33 blocks below the tip.

## 2. Root cause (code walk, v1.1.0)

### 2.1 Bodies are stored per height, terminals differ per commit path

`MdbxStore::commit_applied_next_block` stores every accepted non-genesis block as
`T_RECENT_BLOCKS[height] = block bytes` **and** `T_HISTORY_STEP_TERMINALS[height] = terminal bytes` (`noid_chain/src/storage/mdbx_store.rs:3397-3411`).

What goes into the terminal slot depends on the `AcceptedBlockCommit` variant (`mdbx_store.rs:3100-3131`):

| variant | terminal stored | produced by |
|---|---|---|
| `Complete` / `CompleteObjects` | the full verified HistoryStep terminal | single-block commits, and the **last** block of a suffix |
| `RecursiveSuffix { authority_tip_height, authority_tip_hash }` | a fixed-size *recursive suffix marker* pointing at the tip that carries the real terminal | every block **before the last** in a multi-block suffix |

Both producers:

* normal catch-up: `MdbxChainContext::apply_verified_recursive_suffix_block` — `final_bundle` is only built for the authority tip, every other block is committed as `RecursiveSuffix` (`noid_chain/src/storage/mdbx_context.rs:1587-1593`);
* reorg: `apply_verified_reorg_suffix_with_applier_indexed` builds `replacement_objects` as `CompleteObjects` for `index == last_index` and `RecursiveSuffix` for all others (`mdbx_context.rs:1829-1845`, and identically in the non-indexed variant at `:1680`).

That is by design ("one bounded terminal copy, not one per block") and nothing later upgrades a marker to a complete terminal — `commit_reorg` only *rebinds* existing markers to a new authority (`mdbx_store.rs:3691-3741`).

### 2.2 The bundle accessor deliberately hides marker blocks…

`MdbxStore::get_recent_accepted_block_bundle_bounded(height)` (`mdbx_store.rs:2074-2137`) reads body + terminal and returns

```rust
if recursive_suffix_marker_authority(&terminal_bytes, height, semantic_header_id(&block.header), None).is_some() {
    return Ok(None);          // mdbx_store.rs:2114-2123
}
```

so for a marker block it answers `None` although `T_RECENT_BLOCKS[height]` holds the body. The unit test `mdbx_context.rs:3198-3204` pins exactly this contract: after a two-block recursive suffix, `get_recent_block(1)` is `Some` while `get_recent_accepted_block_bundle_bounded(1)` is `None`. For its intended consumers (snapshot generation, peer serving — "intermediate local markers are not proof payloads") that is correct.

### 2.3 …but the RPC uses it merely to get at the block bytes

* `get_block_details` — `noid_rpc/src/server.rs:1807-1827` calls `get_recent_accepted_block_bundle_bounded(height)` and maps `None` to `retained: None`. Everything it then puts into `RetainedBlockInfo` is derived from `bundle.block_bytes()`; the terminal is only used for the informational `history_step_bytes` length (`server.rs:2028`).
* `get_recent_transactions` — `server.rs:2066-2080` iterates the retention window and silently skips every height where the bundle accessor returns `None`; those blocks' transactions never appear in any page.
* `get_block` — `server.rs:1797-1804` uses `store.get_recent_block(height)` (plain `T_RECENT_BLOCKS` read, `mdbx_store.rs:2040-2044`) and therefore *does* serve the body. This is the asymmetry visible in section 1.

So the RPC inherits a proof-payload policy for what is, at the RPC level, a plain "give me the transactions of canonical block H" query.

### 2.4 Same accessor in wallet crash-recovery

`reconcile_receipts_at_startup` (`noid_node/src/wallet/mod.rs:326-337`) also walks the retention window through `get_recent_accepted_block_bundle_bounded` and `continue`s on `None`, so an outgoing receipt whose durable write was lost in the crash window cannot be recovered if the transaction sits in a marker block. (The live wallet path is not affected: `update_wallet_for_block` runs per block in the suffix applier, `noid_node/src/main.rs:4415`, and the reorg path installs a fresh snapshot with all replacement blocks, `main.rs:4472-4495`.)

### 2.5 The official GUI is affected too

`noid_gui/src/backend.rs:1122` (`getBlockDetails`) and `:1139`, `:1219` (`getRecentTransactions`) — the GUI block/transactions views show these blocks without transactions.

## 3. Evidence that this is the whole story

Our indexer polls `getBlockDetails` every 5 s for every new height and records each height whose body was already `null` on the *first* attempt (`first_seen_at` is milliseconds after the height appeared, so this is not a slow-poller problem). Over the observation window 107822–108341 (2026-09-17 12:33–15:12 UTC) it recorded **20** such heights.

Cross-checking against the node log (`parano1d-node.log`, script `log_vs_gaps.py`):

```
Mehrblock-Ereignisse im Log: 17  -> vorhergesagte Marker-Höhen: 20
Gaps in der Permanode-DB:    20
Übereinstimmung: 20   nur Log-Vorhersage: []   nur DB-Gap: []
```

Every one of the 20 heights is exactly the `n-1` intermediate height(s) of a multi-block commit, and every multi-block commit in the window produced exactly its `n-1` gaps — no exceptions in either direction. Two representative log excerpts:

*Catch-up suffix of two blocks (blocks mined 2 s apart), no reorg:*
```
13:51:47 HeaderDAG-selected exact suffix plan admitted … base_height=108096 target_height=108098 admission="started"
13:51:50 header-first exact suffix application completed … target_height=108098 height=108098 blocks=2 bytes=1403 complete=true
```
→ 108097 marker (`retained: null`), 108098 complete.

*Reorg, one block reverted, two applied:*
```
15:12:08 reorg: reverting height 108338..108337 depth=1 new_blocks=2
15:12:09 reorg: applied new block height=108338
15:12:10 reorg: applied new block height=108339
15:12:10 atomic one-terminal exact reorg completed … new_tip=108339 reverted=1 applied=2
```
→ 108338 marker (`retained: null`), 108339 complete.

Reorgs with `reverted=1 applied=1` (same-height replacement, e.g. 107869, 108049, 108310) do **not** produce the symptom — the single replacement block is the suffix tip and gets a complete terminal. This confirms the marker, not the reorg itself, is the trigger.

### Frequency since the v1.1.0 restart (heights 83179–108364, 25 186 blocks)

| commit kind | count | intermediate (marker) heights |
|---|---|---|
| exact-suffix, 1 block | 24 385 | 0 |
| exact-suffix, 2–7 blocks (+1× 25) | 282 | 349 |
| reorg, 1 reverted / 1 applied | 113 | 0 |
| reorg, ≥2 applied | 168 | 211 |
| **total heights without RPC body** | | **560 = 2.2 %** |

Two-block suffixes dominate (256 of 282): with ~20 s block time and ~0.8 s per terminal verification, two blocks found a few seconds apart are routinely committed as one plan. This is normal operation, not an outage — which is why the symptom is permanent and steady.

## 4. Impact

* Any external indexer/explorer that follows the documented `getBlockDetails` API loses ~2 % of all blocks' transaction lists permanently (the body is pruned after 42 blocks, and there is no later chance to read it through this method).
* Coinbase and development-payout transactions of those blocks cannot be shown or verified via the details API; users cannot confirm a payment that landed in such a block through `getRecentTransactions` or the GUI.
* `retained: null` currently has two meanings (outside the 42-block window vs. marker block) that callers cannot distinguish; the docs describe only the first.

## 5. Suggested fix

The RPC needs the canonical **body**, not a proof bundle. Two small changes, no consensus/storage impact:

1. `get_block_details` / `get_recent_transactions`: read the body via a canonical-checked body accessor instead of the bundle accessor — e.g. the existing pinned-view `get_recent_block` (`mdbx_store.rs:281-315`, which already verifies `T_HEADERS[height]` hash == body hash), exposed as a store method, or `get_recent_block` + the same header check. Build `RetainedBlockInfo` from those bytes exactly as today. For `history_step_bytes` report the actual terminal length or `0`, and optionally add a field such as `terminal: "complete" | "recursive_marker"` so the distinction stays observable.
2. `reconcile_receipts_at_startup`: same substitution, so receipt recovery covers the whole retention window.

Optionally document in `rpc.md` that `retained` is `null` only outside the serving window, and keep `getBlock` and `getBlockDetails` consistent with each other (today they disagree for every marker height).

If instead the intent is that marker blocks must never be served through RPC, then `getBlock` has the opposite bug and the documentation needs to say that ~2 % of canonical blocks have no readable body — but that would make a complete transaction index impossible for third parties, so the body-accessor route seems clearly preferable.

## 6. Reproduction

```
python3 repro_retained_null.py 42            # against http://127.0.0.1:9601
```
Prints every height in the last 42 where `getBlock` has bytes and `getBlockDetails.retained` is `null`, and verifies that the body's wire header equals the canonical header. On a mainnet node that has been running for a few hours it should list 1–5 heights at any time. Alternatively grep the node log for `blocks=2` / `applied=2` and query the height just below `target_height` / `new_tip`.

Attached scripts: `repro_retained_null.py`, `check_body_header.py` (header-from-body comparison for given heights), `log_vs_gaps.py` (log ↔ indexer cross-check, needs our SQLite file).

## 7. Side observation (separate topic)

In the same window our node saw 281 reorgs in 25 186 blocks (one every ~90 blocks). Of the 17 orphaned blocks recorded by our indexer, 14 were mined by `o1mlk6uluf2dghqzz0etew4u9wq6clnnr4y4wuv20q2m255mj9crjquzyr4r` (84 blocks seen, 14 orphaned = 17 %) and 3 by `o1rjvvf94fsha5…` (20 seen); every other miner had 0. Possibly a propagation/latency problem on that miner's side rather than a node bug, reported for completeness only.
