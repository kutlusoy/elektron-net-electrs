//! Parser for the AssumeUTXO / `dumptxoutset` snapshot format, used to
//! bootstrap the index without replaying the whole chain from genesis on a
//! pruned daemon (see `doc-elektron/guideline-electrs-fork-integration.md`
//! §3.2 in the `elektron-net` repo for the design rationale).
//!
//! Wire format traced directly from `elektron-net`'s
//! `src/node/utxo_snapshot.h` and `WriteUTXOSnapshot()` in
//! `src/rpc/blockchain.cpp`. See `doc/utxo-snapshot-bootstrap-plan.md` for
//! the full derivation. Not yet byte-verified against a real `dumptxoutset`
//! fixture; that verification is a blocking step before this is trusted for
//! anything beyond the hand-computed unit tests below.

use anyhow::{bail, Context, Result};
use bitcoin::blockdata::opcodes::all::OP_CHECKSIG;
use bitcoin::blockdata::script::Builder;
use bitcoin::consensus::Decodable;
use bitcoin::hashes::Hash;
use bitcoin::io::Read;
use bitcoin::{Amount, BlockHash, OutPoint, PubkeyHash, ScriptBuf, ScriptHash, Txid};

const SNAPSHOT_MAGIC: [u8; 5] = [b'u', b't', b'x', b'o', 0xff];
const SNAPSHOT_VERSION: u16 = 2;
const NUM_SPECIAL_SCRIPTS: u64 = 6;

/// Metadata header of a UTXO snapshot file.
pub(crate) struct SnapshotMetadata {
    pub(crate) base_blockhash: BlockHash,
    pub(crate) coins_count: u64,
}

impl SnapshotMetadata {
    /// Reads and validates the metadata header. `expected_network_magic`
    /// should be the daemon's own `pchMessageStart` (the same 4 bytes used
    /// for the P2P handshake, see `src/p2p.rs`) -- a snapshot produced for a
    /// different network must be rejected here, not silently accepted.
    pub(crate) fn read<R: Read>(r: &mut R, expected_network_magic: [u8; 4]) -> Result<Self> {
        let mut magic = [0u8; 5];
        r.read_exact(&mut magic).context("reading snapshot magic")?;
        if magic != SNAPSHOT_MAGIC {
            bail!("invalid UTXO snapshot magic bytes (not a dumptxoutset file?)");
        }

        let version = u16::consensus_decode(r).context("reading snapshot version")?;
        if version != SNAPSHOT_VERSION {
            bail!("unsupported snapshot version {version}, expected {SNAPSHOT_VERSION}");
        }

        let mut network_magic = [0u8; 4];
        r.read_exact(&mut network_magic)
            .context("reading snapshot network magic")?;
        if network_magic != expected_network_magic {
            bail!(
                "snapshot network magic {network_magic:02x?} does not match this daemon's {expected_network_magic:02x?}"
            );
        }

        let base_blockhash =
            BlockHash::consensus_decode(r).context("reading snapshot base blockhash")?;
        let coins_count = u64::consensus_decode(r).context("reading snapshot coins count")?;

        Ok(SnapshotMetadata {
            base_blockhash,
            coins_count,
        })
    }
}

/// A single unspent output from the snapshot, in already-decompressed form.
pub(crate) struct SnapshotCoin {
    pub(crate) outpoint: OutPoint,
    pub(crate) height: u32,
    #[allow(dead_code)] // not yet consumed by the bootstrap-trigger step
    pub(crate) is_coinbase: bool,
    pub(crate) amount: Amount,
    pub(crate) script_pubkey: ScriptBuf,
}

/// Reads all coin entries from a snapshot stream, after
/// [`SnapshotMetadata::read`] has already consumed the header.
///
/// On disk, entries are grouped by txid (see `WriteUTXOSnapshot()`): a
/// txid, a count, then that many `(vout, Coin)` pairs, relying on the
/// source database's key ordering to keep same-txid entries adjacent. This
/// flattens that grouping into a single sequential list.
pub(crate) fn read_coins<R: Read>(r: &mut R, coins_count: u64) -> Result<Vec<SnapshotCoin>> {
    let mut result = Vec::with_capacity(coins_count.min(1 << 20) as usize);
    while (result.len() as u64) < coins_count {
        let txid = Txid::consensus_decode(r).context("reading coin group txid")?;
        let group_size = read_compact_size(r).context("reading coin group size")?;
        for _ in 0..group_size {
            let vout = read_compact_size(r).context("reading coin vout index")?;
            let vout = u32::try_from(vout).context("vout index out of range")?;
            let outpoint = OutPoint::new(txid, vout);
            let (height, is_coinbase) = read_height_and_coinbase(r)?;
            let (amount, script_pubkey) = read_compressed_txout(r)?;
            result.push(SnapshotCoin {
                outpoint,
                height,
                is_coinbase,
                amount,
                script_pubkey,
            });
        }
    }
    Ok(result)
}

