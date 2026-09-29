extern crate std;

use proptest::prelude::*;
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, AuthorizedFunction, Events, MockAuth, MockAuthInvoke},
    vec, Address, BytesN, Env, IntoVal, String, Symbol, Val, Vec,
};
use std::collections::{BTreeMap, BTreeSet};

use crate::{
    BridgeStats, Error, EthOrigin, MintAttestation, NftBridge, NftBridgeClient, TokenKind,
    UnlockAttestation,
};

const ETH_CHAIN: u64 = 1;

// ======================================================================
// Harness
// ======================================================================

struct Bridge {
    env: Env,
    id: Address,
    admin: Address,
    relayers: [Address; 3],
}

impl Bridge {
    /// 2-of-3 relayer federation.
    fn new() -> Self {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let relayers = [
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
        ];
        let id = env.register(NftBridge, ());
        NftBridgeClient::new(&env, &id).initialize(
            &admin,
            &String::from_str(&env, "Bridged Punks"),
            &String::from_str(&env, "BPNK"),
            &ETH_CHAIN,
            &vec![
                &env,
                relayers[0].clone(),
                relayers[1].clone(),
                relayers[2].clone(),
            ],
            &2,
        );
        Bridge {
            env,
            id,
            admin,
            relayers,
        }
    }

    fn c(&self) -> NftBridgeClient<'_> {
        NftBridgeClient::new(&self.env, &self.id)
    }

    fn quorum(&self) -> Vec<Address> {
        vec![
            &self.env,
            self.relayers[0].clone(),
            self.relayers[1].clone(),
        ]
    }

    fn user(&self) -> Address {
        Address::generate(&self.env)
    }

    fn uri(&self, s: &str) -> String {
        String::from_str(&self.env, s)
    }

    fn eth_addr(&self, b: u8) -> BytesN<20> {
        BytesN::from_array(&self.env, &[b; 20])
    }

    fn id32(&self, n: u64) -> BytesN<32> {
        let mut a = [0u8; 32];
        a[24..].copy_from_slice(&n.to_be_bytes());
        BytesN::from_array(&self.env, &a)
    }

    fn origin(&self, token: u64) -> EthOrigin {
        EthOrigin {
            chain_id: ETH_CHAIN,
            contract: self.eth_addr(0xAB),
            token_id: self.id32(token),
        }
    }

    fn mint_att(&self, lock_id: u64, token: u64, to: &Address) -> MintAttestation {
        MintAttestation {
            lock_id: self.id32(lock_id),
            origin: self.origin(token),
            recipient: to.clone(),
            token_uri: self.uri("ipfs://eth-token"),
        }
    }

    fn unlock_att(&self, burn_id: u64, token_id: u128, to: &Address) -> UnlockAttestation {
        UnlockAttestation {
            burn_id: self.id32(burn_id),
            token_id,
            recipient: to.clone(),
        }
    }
}

/// Minimal model of the Ethereum side of the bridge: an ERC-721 escrow vault
/// (for Ethereum-native NFTs) and a wrapped collection (for Soroban natives).
#[derive(Default)]
struct EthSide {
    /// Ethereum-native NFTs escrowed in the Ethereum vault.
    escrowed: BTreeSet<u64>,
    /// Wrapped twins of Soroban natives: soroban token id -> eth holder.
    wrapped: BTreeMap<u128, u8>,
}

/// Asserts the most recent contract event (compared as a host Vec, since
/// `Val` has no structural equality).
fn assert_last_event(b: &Bridge, topics: Vec<Val>, data: Val) {
    let all = b.env.events().all();
    let last = all.slice(all.len() - 1..);
    assert_eq!(last, vec![&b.env, (b.id.clone(), topics, data)]);
}

/// Cross-chain supply parity, checked against the contract's own state.
fn assert_parity(b: &Bridge, eth: &EthSide) {
    let s: BridgeStats = b.c().stats();
    assert_eq!(
        s.native_locked as usize,
        eth.wrapped.len(),
        "each escrowed native has one wrapped twin"
    );
    assert_eq!(
        s.synthetic_live as usize,
        eth.escrowed.len(),
        "each Ethereum-locked NFT has one synthetic"
    );
    for token_id in eth.wrapped.keys() {
        assert_eq!(
            b.c().owner_of(token_id),
            b.id,
            "wrapped twin implies native is escrowed"
        );
        assert!(b.c().lock_of(token_id).is_some());
    }
    for token in &eth.escrowed {
        assert!(
            b.c().synthetic_of(&b.origin(*token)).is_some(),
            "escrowed origin has a synthetic"
        );
    }
    assert_eq!(
        b.c().balance_of(&b.id),
        s.native_locked,
        "vault holds exactly the escrowed natives"
    );
}

