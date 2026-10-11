//! The raw block archive (`archive_raw_blocks`): every block's bytes
//! exactly as the node served them (`paranoid_getBlock`), kept next to
//! the decoded tables.
//!
//! The tables hold what this permanode read out of a block; the bytes are
//! the block itself. With them a block can be read again - by a later
//! version that understands more of it, after a decoder bug, or to hand
//! the original to someone else - long after the node pruned the body
//! (~42 blocks). Bytes are only kept once they are proven to be the block
//! on record: they must decode to its height and hash, rebuild its
//! `tx_root`, and be its canonical encoding (`decode::decode_bound_block`).
//! That makes them safe to take from another permanode, too.
//!
//! Stored zlib-compressed (about a third of the raw size on today's
//! blocks), one row per block in `raw_blocks`; the proof is not part of
//! the bytes (`getBlock` does not serve it).

use crate::decode;
use anyhow::{bail, Context, Result};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use permanode_core::db;
use rusqlite::Connection;
use std::io::{Read, Write};

/// How the bytes are compressed in `raw_blocks.codec`.
pub const CODEC: &str = "zlib";

/// The largest block the protocol allows, in bytes: nothing longer is
/// ever decompressed or accepted from a peer.
pub fn max_block_bytes() -> usize {
    noid_chain::canonical_block_wire_len(noid_chain::BLOCK_MAX_TXS).unwrap_or(16 * 1024 * 1024)
}

/// Checks that `bytes` are block `(height, hash)` (see the module docs).
pub fn verify(bytes: &[u8], height: u64, hash: &str) -> Result<()> {
    if bytes.len() > max_block_bytes() {
        bail!("{} bytes, more than a block can have", bytes.len());
    }
    decode::decode_bound_block(bytes, height, hash).map(|_| ())
}

pub fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::with_capacity(bytes.len() / 2), Compression::default());
    enc.write_all(bytes)?;
    Ok(enc.finish()?)
}

pub fn decompress(codec: &str, data: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    if codec != CODEC {
        bail!("unknown codec {codec}");
    }
    if raw_len > max_block_bytes() {
        bail!("recorded length {raw_len} is more than a block can have");
    }
    let mut out = Vec::with_capacity(raw_len);
    ZlibDecoder::new(data).take(raw_len as u64 + 1).read_to_end(&mut out).context("decompress")?;
    if out.len() != raw_len {
        bail!("decompressed to {} bytes, recorded {raw_len}", out.len());
    }
    Ok(out)
}

/// Verifies `bytes` against block `(height, hash)` on record and keeps
/// them. `source`: `node` (getBlock) or `peer` / `import` (another
/// permanode). `Ok(false)` if the block is not on record (it was reorged
/// away meanwhile) or its bytes already are; an error if they are not
/// the block.
pub fn store(conn: &Connection, height: u64, hash: &str, bytes: &[u8], source: &str) -> Result<bool> {
    let Some(block_id) = db::block_id(conn, height, hash)? else {
        return Ok(false);
    };
    if db::has_raw_block(conn, block_id)? {
        return Ok(false);
    }
    verify(bytes, height, hash)?;
    let packed = compress(bytes)?;
    db::insert_raw_block(conn, block_id, CODEC, bytes.len(), &packed, source, &chrono::Utc::now().to_rfc3339())?;
    Ok(true)
}

/// The kept bytes of block `(height, hash)`, checked again on the way out.
pub fn load(conn: &Connection, height: u64, hash: &str) -> Result<Option<Vec<u8>>> {
    let Some((codec, raw_len, data)) = db::raw_block(conn, height, hash)? else {
        return Ok(None);
    };
    let bytes = decompress(&codec, &data, raw_len)?;
    verify(&bytes, height, hash).context("kept bytes no longer match the block on record")?;
    Ok(Some(bytes))
}