/// Bitcoin Core's `CompactSize` (`ReadCompactSize`): 1, 3, 5 or 9 bytes
/// depending on magnitude (`0xfd`/`0xfe`/`0xff` prefixes for 16/32/64-bit
/// values, anything below `0xfd` is a single byte as-is). Used here for the
/// per-txid coin-group count and the vout index. Distinct from the VARINT
/// below -- getting the two confused silently misparses every entry after
/// the first mistake.
fn read_compact_size<R: Read>(r: &mut R) -> Result<u64> {
    let mut first = [0u8; 1];
    r.read_exact(&mut first).context("reading compact size")?;
    Ok(match first[0] {
        0xfd => {
            let mut buf = [0u8; 2];
            r.read_exact(&mut buf)?;
            u16::from_le_bytes(buf) as u64
        }
        0xfe => {
            let mut buf = [0u8; 4];
            r.read_exact(&mut buf)?;
            u32::from_le_bytes(buf) as u64
        }
        0xff => {
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf)?;
            u64::from_le_bytes(buf)
        }
        n => n as u64,
    })
}

/// Bitcoin Core's own prefix-free, MSB-first, base-128 `VARINT`
/// (`serialize.h`'s `WriteVarInt`/`ReadVarInt`) -- NOT the same encoding as
/// `CompactSize` above. Used for `Coin`'s `(height << 1) | coinbase` code
/// and for the compressed script's size field.
fn read_varint<R: Read>(r: &mut R) -> Result<u64> {
    let mut n: u64 = 0;
    loop {
        let mut buf = [0u8; 1];
        r.read_exact(&mut buf).context("reading varint byte")?;
        let byte = buf[0];
        n = n
            .checked_shl(7)
            .context("varint overflow")?
            .checked_add((byte & 0x7F) as u64)
            .context("varint overflow")?;
        if byte & 0x80 != 0 {
            n = n.checked_add(1).context("varint overflow")?;
        } else {
            return Ok(n);
        }
    }
}

fn read_height_and_coinbase<R: Read>(r: &mut R) -> Result<(u32, bool)> {
    let code = read_varint(r).context("reading coin height/coinbase code")?;
    let is_coinbase = code & 1 != 0;
    let height = u32::try_from(code >> 1).context("coin height out of range")?;
    Ok((height, is_coinbase))
}

/// Bitcoin Core's `CTxOutCompressor::DecompressAmount`: reverses the
/// base-10 trailing-zero stripping used to shrink common round amounts.
/// Arithmetic only, no elliptic-curve math involved -- safe to hand-verify
/// against known values (see tests below).
fn decompress_amount(x: u64) -> u64 {
    if x == 0 {
        return 0;
    }
    let mut x = x - 1;
    let e = x % 10;
    x /= 10;
    let mut n;
    if e < 9 {
        let d = (x % 9) + 1;
        x /= 9;
        n = x * 10 + d;
    } else {
        n = x + 1;
    }
    for _ in 0..e {
        n *= 10;
    }
    n
}

/// Reads a compressed `Coin` value: a VARINT-encoded compressed amount,
/// followed by a compressed script.
fn read_compressed_txout<R: Read>(r: &mut R) -> Result<(Amount, ScriptBuf)> {
    let compressed_amount = read_varint(r).context("reading compressed amount")?;
    let amount = Amount::from_sat(decompress_amount(compressed_amount));
    let script_pubkey = read_compressed_script(r)?;
    Ok((amount, script_pubkey))
}

/// Bitcoin Core's `CScriptCompressor` decompression. Four templated cases
/// (P2PKH, P2SH, P2PK compressed, P2PK uncompressed) reconstruct the
/// standard script from a short hash/pubkey; anything else (including
/// P2WPKH/P2WSH/OP_RETURN/taproot) falls back to a raw, length-prefixed
/// script, exactly as it was originally.
fn read_compressed_script<R: Read>(r: &mut R) -> Result<ScriptBuf> {
    let size = read_varint(r).context("reading compressed script size")?;
    if size < NUM_SPECIAL_SCRIPTS {
        return read_special_script(r, size);
    }
    let raw_len = size - NUM_SPECIAL_SCRIPTS;
    let mut raw = vec![0u8; usize::try_from(raw_len).context("script length out of range")?];
    r.read_exact(&mut raw).context("reading raw scriptPubkey")?;
    Ok(ScriptBuf::from_bytes(raw))
}