// ======================================================================
// Round trips
// ======================================================================

/// Ethereum → Soroban → Ethereum: lock on Ethereum, relayers mint the
/// synthetic, it circulates, holder burns it, relayers release on Ethereum.
#[test]
fn round_trip_ethereum_nft_through_soroban() {
    let b = Bridge::new();
    let mut eth = EthSide::default();
    let (alice, bob) = (b.user(), b.user());

    // Ethereum: token 42 escrowed in the Ethereum vault (lock id 7).
    eth.escrowed.insert(42);
    let token_id = b
        .c()
        .mint_synthetic(&b.quorum(), &b.mint_att(7, 42, &alice));
    assert_parity(&b, &eth);
    assert_eq!(b.c().owner_of(&token_id), alice);
    assert_eq!(
        b.c().token_kind(&token_id),
        TokenKind::Synthetic(b.origin(42))
    );
    assert_eq!(b.c().token_uri(&token_id), b.uri("ipfs://eth-token"));
    assert!(b.c().is_lock_consumed(&b.id32(7)));

    // Circulates on Soroban like any ERC-721.
    b.c().transfer_from(&alice, &alice, &bob, &token_id);
    assert_eq!(b.c().owner_of(&token_id), bob);

    // Bob burns to bridge back to 0x11..11 on Ethereum.
    let dest = b.eth_addr(0x11);
    let nonce = b.c().burn_synthetic(&bob, &token_id, &dest);
    assert_last_event(
        &b,
        (symbol_short!("bridge"), symbol_short!("burn"), bob.clone()).into_val(&b.env),
        (nonce, token_id, b.origin(42), dest.clone()).into_val(&b.env),
    );
    // Relayers observe the burn and release token 42 on Ethereum.
    eth.escrowed.remove(&42);
    assert_parity(&b, &eth);
    assert!(b.c().try_owner_of(&token_id).is_err(), "synthetic is gone");
    assert_eq!(b.c().synthetic_of(&b.origin(42)), None);
    assert_eq!(b.c().balance_of(&bob), 0);

    // The NFT can travel again after a *new* Ethereum lock...
    eth.escrowed.insert(42);
    let again = b
        .c()
        .mint_synthetic(&b.quorum(), &b.mint_att(8, 42, &alice));
    assert_ne!(again, token_id, "fresh synthetic id");
    assert_parity(&b, &eth);
    // ...but the old lock can never be replayed.
    b.c().burn_synthetic(&alice, &again, &dest);
    eth.escrowed.remove(&42);
    assert_eq!(
        b.c()
            .try_mint_synthetic(&b.quorum(), &b.mint_att(7, 42, &alice)),
        Err(Ok(Error::AttestationReplayed))
    );
    assert_parity(&b, &eth);
}

