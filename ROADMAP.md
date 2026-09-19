# Roadmap

Ideas and requests, roughly in order. Nothing here is a promise.

- **Start-up sweep retry.** The balance sweep runs once right after
  start; if the node is not up yet it currently waits for the regular
  interval (30 min). Retry until it has succeeded once.
- **Network node count.** A P2P network has no registry, so the number
  of nodes is unknown. A crawler that periodically contacts every peer
  the node has ever seen (`peers.json`) and counts the ones that answer
  would give an estimate (a lower bound: nodes behind NAT without a port
  forward do not answer), the way Bitcoin's public node counters work.
- **Permanode count.** Only possible if permanodes opt in to announce
  themselves somewhere; would need a small registry and an explicit
  `announce = true` config switch. Undecided.
- **Time series in the API.** Hashrate, block time, fees, burn,
  transaction volume and UTXO creation vs consumption per day from the
  recorded history, for charts in frontends.
- **Peer backfill over the API.** An export endpoint with the full
  recorded body so a permanode can fill its gaps from another one over
  HTTPS instead of from a database copy.
- **Search.** Prefix lookup for addresses and txids.
- **Receipts.** Reconstruct the protocol's Merkle inclusion receipts for
  any recorded transaction from the stored page hashes (combination rule
  still to be derived from the node source).