fn read_special_script<R: Read>(r: &mut R, kind: u64) -> Result<ScriptBuf> {
    match kind {
        0 => {
            // P2PKH: OP_DUP OP_HASH160 <20 bytes> OP_EQUALVERIFY OP_CHECKSIG
            let mut hash = [0u8; 20];
            r.read_exact(&mut hash).context("reading P2PKH hash")?;
            let pkh = PubkeyHash::from_byte_array(hash);
            Ok(ScriptBuf::new_p2pkh(&pkh))
        }
        1 => {
            // P2SH: OP_HASH160 <20 bytes> OP_EQUAL
            let mut hash = [0u8; 20];
            r.read_exact(&mut hash).context("reading P2SH hash")?;
            let sh = ScriptHash::from_byte_array(hash);
            Ok(ScriptBuf::new_p2sh(&sh))
        }
        2 | 3 => {
            // P2PK, pubkey stored compressed on disk already (kind encodes
            // the 0x02/0x03 parity prefix byte).
            let mut x = [0u8; 32];
            r.read_exact(&mut x)
                .context("reading compressed pubkey x-coordinate")?;
            let mut compressed = [0u8; 33];
            compressed[0] = if kind == 2 { 0x02 } else { 0x03 };
            compressed[1..].copy_from_slice(&x);
            let pubkey = bitcoin::secp256k1::PublicKey::from_slice(&compressed)
                .context("invalid compressed pubkey in snapshot")?;
            Ok(Builder::new()
                .push_slice(pubkey.serialize())
                .push_opcode(OP_CHECKSIG)
                .into_script())
        }
        4 | 5 => {
            // P2PK, pubkey stored uncompressed originally. The snapshot
            // only carries the x-coordinate plus parity (same as the
            // compressed case) -- reconstructing the full 65-byte
            // uncompressed key needs elliptic-curve point decompression,
            // done here via secp256k1 rather than hand-rolled curve math.
            let mut x = [0u8; 32];
            r.read_exact(&mut x)
                .context("reading uncompressed-origin pubkey x-coordinate")?;
            let mut compressed = [0u8; 33];
            compressed[0] = if kind == 4 { 0x02 } else { 0x03 };
            compressed[1..].copy_from_slice(&x);
            let pubkey = bitcoin::secp256k1::PublicKey::from_slice(&compressed)
                .context("invalid uncompressed-origin pubkey in snapshot")?;
            Ok(Builder::new()
                .push_slice(pubkey.serialize_uncompressed())
                .push_opcode(OP_CHECKSIG)
                .into_script())
        }
        other => bail!("unknown special script kind in snapshot: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-computed against Bitcoin Core's ReadVarInt/WriteVarInt algorithm
    // (serialize.h): single-byte values have no continuation bit; crossing
    // a 7-bit boundary sets the continuation bit on every byte but the last
    // and applies the "+1 per continuation group" trick.
    #[test]
    fn varint_single_byte() {
        assert_eq!(read_varint(&mut &[0x00][..]).unwrap(), 0);
        assert_eq!(read_varint(&mut &[0x7f][..]).unwrap(), 0x7f);
    }

    #[test]
    fn varint_two_bytes() {
        // 128 = 0x80 -> encoded as [0x80, 0x00] (continuation + low group)
        assert_eq!(read_varint(&mut &[0x80, 0x00][..]).unwrap(), 128);
        // 16511 = 0x407F -> a known worked example from Bitcoin Core's own
        // varint test vectors (largest 2-byte-encodable value).
        assert_eq!(read_varint(&mut &[0xff, 0x7f][..]).unwrap(), 16511);
    }

    // Hand-computed against CTxOutCompressor::DecompressAmount.
    #[test]
    fn amount_decompression_known_values() {
        assert_eq!(decompress_amount(0), 0);
        // 1 satoshi compresses to 1 (e=0 branch, d=1, x=0 -> n=1).
        assert_eq!(decompress_amount(1), 1);
        // 1 BTC = 100_000_000 sats is a "nice round" value that compresses
        // small; decompressing must reproduce it exactly.
        let compressed_one_btc = compress_amount_for_test(100_000_000);
        assert_eq!(decompress_amount(compressed_one_btc), 100_000_000);
    }

    #[test]
    fn amount_decompression_roundtrip_arbitrary_values() {
        for n in [0u64, 1, 42, 1_000, 123_456_789, 21_000_000 * 100_000_000] {
            let c = compress_amount_for_test(n);
            assert_eq!(decompress_amount(c), n, "roundtrip failed for {n}");
        }
    }

    // Mirrors CTxOutCompressor::CompressAmount so the roundtrip tests above
    // don't depend on decompress_amount to grade itself.
    fn compress_amount_for_test(mut n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut e = 0u64;
        while n % 10 == 0 && e < 9 {
            n /= 10;
            e += 1;
        }
        if e < 9 {
            let d = n % 10;
            n /= 10;
            1 + (n * 9 + d - 1) * 10 + e
        } else {
            1 + (n - 1) * 10 + 9
        }
    }

    #[test]
    fn p2pkh_script_reconstruction() {
        let hash = [0x11u8; 20];
        let mut input = Vec::new();
        input.extend_from_slice(&hash);
        let script = read_special_script(&mut &input[..], 0).unwrap();
        assert!(script.is_p2pkh());
    }

    #[test]
    fn p2sh_script_reconstruction() {
        let hash = [0x22u8; 20];
        let mut input = Vec::new();
        input.extend_from_slice(&hash);
        let script = read_special_script(&mut &input[..], 1).unwrap();
        assert!(script.is_p2sh());
    }
}