/// Soroban → Ethereum → Soroban: native NFT escrowed, wrapped on Ethereum,
/// wrapped burned, relayers release the native.
#[test]
fn round_trip_native_nft_through_ethereum() {
    let b = Bridge::new();
    let mut eth = EthSide::default();
    let (alice, carol) = (b.user(), b.user());

    let token_id = b.c().mint_native(&alice, &b.uri("ipfs://native"));
    assert_eq!(b.c().token_kind(&token_id), TokenKind::Native);

    let dest = b.eth_addr(0x22);
    let nonce = b.c().lock(&alice, &token_id, &ETH_CHAIN, &dest);
    assert_last_event(
        &b,
        (
            symbol_short!("bridge"),
            symbol_short!("lock"),
            alice.clone(),
        )
            .into_val(&b.env),
        (
            nonce,
            token_id,
            ETH_CHAIN,
            dest.clone(),
            b.uri("ipfs://native"),
        )
            .into_val(&b.env),
    );
    assert_eq!(b.c().owner_of(&token_id), b.id, "escrowed in the vault");
    let rec = b.c().lock_of(&token_id).unwrap();
    assert_eq!((rec.dest_address, rec.locked_by), (dest, alice.clone()));
    // Relayers mint the wrapped twin on Ethereum.
    eth.wrapped.insert(token_id, 0x22);
    assert_parity(&b, &eth);

    // Escrowed NFTs are frozen.
    assert_eq!(
        b.c().try_transfer_from(&alice, &alice, &carol, &token_id),
        Err(Ok(Error::TokenLocked))
    );
    assert_eq!(
        b.c().try_approve(&alice, &Some(carol.clone()), &token_id),
        Err(Ok(Error::TokenLocked))
    );
    assert_eq!(
        b.c().try_lock(&alice, &token_id, &ETH_CHAIN, &dest_or(&b)),
        Err(Ok(Error::TokenLocked))
    );

    // Wrapped twin burned on Ethereum, released to Carol on Soroban.
    eth.wrapped.remove(&token_id);
    b.c()
        .unlock(&b.quorum(), &b.unlock_att(99, token_id, &carol));
    assert_eq!(b.c().owner_of(&token_id), carol);
    assert_eq!(b.c().lock_of(&token_id), None);
    assert_parity(&b, &eth);

    // The same burn cannot release it twice.
    assert_eq!(
        b.c()
            .try_unlock(&b.quorum(), &b.unlock_att(99, token_id, &carol)),
        Err(Ok(Error::AttestationReplayed))
    );
    // A fresh attestation for a token that is not escrowed is rejected too.
    assert_eq!(
        b.c()
            .try_unlock(&b.quorum(), &b.unlock_att(100, token_id, &carol)),
        Err(Ok(Error::NotLocked))
    );
}

fn dest_or(b: &Bridge) -> BytesN<20> {
    b.eth_addr(0x33)
}

// ======================================================================
// Double-mint and vault-pool isolation
// ======================================================================

#[test]
fn synthetic_cannot_be_double_minted() {
    let b = Bridge::new();
    let alice = b.user();
    b.c().mint_synthetic(&b.quorum(), &b.mint_att(1, 5, &alice));
    // Same origin via a different (forged or duplicated) lock event.
    assert_eq!(
        b.c()
            .try_mint_synthetic(&b.quorum(), &b.mint_att(2, 5, &alice)),
        Err(Ok(Error::AlreadyMinted))
    );
    // Same lock event replayed for another origin.
    assert_eq!(
        b.c()
            .try_mint_synthetic(&b.quorum(), &b.mint_att(1, 6, &alice)),
        Err(Ok(Error::AttestationReplayed))
    );
    assert_eq!(b.c().stats().synthetic_live, 1);
}

#[test]
fn synthetic_never_enters_native_vault_pool() {
    let b = Bridge::new();
    let alice = b.user();
    let syn = b.c().mint_synthetic(&b.quorum(), &b.mint_att(1, 5, &alice));
    let native = b.c().mint_native(&alice, &b.uri("n"));

    assert_eq!(
        b.c().try_lock(&alice, &syn, &ETH_CHAIN, &b.eth_addr(1)),
        Err(Ok(Error::CannotLockSynthetic))
    );
    // Nothing can be pushed into the vault address directly.
    for t in [syn, native] {
        assert_eq!(
            b.c().try_transfer_from(&alice, &alice, &b.id, &t),
            Err(Ok(Error::InvalidRecipient))
        );
    }
    // Relayers cannot release a synthetic through the native unlock path,
    // nor burn a native through the synthetic path.
    assert_eq!(
        b.c().try_unlock(&b.quorum(), &b.unlock_att(1, syn, &alice)),
        Err(Ok(Error::NotLocked))
    );
    assert_eq!(
        b.c().try_burn_synthetic(&alice, &native, &b.eth_addr(1)),
        Err(Ok(Error::NotSynthetic))
    );
    // Neither minting path can target the vault itself.
    assert_eq!(
        b.c()
            .try_mint_synthetic(&b.quorum(), &b.mint_att(2, 6, &b.id)),
        Err(Ok(Error::InvalidRecipient))
    );
    assert_eq!(
        b.c().try_mint_native(&b.id, &b.uri("x")),
        Err(Ok(Error::InvalidRecipient))
    );
    assert_eq!(b.c().balance_of(&b.id), 0);
}

