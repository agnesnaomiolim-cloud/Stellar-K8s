# State verification & snapshot oracle (`state-verifier`)

A Soroban contract that lets other contracts verify, cryptographically, that
a transaction was applied in a specific Stellar ledger.

| File | Purpose |
|------|---------|
| `src/lib.rs` | Contract: relayer checkpointing, `verify_tx_hash`, `verify_transaction`, views |
| `src/xdr_parser.rs` | Allocation-free XDR decoding of `LedgerHeader` and `TransactionEnvelope` (v1, fee-bump) |
| `src/merkle.rs` | RFC 6962 inclusion-proof verification using host SHA-256 |
| `src/test.rs` | Tests on real mainnet data, a reference-tree cross-check, fuzzing and gas profiling |
| `testdata/` | Mainnet ledgers 64670961/64670962 (protocol 28) and transactions, from Horizon |

## How verification works

```
relayer quorum ──submit_checkpoint(header XDR, tx_root, tx_count)──► contract
   header decoded from XDR in WASM
   ledger_hash = SHA-256(header XDR)                (computed on-chain)
   previousLedgerHash must link to checkpointed seq-1 / seq+1
   checkpoint stored: ledger hash, close time, txSetHash, tx_root, …

anyone ──verify_transaction(seq, envelope XDR, sig offset, index, proof)──► VerifiedTx
   envelope head decoded (source, fee, seq num, op count, fee-bump source)
   tx_hash = SHA-256(networkId ‖ envelopeType ‖ Transaction)   (on-chain)
   Merkle path: tx_hash → tx_root of the checkpoint
```

`verify_tx_hash(seq, tx_hash, index, proof)` is the cheaper variant for
callers that already hold the transaction hash.

**Merkle tree** (RFC 6962): leaves are the ledger's transaction hashes in
application order, `leaf = SHA-256(0x00 ‖ tx_hash)` and
`node = SHA-256(0x01 ‖ left ‖ right)`. The prefixes separate leaves from
internal nodes, which blocks second-preimage attacks. A node with no right
sibling is promoted unchanged, so the tree is never padded. The proof is the
concatenation of sibling hashes, leaf level first. The tests check the
contract against an independent recursive RFC 6962 implementation for every
index of every tree size from 1 to 64, and on the real 158-transaction
mainnet ledger.

**Envelope decoding.** `signatures_offset` marks where the envelope's
`signatures<20>` array starts. The contract accepts it only if the tail
decodes as exactly one signature array and the decoded head lies inside the
hashed transaction bytes. The reported fields are therefore always covered by
the transaction hash being proven. Operations and extensions are not decoded,
but they are fully bound by the hash. Legacy `ENVELOPE_TYPE_TX_V0` envelopes
(pre-protocol 13) are rejected.

## Trust model

* **Cryptographically verified on-chain:**
  - that the ledger hash matches the header;
  - every header field;
  - hash-chain continuity with adjacent checkpoints (`ChainMismatch` otherwise);
  - transaction hashes computed from envelopes;
  - Merkle inclusion against the checkpointed root.
* **Attested by the relayer quorum:** that the header is canonical (it has a
  valid hash, but the contract has not seen the Stellar validators agree on
  it), and that `tx_root` commits to that ledger's transactions. Stellar's
  own `txSetHash` is a flat SHA-256 over the entire (generalized)
  transaction set, not a Merkle root. Proving inclusion against it directly
  would require submitting the whole set, which does not fit in a Soroban
  transaction for busy ledgers. The transaction root is therefore checkpointed
  by the same quorum that attests the header, and every inclusion proof
  after that is trustless.
* **Quorum rules:**
  - `threshold` distinct registered relayers, with a strict majority of the set;
  - each relayer's `require_auth` covers the full call arguments;
  - the admin manages the set;
  - conflicting re-submissions are rejected;
  - identical re-submissions are no-ops.

## Gas profile (real WASM metering, default network budget)

Measured by `gas_profile_on_mainnet_data` on mainnet ledger 64670962.
Network per-transaction limit: 100,000,000 CPU instructions.

| Entry point | Input | CPU instructions | Memory (bytes) | % of CPU limit |
|---|---|---:|---:|---:|
| `submit_checkpoint` | 428-byte mainnet header | 4,819,410 | 2,053,078 | 4.82% |
| `verify_tx_hash` | 158-tx ledger (depth 8) | 4,648,175 | 2,034,638 | 4.65% |
| `verify_transaction` | v1 envelope, 204 B | 4,826,512 | 2,038,634 | 4.83% |
| `verify_transaction` | fee-bump envelope, 492 B | 4,857,418 | 2,039,634 | 4.86% |
| `verify_transaction` | 100-operation envelope, 15 KB | 5,605,245 | 2,068,516 | 5.61% |
| `verify_tx_hash` | worst case: depth 32 (2³²−1 leaves) | 5,223,044 | 2,042,298 | 5.22% |

* **Per Merkle level:** the depth-8 and depth-32 runs differ by about 575K
  instructions over 24 extra levels, so each level costs about 24K. That is
  three host calls (`bytes_new_from_linear_memory`, `compute_hash_sha256`,
  `bytes_copy_to_linear_memory`) on stack buffers, with no allocation. The
  whole proof is copied into guest memory with a single call.
* **Where the rest goes:** most of the roughly 4.6M baseline is Soroban
  instantiating the 35 KB WASM module, not contract logic. Envelope size adds
  only the SHA-256 of the transaction bytes (about +0.8M for 15 KB), because
  operations are hashed but never decoded.
* **Headroom:** every entry point stays below 6% of the per-transaction CPU
  limit, even at the maximum proof depth. The test asserts `< 100M` for every
  call and `< 10M` for the worst case.

## Build and test

```bash
cd contracts/state-verifier
cargo build --target wasm32v1-none --release       # enables the gas profile test
cargo test                                         # 17 tests
cargo test gas_profile -- --nocapture              # print the gas table
cargo clippy --all-targets -- -D warnings
```
