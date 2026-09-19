# Roadmap

Ideas and requests, roughly in order. Nothing here is a promise.

- **Start-up sweep retry.** The balance sweep runs once right after
  start; if the node is not up yet it currently waits for the regular
  interval (30 min). Retry until it has succeeded once.
- **Network node count.** A P2P network has no registry, so the number
  of nodes is unknown. A crawler that periodically contacts every peer
  the node has ever seen (`peers.json`) and counts the ones that answer
  would give an estimate, the way Bitcoin's public node counters work.
  Until then the dashboard shows this node's connected peers.
- **Permanode count.** Only possible if permanodes opt in to announce
  themselves somewhere; would need a small registry and an explicit
  `announce = true` config switch. Undecided.
- **Chain statistics over time.** Charts for hashrate, block time, fees,
  transaction volume and UTXO growth from the recorded history.
- **Search improvements.** Prefix search and suggestions for addresses
  and txids.
- **Receipts.** Reconstruct the protocol's Merkle inclusion receipts for
  any recorded transaction from the stored page hashes (combination rule
  still to be derived from the node source).