#[test]
fn bridge_inputs_are_validated() {
    let b = Bridge::new();
    let alice = b.user();
    let native = b.c().mint_native(&alice, &b.uri("n"));
    assert_eq!(
        b.c().try_lock(&alice, &native, &5, &b.eth_addr(1)),
        Err(Ok(Error::UnsupportedChain))
    );
    assert_eq!(
        b.c().try_lock(&alice, &native, &ETH_CHAIN, &b.eth_addr(0)),
        Err(Ok(Error::InvalidDestination))
    );
    let mut att = b.mint_att(1, 1, &alice);
    att.origin.chain_id = 137;
    assert_eq!(
        b.c().try_mint_synthetic(&b.quorum(), &att),
        Err(Ok(Error::UnsupportedChain))
    );
    assert_eq!(b.c().try_owner_of(&999), Err(Ok(Error::TokenNotFound)));
}

// ======================================================================
// Relayer privilege exclusivity (SECURITY.md §1)
// ======================================================================

#[test]
fn only_a_relayer_quorum_can_mint_or_unlock() {
    let b = Bridge::new();
    let alice = b.user();
    let outsider = b.user();
    let att = b.mint_att(1, 1, &alice);
    let [r0, r1, r2] = b.relayers.clone();

    // Admin and arbitrary accounts are not relayers.
    for bad in [b.admin.clone(), outsider.clone()] {
        let signers = vec![&b.env, r0.clone(), bad];
        assert_eq!(
            b.c().try_mint_synthetic(&signers, &att),
            Err(Ok(Error::NotRelayer))
        );
    }
    // One relayer is below the 2-of-3 threshold.
    assert_eq!(
        b.c().try_mint_synthetic(&vec![&b.env, r2.clone()], &att),
        Err(Ok(Error::InsufficientSignatures))
    );
    // A relayer cannot count twice.
    assert_eq!(
        b.c()
            .try_mint_synthetic(&vec![&b.env, r1.clone(), r1.clone()], &att),
        Err(Ok(Error::DuplicateSigner))
    );
    // The unlock path enforces the identical quorum.
    let native = b.c().mint_native(&alice, &b.uri("n"));
    b.c().lock(&alice, &native, &ETH_CHAIN, &b.eth_addr(1));
    assert_eq!(
        b.c().try_unlock(
            &vec![&b.env, b.admin.clone(), r0.clone()],
            &b.unlock_att(1, native, &alice)
        ),
        Err(Ok(Error::NotRelayer))
    );
    assert_eq!(b.c().stats().synthetic_live, 0);
    assert_eq!(b.c().owner_of(&native), b.id);
}

/// Each listed relayer must actually authorize: naming a relayer without its
/// signature fails, and the recorded authorization covers the attestation.
#[test]
fn every_listed_relayer_must_authorize_the_exact_attestation() {
    let b = Bridge::new();
    let alice = b.user();
    let att = b.mint_att(1, 1, &alice);
    let signers = b.quorum();
    let args: Vec<Val> = (signers.clone(), att.clone()).into_val(&b.env);

    // Only relayer 0 signs; relayer 1 is listed but never authorized.
    b.env.mock_auths(&[MockAuth {
        address: &b.relayers[0],
        invoke: &MockAuthInvoke {
            contract: &b.id,
            fn_name: "mint_synthetic",
            args: args.clone(),
            sub_invokes: &[],
        },
    }]);
    assert!(b.c().try_mint_synthetic(&signers, &att).is_err());
    assert_eq!(b.c().stats().synthetic_live, 0);

    // Both sign: succeeds, and both authorizations are bound to this call.
    b.env.mock_all_auths();
    b.c().mint_synthetic(&signers, &att);
    let auths = b.env.auths();
    for r in [&b.relayers[0], &b.relayers[1]] {
        let (_, inv) = auths
            .iter()
            .find(|(a, _)| a == r)
            .expect("relayer auth recorded");
        assert_eq!(
            inv.function,
            AuthorizedFunction::Contract((
                b.id.clone(),
                Symbol::new(&b.env, "mint_synthetic"),
                args.clone()
            ))
        );
    }
    assert!(
        auths.iter().all(|(a, _)| *a != b.admin),
        "admin never needed"
    );
}

