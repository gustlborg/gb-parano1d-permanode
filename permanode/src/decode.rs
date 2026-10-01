//! Decoder for `paranoid_getBlock` bytes. Two uses:
//!
//! - **Fallback** when `paranoid_getBlockDetails` reports `retained: null`
//!   for a block still inside the node's serving window. Node v1.1.x hid
//!   every "marker" block of a multi-block commit that way (~2.2% of all
//!   heights, analysis in `docs/node-rpc-marker-bug/REPORT.md`); v2.0.0
//!   fixed it, the fallback stays for older nodes and short-lived gaps.
//! - **Contract flags** of v2 blocks, which getBlockDetails does not report
//!   (`contract_flags`).
//!
//! Fields are derived exactly the way `noid_rpc::server::get_block_details`
//! does (mirrored from v2.0.0), by linking the node's own crates rather
//! than reimplementing txid/bech32m/paged-spend logic - verified
//! byte-identical against live blocks with existing `retained` data, see
//! `permanode/tests/decode_fixtures.rs`. The one exception is the proof
//! class of v2 blocks: it lives only in the proof, never in the body.

use crate::rpc::{
    BlockTransactionInfo, BlockTransactionInputInfo, BlockTransactionOutputInfo, RetainedBlockInfo,
};
use anyhow::{anyhow, bail, Context, Result};

