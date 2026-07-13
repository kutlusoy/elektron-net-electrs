# §3.2 UTXO-Snapshot Bootstrap — Implementation Plan

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
  `index_single_block()`) are height **pointers** only — "scripthash X was
  touched at height H", "outpoint O was spent at height H". No amounts, no
  scriptPubKeys.
- `ScriptHashStatus::sync()` → `sync_confirmed()` (`src/status.rs`) turns
  those pointers into real `TxEntry` data by **re-fetching and re-parsing the
  actual historical block** from the daemon at query/subscribe time
  (`Daemon::for_blocks`/P2P). `Unspent::build()` then folds those `TxEntry`s
  into `(outpoint → (value, height))`.

This means a snapshot bootstrap can't just seed `FUNDING_CF` with synthetic
pointers — resolving them still requires fetching the original transaction,
which is exactly the data pruning removed. The snapshot dump itself carries
amount + scriptPubKey directly per UTXO, so bootstrap needs its own
self-contained storage that `Unspent::build()` can consult **without**
re-fetching anything.

## Plan

### 1. Snapshot parser (new `src/snapshot.rs`)

Format is already fully specified in the node's own
`src/node/utxo_snapshot.h`/`.cpp` (this repo's sibling `elektron-net`): message
start (network magic) + `SnapshotMetadata` (base blockhash, `u64` coins count)
+ that many sequential coin entries (outpoint, height+coinbase flag, `TxOut`
in Core's compressed coin serialization). Sequential reads only, no
lookup structures — straightforward `Decodable` impl mirroring the existing
`HeaderRow`/`bitcoin::consensus::Decodable` usage pattern already in this
codebase.

### 2. New column family: `SNAPSHOT_UNSPENT_CF`

Same conventions as `FUNDING_CF` et al. in `src/db.rs` (`cf_handle()`
accessor, `default_opts()`). Key: scripthash prefix + outpoint (mirrors
`ScriptHashRow`'s prefix scheme so lookups stay prefix-scan, not full-scan).
Value: `(Amount, height)` — this is the piece that lets `Unspent::build()`
resolve a pre-bootstrap UTXO without touching the daemon at all.

Written **once**, in a new bootstrap routine, not touched by the normal
`index_blocks()`/`WriteBatch` path.

### 3. Bootstrap trigger (`src/index.rs`, `Index::load()`/`sync()`)

When `store.get_tip()` is `None` (fresh DB — same check already used to
decide whether to seed the genesis header, see `chain.rs`) **and** the daemon
reports `pruned: true` **and** chain height > 0: run the bootstrap instead of
starting the normal indexing loop from height 0.

1. Obtain a snapshot: call `dumptxoutset`, or read the latest validated
   `datadir/snapshots/*.dat` + `.hash` pair if the operator has mounted the
   node's datadir read-only into the electrs container (open deployment
   question, see below — affects docker-compose, not just this code).
2. Parse it (step 1), write all entries into `SNAPSHOT_UNSPENT_CF` (step 2)
   in one batch.
3. Persist the snapshot's base height as the index's bootstrap height —
   same `CONFIG_CF` mechanism already used for `get_tip()`, new key
   (e.g. `bootstrap_height`).
4. Fall through to the existing `sync()` loop starting from
   `bootstrap_height + 1`. No changes needed here — `Chain`/`daemon.get_new_headers`
   already work from an arbitrary starting tip.

### 4. Query-serving changes (`src/status.rs`)

- `Unspent::build()`: before folding in `confirmed`/`mempool` entries, also
  fold in this scripthash's `SNAPSHOT_UNSPENT_CF` entries (if any bootstrap
  happened). Spending them is already handled for free: a later block that
  spends a bootstrap-seeded outpoint gets indexed completely normally
  (`index_single_block`'s `visit_tx_in` doesn't care whether the prevout
  predates the index), producing a normal `SPENDING_CF` row — `Unspent::remove()`
  already drops any outpoint present in a confirmed spending entry, snapshot-sourced
  or not, no new logic needed there.
- `get_history()`: must **not** attempt to show a "funding" history entry for
  a snapshot-seeded UTXO — there is no recoverable original funding
  transaction (that's the entire reason bootstrap exists). Per §3.6, history
  before the bootstrap height stays "unavailable by design", same semantics
  as the existing pruned-merkle-proof error (code 3) — this is a
  documentation/behavior point, not a new error path, since `get_history()`
  simply won't have entries to iterate for that period.
- `get_balance()`/`get_unspent()` (`listunspent`): DO reflect bootstrap-seeded
  UTXOs correctly, since `Unspent::build()` is the single resolution point
  for both.

### 5. Testing

- Unit test the parser against a small `dumptxoutset` output from a regtest
  chain (few blocks, few UTXOs, hand-verifiable).
- Integration test already implied by the guideline's own acceptance
  criterion: index the same short chain two ways — (a) full genesis sync,
  (b) bootstrap from a snapshot taken partway through, then sync the rest —
  and assert `listunspent`/`get_balance` are byte-identical between the two
  for every test scripthash. `get_history` will legitimately differ (b) is
  missing pre-bootstrap entries — that's expected, not a test failure.

## Open deployment question (not just code)

`dumptxoutset` on a live pruned node vs. reading the automatically-written
`datadir/snapshots/*.dat`: the RPC call is simpler (no new volume mount, no
dependency on the node's own automatic-snapshot timing) but costs I/O/CPU on
the daemon at bootstrap time; reading the node's own file is free at bootstrap
time but requires sharing the node's datadir (read-only) into the
`elektron-net-electrs` container — a `docker-compose.yml`/`install-elektron-stack.sh`
change, not just this repo. Leaning towards `dumptxoutset` for the first cut
(self-contained, no cross-repo coordination) with the shared-file approach as
a possible later optimization once §3.2 also needs to run at scale for
independent node operators.

## Status

- [ ] Snapshot parser (`src/snapshot.rs`)
- [ ] `SNAPSHOT_UNSPENT_CF` schema + write path
- [ ] Bootstrap trigger in `Index::load()`
- [ ] `Unspent::build()` integration
- [ ] Parser unit test
- [ ] Genesis-sync vs. bootstrap-sync equivalence integration test
- [ ] Decide `dumptxoutset` vs. shared snapshot file (deployment question above)