/// Addresses whose authorization was required by the most recent call.
fn signers_of_last_call(b: &Bridge) -> std::vec::Vec<Address> {
    b.env.auths().into_iter().map(|(a, _)| a).collect()
}

/// Every user-facing action demands the acting address's own signature, and
/// only the admin can issue native NFTs.
#[test]
fn every_action_requires_the_actors_signature() {
    let b = Bridge::new();
    let (alice, bob) = (b.user(), b.user());

    let t = b.c().mint_native(&alice, &b.uri("n"));
    assert_eq!(
        signers_of_last_call(&b),
        std::slice::from_ref(&b.admin),
        "admin signs native mints"
    );

    b.c().transfer_from(&alice, &alice, &bob, &t);
    assert_eq!(signers_of_last_call(&b), std::slice::from_ref(&alice));

    b.c().lock(&bob, &t, &ETH_CHAIN, &b.eth_addr(1));
    assert_eq!(signers_of_last_call(&b), std::slice::from_ref(&bob));

    let syn = b.c().mint_synthetic(&b.quorum(), &b.mint_att(1, 1, &alice));
    b.c().burn_synthetic(&alice, &syn, &b.eth_addr(1));
    assert_eq!(signers_of_last_call(&b), std::slice::from_ref(&alice));

    // Initialization is signed by the admin being installed.
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register(NftBridge, ());
    let (a, r) = (Address::generate(&env), Address::generate(&env));
    let s = String::from_str(&env, "n");
    NftBridgeClient::new(&env, &id).initialize(&a, &s, &s, &1, &vec![&env, r], &1);
    let signed: std::vec::Vec<Address> = env.auths().into_iter().map(|(x, _)| x).collect();
    assert_eq!(signed, [a]);
}

#[test]
fn relayer_rotation_requires_current_quorum_and_majority() {
    let b = Bridge::new();
    let (n0, n1, n2) = (b.user(), b.user(), b.user());
    let new_set = vec![&b.env, n0.clone(), n1.clone(), n2.clone()];

    // Admin cannot rotate the federation.
    assert_eq!(
        b.c()
            .try_set_relayers(&vec![&b.env, b.admin.clone()], &new_set, &2),
        Err(Ok(Error::InsufficientSignatures))
    );
    // Minority / degenerate thresholds are rejected.
    for bad in [0u32, 1, 4] {
        assert_eq!(
            b.c().try_set_relayers(&b.quorum(), &new_set, &bad),
            Err(Ok(Error::InvalidRelayerSet))
        );
    }
    assert_eq!(
        b.c()
            .try_set_relayers(&b.quorum(), &vec![&b.env, n0.clone(), n0.clone()], &2),
        Err(Ok(Error::InvalidRelayerSet))
    );

    b.c().set_relayers(&b.quorum(), &new_set, &2);
    assert_eq!(b.c().relayers(), new_set);
    // Old relayers lost their privileges immediately.
    assert_eq!(
        b.c()
            .try_mint_synthetic(&b.quorum(), &b.mint_att(1, 1, &n0)),
        Err(Ok(Error::NotRelayer))
    );
    b.c()
        .mint_synthetic(&vec![&b.env, n1, n2], &b.mint_att(1, 1, &n0));
}

#[test]
fn initialize_rejects_bad_config_and_reinit() {
    let b = Bridge::new();
    let r = vec![&b.env, b.relayers[0].clone()];
    assert_eq!(
        b.c()
            .try_initialize(&b.admin, &b.uri("x"), &b.uri("X"), &ETH_CHAIN, &r, &1),
        Err(Ok(Error::AlreadyInitialized))
    );
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register(NftBridge, ());
    let c = NftBridgeClient::new(&env, &id);
    let (a, x, y) = (
        Address::generate(&env),
        Address::generate(&env),
        Address::generate(&env),
    );
    let s = String::from_str(&env, "n");
    assert_eq!(
        c.try_initialize(&a, &s, &s, &1, &Vec::new(&env), &1),
        Err(Ok(Error::InvalidRelayerSet))
    );
    assert_eq!(
        c.try_initialize(&a, &s, &s, &1, &vec![&env, x.clone(), y.clone()], &1),
        Err(Ok(Error::InvalidRelayerSet)),
        "1-of-2 is not a majority"
    );
    c.initialize(&a, &s, &s, &1, &vec![&env, x, y], &2);
}

