# Bug: `blockchain.scripthash.get_balance` crashes electrs ("receiving on an empty and disconnected channel")

**Status: unfixed, upstream (`romanz/electrs`) issue, not specific to this fork.**
Tracked here so it isn't rediscovered from scratch next time it bites us.

## Symptom

Querying the balance/history of one specific address through the Electrum
RPC path reliably crashes the whole `electrs` process instead of returning
an error for just that request:

```
electrs::electrum] your wallet uses less efficient method of querying electrs,
  consider contacting the developer of your wallet. Reason: blockchain.scripthash.get_balance
  called for unsubscribed scripthash
electrs::electrum] RPC blockchain.scripthash.get_balance failed: failed to get block
  000000000000a42d3a9d0156b86605fbe568fc1aba1f408104e82e0a52da0371:
  receiving on an empty and disconnected channel
electrs::db] closing DB at /data/bitcoin
Error: electrs failed

Caused by:
    0: sync failed
    1: sending on a disconnected channel
```

The process then exits (`electrs exited with code 1`) and is brought back
by Docker's `restart: unless-stopped` a second or two later — only to crash
again immediately if the same address is queried again. Symptom reaches the
mempool explorer frontend as a generic `500: Failed to get address`
(`elektron-net-mempool/backend/src/api/bitcoin/bitcoin.routes.ts`,
`getAddress()` swallows the underlying error into a fixed 500 with no
server-side log of *why*).

Known affected address on our chain: `be1qyjgsxxamz745k5zev7357a7zrle449n6shjhkx`
— its history references block `000000000000a42d3a9d0156b86605fbe568fc1aba1f408104e82e0a52da0371`
(height 65536, part of the bootstrap era right after the v4.0 genesis
restart, deeply confirmed on the active chain — see "ruled out" below).

## What we ruled out

- **Reorg / orphaned block.** `elektron-cli getblock <hash>` on the
  referenced block returns a normal result: height 65536, tens of
  thousands of confirmations, `previousblockhash`/`nextblockhash` both
  present and chained — solidly part of the main chain, not a stale tip.
- **Corrupted/stale index.** Wiped `data/electrs` and let electrs do a
  full resync from genesis (clean, zero errors, matched the daemon's tip).
  The very first query against the same address crashed identically,
  immediately after the fresh index caught up. Ruled out index staleness
  as the cause.
- **Wiring/mixing of two backends.** `bitcoin-api-factory.ts` in
  `elektron-net-mempool` is a plain `switch (config.MEMPOOL.BACKEND)` that
  instantiates exactly one backend implementation
  (`esplora` / `electrum` / `none`) — no dual-path, no leftover index from
  an earlier `MEMPOOL_BACKEND` setting can be "still active" alongside
  electrs.
- **Our own mining-pool indexer.** `pools-parser.ts` (`matchBlockMiner`)
  only compares coinbase data already stored in `elektron-mempool-db` when
  a block is indexed — it never calls into electrs/Electrum for balance
  lookups, so it isn't a trigger.
- **A generic "too many transactions" limit.** Reproduces even for
  addresses well under 10,000 entries and reproduces with
  `index_lookup_limit` disabled *or* set high enough that the count check
  never fires — i.e. it isn't really about volume, this one address just
  happens to touch the code path that races.

## Working theory

Race condition between electrs' background block indexer and the
on-demand Electrum RPC handler when both need the daemon's P2P
block-fetch channel around the same time — plausible given the crash
consistently correlates with an `indexing N blocks: [...]` log line
immediately preceding it. Not confirmed against electrs' Rust source in
detail; flagging as the leading hypothesis for whoever picks this up.

## Confirmed upstream, still open (as of writing, electrs v0.11.1 / Feb 2026)

This fork tracks `romanz/electrs` directly (see `Cargo.toml`:
`repository = "https://github.com/romanz/electrs"`), currently pinned at
`0.10.10`. The exact same failure signature is reported multiple times
upstream and **none of the following are marked fixed**:

- [romanz/electrs#1047](https://github.com/romanz/electrs/issues/1047) — "electrs stops when looking up for addresses with a lot of transactions" (closest match)
- [romanz/electrs#1055](https://github.com/romanz/electrs/issues/1055) — "Getting receiving on an empty and disconnected channel running in docker"
- [romanz/electrs#1069](https://github.com/romanz/electrs/issues/1069) — "electrs hangs on `get_history`"
- [romanz/electrs#1239](https://github.com/romanz/electrs/issues/1239) — "crash on wallet connect followed by the never ending restart loop" (matches our restart-loop pattern)
- [romanz/electrs#1314](https://github.com/romanz/electrs/issues/1314) — "electrs exits with 'receiving on an empty and disconnected channel'..." (newest, same error string)

Upgrading to `0.11.1` has **not** been tried on this fork yet and isn't
confirmed to fix it (none of the issues above are closed) — worth
testing, but expect to redo the chain-specific patches (network magic,
`MandatoryPruneDepth`, etc.) against the newer base.

## Current mitigation (deployed, not a fix)

`index_lookup_limit` in `electrs.toml` was tried as a workaround and
**rejected**: setting it low enough to make the broken address fail
cleanly (`>N index entries, query may take too long`, HTTP 413) also
blocks every other legitimately large address (a real address with
12,478 transactions and a healthy balance was collateral damage at
`index_lookup_limit = 5000`). Since the crash is isolated to one specific
address and self-heals in ~1s via `restart: unless-stopped`, we run with
`index_lookup_limit = 0` (disabled) in production — full address coverage
for everyone else, at the cost of a brief, isolated restart blip whenever
someone looks up the one known-bad address.

## Suggested next steps

1. Watch the upstream issues linked above for a fix landing.
2. Consider posting our reproduction (specific block + "crashes exactly
   once, immediately, on a freshly rebuilt index" timing) as a comment on
   [#1047](https://github.com/romanz/electrs/issues/1047) — it's a cleaner
   repro than what's currently on that thread.
3. If/when time allows: try the `0.11.1` upgrade in a branch, see if it's
   silently fixed despite the open issues.
4. If we ever want to dig into the actual Rust source ourselves, the
   likely starting points are the P2P block-fetch/channel handling used
   by the on-demand Electrum RPC lookups (`status.rs` / `tracker.rs`) vs.
   the indexer's own use of the same daemon connection.
