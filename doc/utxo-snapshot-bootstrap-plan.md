# §3.2 UTXO-Snapshot Bootstrap - Implementation Plan

Status: planning, branch `UTXO-Snapshot-Bootstrap`. Companion to
[`doc-elektron/guideline-electrs-fork-integration.md`](https://github.com/kutlusoy/elektron-net/blob/main/doc-elektron/guideline-electrs-fork-integration.md)
§3.2 in the `elektron-net` repo, which covers the design rationale (why
`dumptxoutset`/the automatic snapshot files, not a new RPC). This file covers
the concrete code changes needed in *this* repo.

## Why this is more than "read a file and insert some rows"

Traced the actual data flow before writing this plan (`src/index.rs`,
`src/status.rs`, `src/db.rs`). The index does **not** store UTXO
amounts/scriptPubKeys anywhere today:

- `FUNDING_CF`/`SPENDING_CF`/`TXID_CF` (`src/types.rs`, written in
  `index_single_block()`) are height **pointers** only: "scripthash X was
  touched at height H", "outpoint O was spent at height H". No amounts, no
  scriptPubKeys.
- `ScriptHashStatus::sync()` calling `sync_confirmed()` (`src/status.rs`) turns
  those pointers into real `TxEntry` data by **re-fetching and re-parsing the
  actual historical block** from the daemon at query/subscribe time
  (`Daemon::for_blocks`/P2P). `Unspent::build()` then folds those `TxEntry`s
  into `(outpoint -> (value, height))`.

This means a snapshot bootstrap can't just seed `FUNDING_CF` with synthetic
pointers: resolving them still requires fetching the original transaction,
which is exactly the data pruning removed. The snapshot dump itself carries
amount and scriptPubKey directly per UTXO, so bootstrap needs its own
self-contained storage that `Unspent::build()` can consult **without**
re-fetching anything.

## Wire format (traced from the node source, not yet validated against a live fixture)

Confirmed by reading `elektron-net`'s `src/node/utxo_snapshot.h` and
`src/rpc/blockchain.cpp::WriteUTXOSnapshot()` directly:

```
SnapshotMetadata:
  magic bytes "utxo\xff" (5 bytes)
  version (u16)
  network magic (4 bytes, pchMessageStart)
  base_blockhash (32 bytes)
  coins_count (u64)

then, repeated, grouped by txid (leveldb iteration order guarantees grouping):
  txid (32 bytes)
  CompactSize: number of outputs for this txid
  repeated per output:
    CompactSize: vout index
    Coin:
      VARINT(code)   where code = (height << 1) | coinbase_flag
      compressed TxOut (CTxOutCompressor: compressed amount + compressed script)
```

Two sub-encodings need care because they are **not** the same as the
CompactSize format `rust-bitcoin` already supports:

- **VARINT** (used for the `code` field): Bitcoin Core's own prefix-free,
  MSB-first, base-128 varint (`serialize.h`'s `WriteVarInt`/`ReadVarInt`),
  distinct from `CompactSize`. Continuation is signaled by the high bit being
  *set* on every byte except the last one, and the encoding uses the "add 1
  per continuation group" trick, so it needs its own decoder, not the
  `bitcoin` crate's `VarInt`.
- **Compressed `TxOut`** (`compressor.h`/`compressor.cpp`): amounts are
  compressed with `CompressAmount`/`DecompressAmount` (base-10 trailing-zero
  stripping, arithmetic only, no elliptic-curve math), and scripts are
  compressed for the common templates (P2PKH, P2SH, P2PK compressed
  pubkey, P2PK uncompressed pubkey) with everything else (including
  P2WPKH/P2WSH/OP_RETURN/anything segwit) falling back to a raw
  length-prefixed script. The uncompressed-P2PK case needs secp256k1 point
  decompression to recover the full 65-byte public key from just the stored
  X-coordinate; the `secp256k1`/`bitcoin` crates already vendored here can do
  this, it just needs wiring up correctly.

**Important caveat:** there is no buildable node binary or existing
`dumptxoutset` output available in the environment this plan was written in,
so the decoder below has not been byte-verified against a real snapshot file.
Before this lands on `main`, it MUST be run against an actual `dumptxoutset`
dump from a real (even tiny regtest) Elektron Net node and cross-checked
entry-by-entry against `gettxoutsetinfo`/`listunspent` for the same UTXO set.
This is flagged explicitly in the status checklist below as a blocking step,
not an assumed pass.

## Plan

### 1. Snapshot parser (new `src/snapshot.rs`)

Implements the wire format above: a small local VARINT reader, `CompressAmount`/
`DecompressAmount`, script decompression for the four templated cases plus
the raw fallback, and a `SnapshotMetadata` + coin-entry iterator mirroring the
`HeaderRow`/`bitcoin::consensus::Decodable` pattern already used elsewhere in
this codebase. Sequential reads only, no lookup structures needed.

### 2. New column family: `SNAPSHOT_UNSPENT_CF`

Same conventions as `FUNDING_CF` et al. in `src/db.rs` (`cf_handle()`
accessor, `default_opts()`). Key: scripthash prefix plus outpoint (mirrors
`ScriptHashRow`'s prefix scheme so lookups stay prefix-scan, not full-scan).
Value: `(Amount, height)`, the piece that lets `Unspent::build()` resolve a
pre-bootstrap UTXO without touching the daemon at all.

Written **once**, in a new bootstrap routine, not touched by the normal
`index_blocks()`/`WriteBatch` path.

### 3. Bootstrap trigger (`src/index.rs`, `Index::load()`/`sync()`)

When `store.get_tip()` is `None` (fresh DB, same check already used to decide
whether to seed the genesis header, see `chain.rs`) **and** the daemon reports
`pruned: true` **and** chain height is greater than zero: run the bootstrap
instead of starting the normal indexing loop from height 0.

1. Obtain a snapshot: call `dumptxoutset`, or read the latest validated
   `datadir/snapshots/*.dat` plus `.hash` pair if the operator has mounted the
   node's datadir read-only into the electrs container (open deployment
   question below, affects docker-compose, not just this code).
2. Parse it (step 1), write all entries into `SNAPSHOT_UNSPENT_CF` (step 2)
   in one batch.
3. Persist the snapshot's base height as the index's bootstrap height, using
   the same `CONFIG_CF` mechanism already used for `get_tip()`, under a new
   key (e.g. `bootstrap_height`).
4. Fall through to the existing `sync()` loop starting from
   `bootstrap_height + 1`. No changes needed here: `Chain`/`daemon.get_new_headers`
   already work from an arbitrary starting tip.

### 4. Query-serving changes (`src/status.rs`)

- `Unspent::build()`: before folding in `confirmed`/`mempool` entries, also
  fold in this scripthash's `SNAPSHOT_UNSPENT_CF` entries (if any bootstrap
  happened). Spending them is already handled for free: a later block that
  spends a bootstrap-seeded outpoint gets indexed completely normally
  (`index_single_block`'s `visit_tx_in` doesn't care whether the prevout
  predates the index), producing a normal `SPENDING_CF` row. `Unspent::remove()`
  already drops any outpoint present in a confirmed spending entry,
  snapshot-sourced or not, so no new logic is needed there.
- `get_history()`: must **not** attempt to show a "funding" history entry for
  a snapshot-seeded UTXO, since there is no recoverable original funding
  transaction (that's the entire reason bootstrap exists). Per §3.6, history
  before the bootstrap height stays "unavailable by design", the same
  semantics as the existing pruned-merkle-proof error (code 3); this is a
  documentation/behavior point, not a new error path, since `get_history()`
  simply won't have entries to iterate for that period.
- `get_balance()`/`get_unspent()` (`listunspent`): DO reflect bootstrap-seeded
  UTXOs correctly, since `Unspent::build()` is the single resolution point
  for both.

### 5. Testing

- Unit tests for the sub-encodings with hand-computed expected byte
  sequences (VARINT round-trips, `CompressAmount`/`DecompressAmount`
  round-trips, P2PKH/P2SH template reconstruction). These can be verified
  without a live node.
- A real-fixture test against actual `dumptxoutset` output (see caveat
  above) is required before merging, not optional polish.
- Integration test already implied by the guideline's own acceptance
  criterion: index the same short chain two ways, (a) full genesis sync,
  (b) bootstrap from a snapshot taken partway through then sync the rest,
  and assert `listunspent`/`get_balance` are byte-identical between the two
  for every test scripthash. `get_history` will legitimately differ, (b) is
  missing pre-bootstrap entries, that's expected, not a test failure.

## Open deployment question (not just code)

`dumptxoutset` on a live pruned node vs. reading the automatically-written
`datadir/snapshots/*.dat`: the RPC call is simpler (no new volume mount, no
dependency on the node's own automatic-snapshot timing) but costs I/O/CPU on
the daemon at bootstrap time; reading the node's own file is free at bootstrap
time but requires sharing the node's datadir (read-only) into the
`elektron-net-electrs` container, a `docker-compose.yml`/`install-elektron-stack.sh`
change, not just this repo. Leaning towards `dumptxoutset` for the first cut
(self-contained, no cross-repo coordination) with the shared-file approach as
a possible later optimization once §3.2 also needs to run at scale for
independent node operators.

## Status

- [x] Snapshot parser (`src/snapshot.rs`): VARINT, amount compression, script
      compression, `SnapshotMetadata` and coin-entry reader.
- [x] `SNAPSHOT_UNSPENT_CF` schema (`SnapshotUnspentRow` in `src/types.rs`)
      and write path (`DBStore::write_snapshot_bootstrap()`,
      `get_bootstrap_height()` in `src/db.rs`).
- [x] `Daemon::dump_txoutset()` (`src/daemon.rs`), new `utxo_snapshot_dir`
      config option (`internal/config_specification.toml`, `src/config.rs`).
- [x] Bootstrap trigger: `Index::bootstrap()`, called from the top of
      `Index::sync()` on a fresh DB when `utxo_snapshot_dir` is set
      (`src/index.rs`). `index_blocks()` now skips the P2P body fetch for
      any height at or below the bootstrap height, recording only the
      header (already available from the body-independent `getheaders`
      walk, see `NewHeader::header()` in `src/chain.rs`) -- fixed a
      self-found bug here where an all-header chunk left the batch's tip
      marker at all-zeros instead of the chunk's actual last hash.
- [x] `Unspent::build()` integration (`src/status.rs`): folds in
      `Index::get_snapshot_unspent()` entries before the normal
      confirmed/mempool folding; `ScriptHashStatus::sync()` now also seeds
      these outpoints into its spending-detection set, so a later spend of
      a bootstrap-seeded UTXO is still detected through the ordinary
      `SPENDING_CF` path.
- [x] Sub-encoding unit tests written (VARINT, amount round-trip, P2PKH/P2SH
      reconstruction), hand-verified on paper; not yet run (see caveat below)
- [x] **Blocking (resolved):** built and run on a real machine (Docker,
      WSL2 Ubuntu regtest), not just this environment -- four real
      compiler/runtime issues surfaced and got fixed one at a time:
      `bitcoin::io::Read` vs `std::io::Read` for `consensus_decode()`;
      `dumptxoutset` needing an explicit `"latest"` type argument; a P2P
      network-magic mismatch (`signet_magic` unset, defaulted to stock
      testnet magic); and a `/snapshot` bind-mount permission issue in the
      daemon container's entrypoint. All fixed and pushed.
- [x] **Blocking (resolved):** validated against a live `dumptxoutset`
      fixture, not a synthetic one -- 800-block Elektron Net regtest,
      `fastprune` forcing real file rollover/pruning (`pruneheight: 649`
      confirmed), bootstrap wrote 800 coins at height 800. Cross-checked
      `blockchain.scripthash.listunspent`/`get_balance` against the node
      wallet's own `listunspent` for a real address: 500 UTXOs, summed sats
      match `get_balance.confirmed` exactly (33484375000), 349 of those 500
      entries sit at heights already pruned on the node. `get_history`
      correctly returns `[]` for the bootstrap period (no crash, no bogus
      entries).
- [x] Prune-gap resilience: what happens if electrs is offline across one or
      more *further* automatic-pruning checkpoints, so the daemon prunes
      block bodies between electrs' last-known tip and its new prune floor
      before electrs ever gets to index them? Found live (simulated: stopped
      electrs at tip 800, generated 1250 more blocks on the node, restarted
      electrs) that the old code would have tried `daemon.for_blocks()` for
      already-pruned heights; the daemon replies `notfound`, which
      `src/p2p.rs`'s message parser doesn't otherwise handle (`_ =>
      bail!("unsupported message...")`), killing the P2P connection and
      crashing electrs. Fixed: `Daemon::get_prune_height()` (new, reads
      `getblockchaininfo`'s `pruneheight`) plus `Index::ensure_no_prune_gap()`
      (new, called at the top of every `sync()` before indexing) detect a gap
      between the daemon's current prune floor and our last-covered height
      and re-run the §3.2 bootstrap to close it, capturing the UTXO set fresh
      as of now rather than ever attempting a doomed body fetch.
      `DBStore::write_snapshot_bootstrap()` now clears previously-written
      `SNAPSHOT_UNSPENT_CF` rows before writing the new ones, since a second
      bootstrap run must not leave stale entries for coins spent between the
      two snapshots. Without `utxo_snapshot_dir` configured, the same gap now
      fails with a clear, actionable error instead of the opaque P2P crash.
      Implemented, live-reverification of the exact simulated scenario above
      still pending (in progress).
- [ ] Genesis-sync vs. bootstrap-sync equivalence integration test
- [x] Decide `dumptxoutset` vs. shared snapshot file (deployment question
      above): went with `dumptxoutset` for the first cut, as planned.
