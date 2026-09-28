// SPDX-License-Identifier: Apache-2.0
use soroban_sdk::{testutils::*, Address, Bytes, Symbol, Vec};
use soroban_sdk::token::{TokenClient, TokenType};
use soroban_sdk::stl::vec;

#[test]
fn test_atomic_swap_across_3_amm_contracts() {
    let env = Env::default();
    TestLedger::new(&env, 100_000_000_000);

    // Deploy mock AMM contracts
    let amm1 = Address::generate(&env);
    let amm2 = Address::generate(&env);
    let amm3 = Address::generate(&env);

    // Deploy coordinator
    let coordinator = CrossShardCoordinatorClient::new(&env, &CrossShardCoordinator::new(&env));

    // Simulate atomic swap with intentional failure in AMM3
    let transitions = vec![
        StateTransition {
            target_contract: amm1,
            function: Symbol::new(&env, "swap"),
            args: vec![Bytes::from_vec(&env, vec![100])],
        },
        StateTransition {
            target_contract: amm2,
            function: Symbol::new(&env, "swap"),
            args: vec![Bytes::from_vec(&env, vec![200])],
        },
        StateTransition {
            target_contract: amm3,
            function: Symbol::new(&env, "swap"),
            args: vec![Bytes::from_vec(&env, vec![300])], // Intentionally fails
        },
    ];

    // Assert strict rollback
    assert!(!coordinator.execute_atomic_transitions(transitions));
}