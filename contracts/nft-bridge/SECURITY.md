# Security audit: relayer privilege exclusivity and supply parity

Scope: `src/lib.rs`, `src/vault.rs`, `src/storage.rs` of the `nft-bridge`
contract. Every claim below cites the enforcing code and the test that fails
if that code is removed (verified by mutation testing; see §4).

## 1. Privilege matrix

| Entry point | Who can call it | Enforced by |
|---|---|---|
| `mint_synthetic` | quorum of registered relayers | `vault::require_relayer_quorum` |
| `unlock` (release escrowed native) | quorum of registered relayers | `vault::require_relayer_quorum` |
| `set_relayers` (rotate federation) | quorum of the **current** relayers | `vault::require_relayer_quorum` + `validate_relayer_set` |
| `mint_native` | collection admin | `admin.require_auth()` |
| `lock`, `burn_synthetic`, `transfer_from` | token owner, approved address or operator | `storage::require_can_manage` (`caller.require_auth()`) |
| `approve` | owner or operator | `caller.require_auth()` + ownership check |
| `set_approval_for_all` | the owner | `owner.require_auth()` |
| `initialize` | the admin being installed, once | `admin.require_auth()`, `AlreadyInitialized` |

**The admin has no bridge power.** It cannot mint or burn synthetics, release
escrowed natives, or change the relayer set. Those paths never read the admin
key. Test: `only_a_relayer_quorum_can_mint_or_unlock`,
`relayer_rotation_requires_current_quorum_and_majority`.

**Relayers have no power over circulating tokens.** A quorum can only create
synthetics and release *escrowed* natives. It cannot move, burn or lock a
token held by a user; those paths require the owner's signature. Burning a
synthetic is a holder action by design, and the relayers only observe it.

## 2. Relayer quorum (`vault::require_relayer_quorum`)

1. `signers.len() >= threshold`, else `InsufficientSignatures`.
2. No address appears twice, else `DuplicateSigner`, so one key cannot count as several.
3. Every signer is a registered relayer, else `NotRelayer`.
4. `signer.require_auth()` for **each** signer. A Soroban authorization
   covers the contract, the function and the full argument list, so each
   relayer's approval is bound to this exact attestation. It cannot be
   replayed on a different attestation or function.

Checks 1–3 run before any `require_auth`, so malformed signer lists fail fast.

**Federation invariants** (`validate_relayer_set`): `1 <= threshold <= n <= 20`,
no duplicates, and a strict majority (`2 * threshold > n`). With a majority
threshold, two disjoint relayer subsets can never both reach quorum, so a
minority cannot fork the federation. Rotation requires the current quorum,
and old relayers lose all privileges in the same transaction
(`relayer_rotation_requires_current_quorum_and_majority`).

Tests: `only_a_relayer_quorum_can_mint_or_unlock` (non-relayer, below threshold,
duplicate) and `every_listed_relayer_must_authorize_the_exact_attestation`.
The second test shows a relayer that is listed but never signed makes the call
fail, and that each recorded authorization equals
`(contract, "mint_synthetic", (signers, attestation))`.

## 3. Supply parity

| Invariant | Enforcement |
|---|---|
| An Ethereum NFT has at most one live synthetic | `SyntheticOf(origin)` must be absent to mint (`AlreadyMinted`); it is cleared only when that synthetic is burned |
| An Ethereum lock event backs at most one mint | `ConsumedLock(lock_id)` is permanent (`AttestationReplayed`) |
| An Ethereum burn event releases at most one native | `ConsumedBurn(burn_id)` is permanent (`AttestationReplayed`) |
| Synthetics never enter the native vault pool | `lock` rejects `TokenKind::Synthetic` (`CannotLockSynthetic`); `unlock` requires `Native` + a lock record (`NotLocked`) |
| Nothing enters escrow except through `lock` | `transfer_from`, `mint_native` and `mint_synthetic` reject the vault address (`InvalidRecipient`) |
| Escrowed natives are frozen | `require_can_manage` and `approve` reject vault-owned tokens (`TokenLocked`) |
| Attestations target the configured chain only | origin / destination chain id must equal `eth_chain_id` (`UnsupportedChain`) |

Accounting: `stats.native_locked` equals the escrowed natives, which equals the
wrapped twins on Ethereum. `stats.synthetic_live` equals the Ethereum NFTs
locked for Soroban. `fuzz_supply_parity` runs random multi-actor sequences
against a model of the Ethereum side (escrow + wrapped collection) and checks,
after every step:
- both equalities above;
- vault balance equals `native_locked`;
- the balances of all holders sum to `total_supply`;
- every replayed attestation is rejected.

## 4. Mutation testing

Each guard below was removed in turn and the suite re-run. Every mutation
was caught (at least one test failed):

| Guard removed | Caught |
|---|---|
| relayer `require_auth` | ✅ |
| relayer membership check | ✅ |
| threshold check | ✅ |
| duplicate-signer check | ✅ |
| majority-threshold rule | ✅ |
| `set_relayers` quorum | ✅ |
| double-mint (`SyntheticOf`) check | ✅ |
| mint-path lock replay check | ✅ |
| unlock replay check | ✅ |
| unlock "is escrowed native" check | ✅ |
| cannot-lock-synthetic check | ✅ |
| transfer-into-vault check | ✅ |
| `mint_native` admin auth | ✅ |
| owner/operator auth on `lock` / `burn` / `transfer` | ✅ |
| `initialize` admin auth | ✅ |

## 5. Trust assumptions and residual risk

* **Relayer honesty.** A colluding quorum can attest to a lock that never
  happened; no on-chain check on Soroban can prevent this. It is bounded by
  the majority threshold and key separation across relayer operators.
  Verifying Ethereum state on Soroban (for example with a light client) is
  outside this issue's scope.
* **Ethereum counterpart.** Parity also depends on the Ethereum vault
  honouring `lock` / `burn` events exactly once. Relayers should key those
  events on `nonce`, which is monotonic and unique per outbound event.
* **Storage TTL.** Persistent entries are extended on every write. Very old
  replay markers (`Consumed*`) could be archived, but archived entries are
  restored rather than read as absent, so replay protection holds.
