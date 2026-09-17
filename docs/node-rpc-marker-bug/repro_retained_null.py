#!/usr/bin/env python3
"""Reproduction for: getBlockDetails returns retained=null although getBlock serves the body.

Scans the last N canonical heights (default 42 = local body serving window) and
prints every height where `paranoid_getBlock` returns a body but
`paranoid_getBlockDetails` reports `retained: null`.  Needs only python3 and a
node RPC on loopback.  Usage: repro_retained_null.py [N] [rpc-url]
"""
import json, struct, sys, urllib.request

N = int(sys.argv[1]) if len(sys.argv) > 1 else 42
RPC = sys.argv[2] if len(sys.argv) > 2 else "http://127.0.0.1:9601"

def rpc(method, params):
    req = urllib.request.Request(
        RPC,
        data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
        headers={"content-type": "application/json"},
    )
    return json.load(urllib.request.urlopen(req))["result"]

def header_from_body(raw_hex):
    """Decode the wire header (noid_chain/src/wire.rs, little endian) from a raw block."""
    b = bytes.fromhex(raw_hex)
    assert b[0] == 0xB3, "block wire marker"
    o = 1
    prev = b[o:o+32].hex(); o += 32
    state = b[o:o+32].hex(); o += 32
    tx_root = b[o:o+32].hex(); o += 32
    ts, height = struct.unpack("<QQ", b[o:o+16]); o += 16
    o += 32 + 16 + 32 + 4 + 8 + 8        # miner, nonce, target, log_slots, active_slot_count, alloc_counter
    ntx, = struct.unpack("<I", b[o:o+4])
    return dict(prev_hash=prev, state_root=state, tx_root=tx_root, timestamp=ts, height=height, ntx=ntx)

tip = rpc("paranoid_blockCount", [])
affected = []
for h in range(tip - N + 1, tip + 1):
    details = rpc("paranoid_getBlockDetails", [h])
    body = rpc("paranoid_getBlock", [h])
    if details is None:
        continue
    has_details = details["retained"] is not None
    if body is not None and not has_details:
        hdr = header_from_body(body)
        canon = details["header"]
        same = (hdr["prev_hash"] == canon["prev_hash"] and hdr["tx_root"] == canon["tx_root"]
                and hdr["state_root"] == canon["state_root"] and hdr["timestamp"] == canon["timestamp"]
                and hdr["height"] == h)
        affected.append(h)
        print(f"height {h}: getBlockDetails.retained=null  but  getBlock={len(body)//2} bytes "
              f"({hdr['ntx']} tx), body header == canonical header: {same}")
print(f"tip={tip}, scanned {N} heights, affected: {len(affected)} -> {affected}")