/// Decodes raw `paranoid_getBlock` bytes into the same shape
/// `paranoid_getBlockDetails`'s `retained` field would have held, had the
/// node not hidden this block as a marker. `expected_height`/
/// `expected_hash_hex` must come from a trusted canonical source
/// (`getBlockHeader`/`getBlockDetails.header`, same RPC round as the raw
/// bytes) - `getBlock` itself does not check that the body it serves is
/// still canonical, so the caller must bind it before trusting anything in
/// it (mismatch during a reorg between two separate RPC calls is exactly
/// the failure mode this guards against).
pub fn decode_retained_block(
    bytes: &[u8],
    expected_height: u64,
    expected_hash_hex: &str,
) -> Result<RetainedBlockInfo> {
    if expected_height == 0 {
        bail!("genesis block has no coinbase/transactions, not a decode target");
    }

    let block = decode_bound_block(bytes, expected_height, expected_hash_hex)?;
    let header = &block.header;

    let stream = noid_chain::validate_block_page_stream(&block.transactions)
        .map_err(|e| anyhow!("page stream: {e}"))?;
    let logical_txids = noid_chain::try_compute_logical_txids(&block.transactions)
        .map_err(|e| anyhow!("txids: {e}"))?;
    let coinbase = block.transactions.first().context("missing coinbase")?;
    let reward_micronoid = coinbase
        .body
        .live_outputs()
        .map(|(_, o)| o.amount)
        .fold(0u64, u64::saturating_add);
    let coinbase_outputs = u16::try_from(coinbase.body.live_outputs().count())?;
    let development_outputs = if stream.has_development_payout {
        let payout = block.transactions.get(1).context("development payout flagged but block has no second page")?;
        u16::try_from(payout.body.live_outputs().count())?
    } else {
        0
    };
    let minted_outputs =
        u64::from(stream.live_outputs) + u64::from(coinbase_outputs) + u64::from(development_outputs);
    let mut alloc_cursor = header
        .alloc_counter
        .checked_sub(minted_outputs)
        .context("alloc underflow")?;

    let mut transactions = Vec::new();

    // --- coinbase (position 0) ---
    let mut cb_outputs = Vec::new();
    for (lane, o) in coinbase.body.live_outputs() {
        alloc_cursor += 1;
        cb_outputs.push(BlockTransactionOutputInfo {
            page: 0,
            lane: u32::from(lane as u8),
            slot_index: u64::from(o.slot_index),
            amount_micronoid: o.amount,
            owner: o.owner.to_bech32(),
            creation_id: noid_chain::consensus::params::coinbase_creation_id(header.height),
        });
    }
    let cb_txid = hex::encode(logical_txids.first().context("no logical txids")?.0);
    transactions.push(BlockTransactionInfo {
        position: 0,
        txid: cb_txid.clone(),
        page_count: 1,
        live_inputs: 0,
        live_outputs: u32::from(coinbase_outputs),
        fee_micronoid: 0,
        coinbase: true,
        development_payout: false,
        epoch_anchor: hex::encode(coinbase.body.epoch_anchor),
        input_owner: None,
        input_sum_micronoid: "0".into(),
        output_sum_micronoid: reward_micronoid.to_string(),
        page_hashes: vec![cb_txid],
        inputs: vec![],
        outputs: cb_outputs,
    });

    // --- development payout (position 1, optional) ---
    if stream.has_development_payout {
        let payout = block.transactions.get(1).context("development payout page missing")?;
        let mut outs = Vec::new();
        let mut sum = 0u128;
        for (lane, o) in payout.body.live_outputs() {
            alloc_cursor += 1;
            sum += u128::from(o.amount);
            outs.push(BlockTransactionOutputInfo {
                page: 0,
                lane: u32::from(lane as u8),
                slot_index: u64::from(o.slot_index),
                amount_micronoid: o.amount,
                owner: o.owner.to_bech32(),
                creation_id: alloc_cursor,
            });
        }
        let txid = hex::encode(logical_txids.get(1).context("development payout txid missing")?.0);
        transactions.push(BlockTransactionInfo {
            position: 1,
            txid: txid.clone(),
            page_count: 1,
            live_inputs: 0,
            live_outputs: u32::from(development_outputs),
            fee_micronoid: 0,
            coinbase: true,
            development_payout: true,
            epoch_anchor: hex::encode(payout.body.epoch_anchor),
            input_owner: None,
            input_sum_micronoid: "0".into(),
            output_sum_micronoid: sum.to_string(),
            page_hashes: vec![txid],
            inputs: vec![],
            outputs: outs,
        });
    }

    // --- user PagedSpend groups ---
    for (index, group) in stream.groups.iter().enumerate() {
        let start = stream.user_body_start(usize::from(group.start_page));
        let end = start + usize::from(group.page_count);
        let pages = block.transactions.get(start..end).context("page range")?;
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        let mut page_hashes = Vec::new();
        for (page_index, page) in pages.iter().enumerate() {
            page_hashes.push(hex::encode(page.txid().0));
            for (lane, i) in page.body.live_inputs() {
                inputs.push(BlockTransactionInputInfo {
                    page: page_index as u32,
                    lane: u32::from(lane as u8),
                    slot_index: u64::from(i.slot_index),
                    amount_micronoid: i.amount,
                    creation_id: i.creation_id,
                });
            }
            for (lane, o) in page.body.live_outputs() {
                alloc_cursor += 1;
                outputs.push(BlockTransactionOutputInfo {
                    page: page_index as u32,
                    lane: u32::from(lane as u8),
                    slot_index: u64::from(o.slot_index),
                    amount_micronoid: o.amount,
                    owner: o.owner.to_bech32(),
                    creation_id: alloc_cursor,
                });
            }
        }
        transactions.push(BlockTransactionInfo {
            position: stream.user_logical_index(index) as u32,
            txid: hex::encode(group.spend.logical_txid.0),
            page_count: u32::from(group.page_count),
            live_inputs: u32::from(group.spend.live_inputs),
            live_outputs: u32::from(group.spend.live_outputs),
            fee_micronoid: group.spend.fee,
            coinbase: false,
            development_payout: false,
            epoch_anchor: hex::encode(group.spend.epoch_anchor),
            input_owner: Some(group.spend.input_owner.to_bech32()),
            input_sum_micronoid: group.spend.input_sum.to_string(),
            output_sum_micronoid: group.spend.output_sum.to_string(),
            page_hashes,
            inputs,
            outputs,
        });
    }

    if alloc_cursor != header.alloc_counter {
        bail!("alloc_counter mismatch: {alloc_cursor} vs {}", header.alloc_counter);
    }

    let total_fees_micronoid = stream
        .groups
        .iter()
        .map(|g| u128::from(g.spend.fee))
        .sum::<u128>()
        .to_string();
    let proof_class = if noid_chain::consensus::params::v2_active(header.height) {
        // The v2 class (Small/Large) is chosen by the producer and only
        // recorded in the proof, which getBlock does not carry. The node
        // itself reports exactly this string once the proof is gone.
        V2_CLASS_UNAVAILABLE
    } else {
        match stream.proof_class {
            noid_chain::consensus::BlockProofClass::B25 => "B25 / m22",
            noid_chain::consensus::BlockProofClass::B255 => "B255 / m24",
        }
    }
    .to_string();

    Ok(RetainedBlockInfo {
        proof_class,
        logical_transactions: logical_txids.len() as u32,
        user_pages: u32::from(stream.page_count),
        live_inputs: u32::from(stream.live_inputs),
        live_outputs: u32::from(stream.live_outputs)
            .saturating_add(u32::from(coinbase_outputs))
            .saturating_add(u32::from(development_outputs)),
        reward_micronoid,
        total_fees_micronoid,
        block_bytes: bytes.len() as u64,
        transactions,
    })
}