// ======================================================================
// ERC-721 semantics
// ======================================================================

#[test]
fn erc721_approvals_and_operators() {
    let b = Bridge::new();
    let (alice, bob, op, mallory) = (b.user(), b.user(), b.user(), b.user());
    let t = b.c().mint_native(&alice, &b.uri("n"));
    assert_eq!((b.c().balance_of(&alice), b.c().total_supply()), (1, 1));

    assert_eq!(
        b.c().try_transfer_from(&mallory, &alice, &mallory, &t),
        Err(Ok(Error::NotAuthorized))
    );
    assert_eq!(
        b.c().try_approve(&mallory, &Some(mallory.clone()), &t),
        Err(Ok(Error::NotAuthorized))
    );

    // Single-token approval is consumed by a transfer.
    b.c().approve(&alice, &Some(bob.clone()), &t);
    assert_eq!(b.c().get_approved(&t), Some(bob.clone()));
    b.c().transfer_from(&bob, &alice, &bob, &t);
    assert_eq!(b.c().get_approved(&t), None);
    assert_eq!((b.c().balance_of(&alice), b.c().balance_of(&bob)), (0, 1));

    // Operators can transfer and lock on the owner's behalf.
    b.c().set_approval_for_all(&bob, &op, &true);
    assert!(b.c().is_approved_for_all(&bob, &op));
    b.c().approve(&op, &Some(alice.clone()), &t);
    b.c().lock(&op, &t, &ETH_CHAIN, &b.eth_addr(9));
    assert_eq!(b.c().lock_of(&t).unwrap().locked_by, bob);
    b.c().set_approval_for_all(&bob, &op, &false);
    assert!(!b.c().is_approved_for_all(&bob, &op));

    // `from` must be the current owner.
    let t2 = b.c().mint_native(&alice, &b.uri("m"));
    assert_eq!(
        b.c().try_transfer_from(&alice, &bob, &mallory, &t2),
        Err(Ok(Error::NotAuthorized))
    );
    assert_eq!(b.c().name(), b.uri("Bridged Punks"));
    assert_eq!(b.c().symbol(), b.uri("BPNK"));
}

// ======================================================================
// Fuzzing: random multi-actor sequences keep cross-chain supply parity
// ======================================================================

