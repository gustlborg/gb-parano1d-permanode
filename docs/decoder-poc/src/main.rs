//! PoC: decode `paranoid_getBlock` bytes into the same transaction structure
//! that `paranoid_getBlockDetails` returns, using the node's own crates.
//! Mirrors noid_rpc/src/server.rs:1838-2010 (v1.1.0) step by step.
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Serialize, Debug, PartialEq)]
struct InputInfo { page: u16, lane: u8, slot_index: u64, amount_micronoid: u64, creation_id: u64 }
#[derive(Serialize, Debug, PartialEq)]
struct OutputInfo { page: u16, lane: u8, slot_index: u64, amount_micronoid: u64, owner: String, creation_id: u64 }
#[derive(Serialize, Debug, PartialEq)]
struct TxInfo {
    position: u16, txid: String, page_count: u16, live_inputs: u16, live_outputs: u16,
    fee_micronoid: u64, coinbase: bool, development_payout: bool, epoch_anchor: String,
    input_owner: Option<String>, input_sum_micronoid: String, output_sum_micronoid: String,
    page_hashes: Vec<String>, inputs: Vec<InputInfo>, outputs: Vec<OutputInfo>,
}
#[derive(Serialize, Debug)]
struct Decoded {
    proof_class: String, logical_transactions: u16, user_pages: u16, live_inputs: u16, live_outputs: u16,
    reward_micronoid: u64, total_fees_micronoid: String, block_bytes: u64, transactions: Vec<TxInfo>,
}

/// The decoder. `expected_hash` = canonical header hash from getBlockHeader (hex).
fn decode_block(bytes: &[u8], expected_height: u64, expected_hash: &str) -> Result<Decoded> {
    let block = noid_chain::Block::from_bytes(bytes).map_err(|e| anyhow!("decode block: {e:?}"))?;
    let header = &block.header;
    // Bind the body to the canonical header before trusting anything in it.
    if header.height != expected_height { bail!("height mismatch"); }
    let hash = hex::encode(noid_chain::block_header::block_id(header));
    if hash != expected_hash { bail!("body hash {hash} != canonical {expected_hash}"); }

    let stream = noid_chain::validate_block_page_stream(&block.transactions)
        .map_err(|e| anyhow!("page stream: {e}"))?;
    let logical_txids = noid_chain::try_compute_logical_txids(&block.transactions)
        .map_err(|e| anyhow!("txids: {e}"))?;
    let coinbase = block.transactions.first().context("missing coinbase")?;
    let reward_micronoid = coinbase.body.live_outputs().map(|(_, o)| o.amount).fold(0u64, u64::saturating_add);
    let coinbase_outputs = u16::try_from(coinbase.body.live_outputs().count())?;
    let development_outputs = if stream.has_development_payout {
        u16::try_from(block.transactions[1].body.live_outputs().count())?
    } else { 0 };
    let minted_outputs = u64::from(stream.live_outputs) + u64::from(coinbase_outputs) + u64::from(development_outputs);
    let mut alloc_cursor = header.alloc_counter.checked_sub(minted_outputs).context("alloc underflow")?;

    let mut transactions = Vec::new();
    // --- coinbase (position 0) ---
    let mut cb_outputs = Vec::new();
    for (lane, o) in coinbase.body.live_outputs() {
        alloc_cursor += 1;
        cb_outputs.push(OutputInfo { page: 0, lane: lane as u8, slot_index: u64::from(o.slot_index), amount_micronoid: o.amount,
            owner: o.owner.to_bech32(), creation_id: noid_chain::consensus::params::coinbase_creation_id(header.height) });
    }
    let cb_txid = hex::encode(logical_txids[0].0);
    transactions.push(TxInfo { position: 0, txid: cb_txid.clone(), page_count: 1, live_inputs: 0, live_outputs: coinbase_outputs,
        fee_micronoid: 0, coinbase: true, development_payout: false, epoch_anchor: hex::encode(coinbase.body.epoch_anchor),
        input_owner: None, input_sum_micronoid: "0".into(), output_sum_micronoid: reward_micronoid.to_string(),
        page_hashes: vec![cb_txid], inputs: vec![], outputs: cb_outputs });
    // --- development payout (position 1, optional) ---
    if stream.has_development_payout {
        let payout = &block.transactions[1];
        let mut outs = Vec::new(); let mut sum = 0u128;
        for (lane, o) in payout.body.live_outputs() {
            alloc_cursor += 1; sum += u128::from(o.amount);
            outs.push(OutputInfo { page: 0, lane: lane as u8, slot_index: u64::from(o.slot_index), amount_micronoid: o.amount,
                owner: o.owner.to_bech32(), creation_id: alloc_cursor });
        }
        let txid = hex::encode(logical_txids[1].0);
        transactions.push(TxInfo { position: 1, txid: txid.clone(), page_count: 1, live_inputs: 0, live_outputs: development_outputs,
            fee_micronoid: 0, coinbase: true, development_payout: true, epoch_anchor: hex::encode(payout.body.epoch_anchor),
            input_owner: None, input_sum_micronoid: "0".into(), output_sum_micronoid: sum.to_string(),
            page_hashes: vec![txid], inputs: vec![], outputs: outs });
    }
    // --- user PagedSpend groups ---
    for (index, group) in stream.groups.iter().enumerate() {
        let start = stream.user_body_start(usize::from(group.start_page));
        let end = start + usize::from(group.page_count);
        let pages = block.transactions.get(start..end).context("page range")?;
        let mut inputs = Vec::new(); let mut outputs = Vec::new(); let mut page_hashes = Vec::new();
        for (page_index, page) in pages.iter().enumerate() {
            page_hashes.push(hex::encode(page.txid().0));
            for (lane, i) in page.body.live_inputs() {
                inputs.push(InputInfo { page: page_index as u16, lane: lane as u8, slot_index: u64::from(i.slot_index),
                    amount_micronoid: i.amount, creation_id: i.creation_id });
            }
            for (lane, o) in page.body.live_outputs() {
                alloc_cursor += 1;
                outputs.push(OutputInfo { page: page_index as u16, lane: lane as u8, slot_index: u64::from(o.slot_index),
                    amount_micronoid: o.amount, owner: o.owner.to_bech32(), creation_id: alloc_cursor });
            }
        }
        transactions.push(TxInfo { position: stream.user_logical_index(index) as u16, txid: hex::encode(group.spend.logical_txid.0),
            page_count: group.page_count, live_inputs: group.spend.live_inputs, live_outputs: group.spend.live_outputs,
            fee_micronoid: group.spend.fee, coinbase: false, development_payout: false,
            epoch_anchor: hex::encode(group.spend.epoch_anchor), input_owner: Some(group.spend.input_owner.to_bech32()),
            input_sum_micronoid: group.spend.input_sum.to_string(), output_sum_micronoid: group.spend.output_sum.to_string(),
            page_hashes, inputs, outputs });
    }
    if alloc_cursor != header.alloc_counter { bail!("alloc_counter mismatch: {alloc_cursor} vs {}", header.alloc_counter); }
    let total_fees = stream.groups.iter().map(|g| u128::from(g.spend.fee)).sum::<u128>().to_string();
    let proof_class = match stream.proof_class {
        noid_chain::consensus::BlockProofClass::B25 => "B25 / m22",
        noid_chain::consensus::BlockProofClass::B255 => "B255 / m24",
    }.to_string();
    Ok(Decoded { proof_class, logical_transactions: logical_txids.len() as u16, user_pages: stream.page_count,
        live_inputs: stream.live_inputs,
        live_outputs: stream.live_outputs.saturating_add(coinbase_outputs).saturating_add(development_outputs),
        reward_micronoid, total_fees_micronoid: total_fees, block_bytes: bytes.len() as u64, transactions })
}