/// What the node reports as the proof class of a v2 block whose proof it
/// no longer holds (and what this decoder reports for every v2 block).
pub const V2_CLASS_UNAVAILABLE: &str = "v2 / class unavailable";

/// Contract flags of one logical transaction, as stored in
/// `transactions.contract_flags`: a v2 contract call, and additionally
/// whether that call closes the contract (no successor output).
pub const CONTRACT_CALL: u8 = 1;
pub const CONTRACT_CLOSE: u8 = 2;

/// Decodes `paranoid_getBlock` bytes and binds them to the canonical
/// height and hash from a trusted source (see `decode_retained_block`).
///
/// The hash covers the header only, so the pages are bound to it through
/// the header's `tx_root`: the logical transactions they form must rebuild
/// it. And the bytes must be the block's one canonical encoding - nothing
/// appended, nothing encoded differently - so bytes that pass are the
/// block itself, whoever handed them over (`raw.rs` keeps them).
pub(crate) fn decode_bound_block(bytes: &[u8], expected_height: u64, expected_hash_hex: &str) -> Result<noid_chain::Block> {
    let block = noid_chain::Block::from_bytes(bytes).map_err(|e| anyhow!("decode block: {e:?}"))?;
    if block.header.height != expected_height {
        bail!("height mismatch: body claims {}, expected {expected_height}", block.header.height);
    }
    let hash = hex::encode(noid_chain::block_header::block_id(&block.header));
    if hash != expected_hash_hex {
        bail!("body hash {hash} != canonical {expected_hash_hex}");
    }
    if !block.transactions.is_empty() {
        let root = noid_chain::try_compute_tx_root(&block.transactions).map_err(|e| anyhow!("tx_root: {e:?}"))?;
        if root != block.header.tx_root {
            bail!("the pages rebuild tx_root {}, the header says {}", hex::encode(root), hex::encode(block.header.tx_root));
        }
    }
    if block.to_bytes() != bytes {
        bail!("not the canonical encoding of the block");
    }
    Ok(block)
}

/// `(position, flags)` of every contract call in a block, from the raw
/// `paranoid_getBlock` bytes - getBlockDetails does not report them. A call
/// is a one-page spend whose validity bitmap carries the contract bit; the
/// terminal bit marks a call that closes the contract.
pub fn contract_flags(bytes: &[u8], expected_height: u64, expected_hash_hex: &str) -> Result<Vec<(u32, u8)>> {
    let block = decode_bound_block(bytes, expected_height, expected_hash_hex)?;
    contract_flags_of(&block.transactions)
}

pub fn contract_flags_of(transactions: &[noid_tx::Transaction]) -> Result<Vec<(u32, u8)>> {
    let stream = noid_chain::validate_block_page_stream(transactions).map_err(|e| anyhow!("page stream: {e}"))?;
    let mut flags = Vec::new();
    for (index, group) in stream.groups.iter().enumerate() {
        let first = transactions
            .get(stream.user_body_start(usize::from(group.start_page)))
            .context("page range")?;
        let bitmap = first.body.validity_bitmap;
        let mut f = 0u8;
        if bitmap & noid_tx::PAGED_SPEND_CONTRACT_BIT != 0 {
            f |= CONTRACT_CALL;
            if bitmap & noid_tx::PAGED_SPEND_TERMINAL_BIT != 0 {
                f |= CONTRACT_CLOSE;
            }
        }
        if f != 0 {
            flags.push((stream.user_logical_index(index) as u32, f));
        }
    }
    Ok(flags)
}