#[derive(Debug, Clone)]
enum Op {
    MintNative(u8),
    Lock(u8),
    Unlock(u8, u8),
    EthLockAndMint(u8, u8),
    Burn(u8),
    Transfer(u8, u8),
    ReplayMint(u8),
    ReplayUnlock(u8),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        any::<u8>().prop_map(Op::MintNative),
        any::<u8>().prop_map(Op::Lock),
        (any::<u8>(), any::<u8>()).prop_map(|(a, b)| Op::Unlock(a, b)),
        (any::<u8>(), any::<u8>()).prop_map(|(a, b)| Op::EthLockAndMint(a, b)),
        any::<u8>().prop_map(Op::Burn),
        (any::<u8>(), any::<u8>()).prop_map(|(a, b)| Op::Transfer(a, b)),
        any::<u8>().prop_map(Op::ReplayMint),
        any::<u8>().prop_map(Op::ReplayUnlock),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn fuzz_supply_parity(ops in prop::collection::vec(op_strategy(), 1..40)) {
        let b = Bridge::new();
        let mut eth = EthSide::default();
        let users: std::vec::Vec<Address> = (0..4).map(|_| b.user()).collect();
        let mut tokens: std::vec::Vec<u128> = std::vec::Vec::new();
        let mut used_locks: std::vec::Vec<u64> = std::vec::Vec::new();
        let mut used_burns: std::vec::Vec<(u64, u128)> = std::vec::Vec::new();
        let mut next_event = 1u64;

        let pick = |v: &std::vec::Vec<u128>, i: u8| -> Option<u128> {
            if v.is_empty() { None } else { Some(v[i as usize % v.len()]) }
        };

        for op in ops {
            match op {
                Op::MintNative(u) => {
                    tokens.push(b.c().mint_native(&users[u as usize % 4], &b.uri("n")));
                }
                Op::Lock(i) => if let Some(t) = pick(&tokens, i) {
                    let owner = b.c().owner_of(&t);
                    let res = b.c().try_lock(&owner, &t, &ETH_CHAIN, &b.eth_addr(7));
                    match b.c().token_kind(&t) {
                        TokenKind::Synthetic(_) => prop_assert_eq!(res, Err(Ok(Error::CannotLockSynthetic))),
                        TokenKind::Native if owner == b.id => prop_assert_eq!(res, Err(Ok(Error::TokenLocked))),
                        TokenKind::Native => { prop_assert!(res.is_ok()); eth.wrapped.insert(t, 7); }
                    }
                },
                Op::Unlock(i, u) => if let Some(t) = pick(&tokens, i) {
                    let burn = next_event; next_event += 1;
                    let res = b.c().try_unlock(&b.quorum(), &b.unlock_att(burn, t, &users[u as usize % 4]));
                    if eth.wrapped.remove(&t).is_some() {
                        prop_assert!(res.is_ok());
                        used_burns.push((burn, t));
                    } else {
                        prop_assert_eq!(res, Err(Ok(Error::NotLocked)));
                    }
                },
                Op::EthLockAndMint(n, u) => {
                    let origin = (n % 8) as u64;
                    let lock = next_event; next_event += 1;
                    let res = b.c().try_mint_synthetic(&b.quorum(), &b.mint_att(lock, origin, &users[u as usize % 4]));
                    if eth.escrowed.insert(origin) {
                        match res { Ok(Ok(t)) => tokens.push(t), other => prop_assert!(false, "{:?}", other) }
                        used_locks.push(lock);
                    } else {
                        prop_assert_eq!(res, Err(Ok(Error::AlreadyMinted)));
                    }
                }
                Op::Burn(i) => if let Some(t) = pick(&tokens, i) {
                    let owner = b.c().owner_of(&t);
                    let kind = b.c().token_kind(&t);
                    let res = b.c().try_burn_synthetic(&owner, &t, &b.eth_addr(3));
                    match kind {
                        TokenKind::Synthetic(o) => {
                            prop_assert!(res.is_ok());
                            tokens.retain(|x| *x != t);
                            let mut raw = [0u8; 32];
                            o.token_id.copy_into_slice(&mut raw);
                            eth.escrowed.remove(&u64::from_be_bytes(raw[24..].try_into().unwrap()));
                        }
                        TokenKind::Native => prop_assert_eq!(res, Err(Ok(Error::NotSynthetic))),
                    }
                },
                Op::Transfer(i, u) => if let Some(t) = pick(&tokens, i) {
                    if let Ok(Ok(owner)) = b.c().try_owner_of(&t) {
                        let to = users[u as usize % 4].clone();
                        let res = b.c().try_transfer_from(&owner, &owner, &to, &t);
                        prop_assert_eq!(res.is_ok(), owner != b.id);
                    }
                },
                Op::ReplayMint(i) => if !used_locks.is_empty() {
                    let lock = used_locks[i as usize % used_locks.len()];
                    let res = b.c().try_mint_synthetic(&b.quorum(), &b.mint_att(lock, 100 + i as u64, &users[0]));
                    prop_assert_eq!(res, Err(Ok(Error::AttestationReplayed)));
                },
                Op::ReplayUnlock(i) => if !used_burns.is_empty() {
                    let (burn, t) = used_burns[i as usize % used_burns.len()];
                    let res = b.c().try_unlock(&b.quorum(), &b.unlock_att(burn, t, &users[0]));
                    prop_assert_eq!(res, Err(Ok(Error::AttestationReplayed)));
                },
            }
            assert_parity(&b, &eth);
            let s = b.c().stats();
            let held: u64 = users.iter().map(|u| b.c().balance_of(u)).sum::<u64>() + b.c().balance_of(&b.id);
            prop_assert_eq!(held, b.c().total_supply(), "balances sum to supply");
            prop_assert_eq!(b.c().total_supply(), s.native_minted + s.synthetic_live);
        }
    }
}
