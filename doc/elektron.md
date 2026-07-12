# Elektron Net Fork Notes

This repository is a **permanently-diverged fork** of
[`romanz/electrs`](https://github.com/romanz/electrs), based on **v0.10.10**,
adapted for [Elektron Net](https://github.com/kutlusoy/elektron-net). Upstream
explicitly does not support altcoins, so Elektron-specific changes will never
be upstreamed; the fork picks up upstream protocol/security fixes by manual
porting instead.

v0.10.10 was chosen deliberately over the newer 0.11.x line: in 0.11.x the
indexing internals moved into the external `bindex` crate, while the Elektron
adaptations (pruned-node bootstrap, see below) need to modify exactly those
internals — 0.10.x keeps them all in this repository (`src/index.rs`,
`src/p2p.rs`, `src/db.rs`).

The full design rationale lives in the elektron-net repo:
[`doc-elektron/guideline-electrs-fork-integration.md`](https://github.com/kutlusoy/elektron-net/blob/main/doc-elektron/guideline-electrs-fork-integration.md).
This file documents only what is already implemented here.

## Pruned daemon is the expected steady state

Elektron Net enforces mandatory pruning on every node
(`MandatoryPruneDepth` = 197,280 blocks ≈ 137 days at 60-second blocks).
A non-pruned node does not exist on this network.

Upstream electrs refuses to start against a pruned daemon
(`electrs requires non-pruned bitcoind node`). This fork removes that
hard-fail: a pruned daemon is accepted and logged. Consequences for clients:

- **Headers are always complete.** The daemon retains the full header chain,
  so header subscription, checkpoints, and chain-of-work verification are
  unaffected by pruning.
- **Block bodies older than the retention window are gone** — network-wide,
  by design. Anything that needs the full block (merkle proofs, see below)
  is impossible for those heights; this is not a server malfunction.

## P2P: network magic and protocol version

electrs 0.10.x downloads blocks from the daemon over the Bitcoin P2P
protocol, so two Elektron Net specifics matter here:

- **Network magic.** Upstream only allows overriding the magic on signet;
  this fork allows it on every network (upstream later did the same in
  0.11.1). Running against `elektrond` mainnet requires:

  ```toml
  # electrs.toml
  signet_magic = "e1ec7a6e"   # Elektron Net mainnet magic
  ```

  (The option keeps its upstream name for config compatibility.)

- **Protocol version.** `elektrond` requires peers to advertise protocol
  version ≥ 70017 from genesis and disconnects anything lower. The P2P
  handshake (`src/p2p.rs`) therefore advertises 70017 instead of
  rust-bitcoin's default constant.

- **Genesis block.** `Network::Bitcoin` serves as the internal Elektron
  mainnet stand-in, so `src/chain.rs` seeds the header chain with Elektron
  Net's genesis header (hash
  `00000006b054338443f1a5d5534df21eab0d13232028158ae198edbb169f9dad`, built
  from the chainparams.cpp constants and self-checked at startup) instead of
  rust-bitcoin's Bitcoin genesis — otherwise no header from `elektrond`
  could ever attach ("missing prev_blockhash"). Consequence: this binary
  cannot index Bitcoin mainnet, and an Elektron *testnet* deployment would
  need the analogous override for its own genesis first.

## Typed "block pruned" Electrum error (code 3)

`blockchain.transaction.get_merkle` and `blockchain.transaction.id_from_pos`
need the block's full txid list, fetched from the daemon via RPC. For heights
older than the retention window that call fails on every Elektron Net node,
permanently.

Instead of forwarding the daemon's raw error text (upstream behavior, error
code 2, indistinguishable from any other RPC failure), this fork returns a
stable, typed error:

```json
{"code": 3, "message": "block at height <h> is pruned - unavailable by design (Elektron Net mandatory pruning)"}
```

Wallet clients SHOULD treat code 3 as "proof unavailable by design" (a
permanent property of the requested height, safe to cache) and MUST NOT treat
it as a server error worth retrying or failing over for.

Error codes used by this server:

| Code | Meaning |
|------|---------|
| 1 | bad request (upstream) |
| 2 | daemon RPC error, forwarded (upstream) |
| 3 | block pruned — unavailable by design (this fork) |
| -32700 … -32603 | standard JSON-RPC errors (upstream) |

## Not yet implemented (planned, see the integration guideline)

- **§3.2 UTXO-snapshot bootstrap** of the scripthash index on first start
  (from `dumptxoutset` or the node's automatic checkpoint snapshots), with the
  bootstrap height recorded as the index's effective genesis. Until this
  lands, the index can only be built by syncing blocks that are still within
  the daemon's retention window.
- **§3.3 network identity**: mainnet bech32 HRP `be`; base58 prefixes and the
  RPC port already match Bitcoin's and need no override. (electrs itself works
  on scripthashes, so this mainly affects log/debug output.)