fn rpc(url: &str, method: &str, params: Value) -> Result<Value> {
    let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
    let v: Value = ureq::post(url).send_json(&body)?.body_mut().read_json()?;
    if let Some(e) = v.get("error") { bail!("rpc {method}: {e}"); }
    Ok(v["result"].clone())
}

fn main() -> Result<()> {
    let url = "http://127.0.0.1:9601";
    let n: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(42);
    let tip = rpc(url, "paranoid_blockCount", json!([]))?.as_u64().unwrap();
    if let Some(h) = std::env::args().nth(2).and_then(|s| s.parse::<u64>().ok()) {
        // Single-height mode: print the decoded structure as JSON (fixture generation).
        let raw = rpc(url, "paranoid_getBlock", json!([h]))?;
        let hdr = rpc(url, "paranoid_getBlockHeader", json!([h]))?;
        let dec = decode_block(&hex::decode(raw.as_str().context("getBlock null")?)?, h, hdr["hash"].as_str().unwrap())?;
        println!("{}", serde_json::to_string_pretty(&dec)?);
        return Ok(());
    }
    let (mut compared, mut equal, mut filled) = (0, 0, 0);
    for h in (tip - n + 1)..=tip {
        let raw = rpc(url, "paranoid_getBlock", json!([h]))?;
        let Some(raw) = raw.as_str() else { println!("{h}: getBlock=null"); continue };
        let details = rpc(url, "paranoid_getBlockDetails", json!([h]))?;
        let canon_hash = details["header"]["hash"].as_str().unwrap().to_string();
        let dec = decode_block(&hex::decode(raw)?, h, &canon_hash)?;
        let mine = serde_json::to_value(&dec.transactions)?;
        match &details["retained"] {
            Value::Null => { filled += 1; println!("{h}: retained=null -> decoded {} tx from getBlock (coinbase {} -> {} µNOID)", dec.transactions.len(), &dec.transactions[0].txid[..12], dec.reward_micronoid); }
            r => {
                compared += 1;
                let theirs = &r["transactions"];
                if &mine == theirs { equal += 1; } else {
                    println!("{h}: MISMATCH\n mine  ={}\n theirs={}", mine, theirs);
                }
                for f in ["proof_class","logical_transactions","user_pages","live_inputs","live_outputs","reward_micronoid","total_fees_micronoid","block_bytes"] {
                    let m = serde_json::to_value(&dec)?[f].clone();
                    if m != r[f] { println!("{h}: field {f} differs: mine={m} theirs={}", r[f]); }
                }
            }
        }
    }
    println!("tip={tip}: compared {compared} blocks with details -> {equal} identical; filled {filled} retained=null heights from getBlock");
    Ok(())
}
