# Cross-chain NFT bridge vault (`nft-bridge`)

A Soroban contract that is both an ERC-721-style NFT collection and the
Soroban side of an Ethereum ⇄ Stellar NFT bridge.

| File | Purpose |
|------|---------|
| `src/lib.rs` | Contract entry points: ERC-721 surface, native minting, bridge calls, views |
| `src/vault.rs` | Escrow, relayer quorum, synthetic mint/burn, replay and double-mint guards |
| `src/storage.rs` | Typed storage; balances and approvals stay consistent with ownership |
| `src/test.rs` | Round-trip, security and fuzz tests against a model of the Ethereum side |
| `SECURITY.md` | Privilege audit, supply-parity invariants, mutation-testing results |

## Flows

```
Soroban → Ethereum → Soroban (native NFT)
  mint_native ─► lock ──(event: dest address)──► relayers mint wrapped on Ethereum
                 escrowed in vault                ...wrapped burned on Ethereum...
  owner restored ◄── unlock (relayer quorum, burn_id consumed once)

Ethereum → Soroban → Ethereum (ERC-721)
  NFT locked in Ethereum vault ──► mint_synthetic (relayer quorum, lock_id consumed once)
  synthetic circulates (transfer_from / approve)
  burn_synthetic ──(event: origin + dest address)──► relayers release on Ethereum
```

## Interface

**ERC-721 surface.** `name`, `symbol`, `total_supply`, `balance_of`, `owner_of`,
`token_uri`, `approve`, `get_approved`, `set_approval_for_all`,
`is_approved_for_all` and `transfer_from`. Where Solidity checks `msg.sender`,
each call instead takes the acting address and requires its
`require_auth()`. Token ids are `u128`.

**Bridge.**

| Function | Caller | Effect |
|---|---|---|
| `mint_native(to, uri)` | admin | issue a native NFT |
| `lock(caller, token_id, dest_chain_id, dest_address)` | owner / approved / operator | escrow a **native** NFT; emits `("bridge","lock",owner)` → `(nonce, token_id, dest_chain_id, dest_address, uri)` |
| `unlock(signers, UnlockAttestation)` | relayer quorum | release an escrowed native after its wrapped twin is burned on Ethereum |
| `mint_synthetic(signers, MintAttestation)` | relayer quorum | mint the synthetic twin of an ERC-721 locked on Ethereum |
| `burn_synthetic(caller, token_id, dest_address)` | owner / approved / operator | burn a synthetic; emits `("bridge","burn",owner)` → `(nonce, token_id, origin, dest_address)` |
| `set_relayers(signers, relayers, threshold)` | current relayer quorum | rotate the federation (strict majority threshold) |

**Views.** `token_kind`, `lock_of`, `synthetic_of`, `is_lock_consumed`,
`is_burn_consumed`, `relayers`, `threshold` and `stats`.

Ethereum identities use `BytesN<20>` for addresses and `BytesN<32>` (big-endian
`uint256`) for ERC-721 token ids. `EthOrigin { chain_id, contract, token_id }`
identifies an Ethereum NFT.

## Guarantees

* Synthetic NFTs cannot be double-minted: one live synthetic per origin, and
  each Ethereum event is consumed at most once.
* Synthetic NFTs can never enter the native vault pool, and nothing reaches
  escrow except through `lock`.
* Only a strict-majority quorum of relayers, each authorizing the exact
  attestation, can mint synthetics or release escrow. The admin cannot.

See `SECURITY.md` for the full audit.

## Build and test

```bash
cd contracts/nft-bridge
cargo test                                      # 12 tests incl. proptest fuzzing
cargo clippy --all-targets -- -D warnings
cargo build --target wasm32v1-none --release    # ~42 KB WASM
```
