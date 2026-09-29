/**
 * Vitest unit tests for validateAuthTree and buildAuthEntries
 * (frontend/utils/auth_validator.ts).
 *
 * The canonical scenario mirrors the issue's acceptance criteria: Alice
 * authorizes a Router contract, which in turn calls a Liquidity Pool
 * contract — the resulting JSON payload is asserted against the exact shape
 * `stellar-sdk` expects for a `SorobanAuthorizationEntry` (credentials /
 * rootInvocation / subInvocations / function.contractAddress / functionName
 * / args), so it can be mapped field-for-field onto XDR construction.
 */

import { describe, it, expect } from 'vitest';
import {
  validateAuthTree,
  buildAuthEntries,
  scValFromArg,
  MAX_RECOMMENDED_DEPTH,
} from '../../../../utils/auth_validator';
import type { AuthTreeNode } from '../../../../utils/auth_validator';

// ---------------------------------------------------------------------------
// Test-data helpers
// ---------------------------------------------------------------------------

const ALICE = 'GALICE7777777777777777777777777777777777777777777777777777';
const ROUTER = 'CROUTER7777777777777777777777777777777777777777777777777777';
const POOL = 'CPOOL77777777777777777777777777777777777777777777777777777';

function makeAddress(id: string, address: string, children: AuthTreeNode[] = []): AuthTreeNode {
  return { id, kind: 'address', address, children };
}

function makeContract(
  id: string,
  contractAddress: string,
  functionName: string,
  children: AuthTreeNode[] = [],
  args: AuthTreeNode['args'] = [],
): AuthTreeNode {
  return { id, kind: 'contract', contractAddress, functionName, args, children };
}

/** Alice -> Router.swap(amount) -> Pool.withdraw(shares) */
function makeAliceRouterPoolTree(): AuthTreeNode[] {
  const pool = makeContract('pool', POOL, 'withdraw', [], [
    { name: 'shares', type: 'i128', value: '500' },
  ]);
  const router = makeContract(
    'router',
    ROUTER,
    'swap',
    [pool],
    [{ name: 'amount', type: 'i128', value: '1000' }],
  );
  return [makeAddress('alice', ALICE, [router])];
}

// ---------------------------------------------------------------------------
// validateAuthTree
// ---------------------------------------------------------------------------

describe('validateAuthTree — valid tree', () => {
  it('returns valid with no errors for the Alice -> Router -> Pool scenario', () => {
    const result = validateAuthTree(makeAliceRouterPoolTree());
    expect(result.valid).toBe(true);
    expect(result.errors).toHaveLength(0);
  });
});

describe('validateAuthTree — NO_ROOT_ADDRESS', () => {
  it('flags an empty forest', () => {
    const result = validateAuthTree([]);
    expect(result.valid).toBe(false);
    expect(result.errors.find((e) => e.code === 'NO_ROOT_ADDRESS')).toBeDefined();
  });

  it('flags a forest with only contract-kind roots', () => {
    const result = validateAuthTree([makeContract('c1', ROUTER, 'swap')]);
    expect(result.errors.find((e) => e.code === 'NO_ROOT_ADDRESS')).toBeDefined();
  });
});

describe('validateAuthTree — ADDRESS_MISSING_VALUE', () => {
  it('flags an address root with a blank address', () => {
    const result = validateAuthTree([makeAddress('alice', '', [makeContract('c1', ROUTER, 'swap')])]);
    const err = result.errors.find((e) => e.code === 'ADDRESS_MISSING_VALUE');
    expect(err).toBeDefined();
    expect(err!.nodeIds).toContain('alice');
  });
});

describe('validateAuthTree — ADDRESS_NO_ROOT_INVOCATION', () => {
  it('flags an address root with no children', () => {
    const result = validateAuthTree([makeAddress('alice', ALICE)]);
    const err = result.errors.find((e) => e.code === 'ADDRESS_NO_ROOT_INVOCATION');
    expect(err).toBeDefined();
    expect(err!.nodeIds).toContain('alice');
  });
});

describe('validateAuthTree — CONTRACT_INCOMPLETE', () => {
  it('flags a contract node missing its function name', () => {
    const tree = [makeAddress('alice', ALICE, [makeContract('router', ROUTER, '')])];
    const result = validateAuthTree(tree);
    const err = result.errors.find((e) => e.code === 'CONTRACT_INCOMPLETE');
    expect(err).toBeDefined();
    expect(err!.nodeIds).toContain('router');
  });

  it('flags a nested sub-invocation missing its contract address', () => {
    const pool = makeContract('pool', '', 'withdraw');
    const tree = [makeAddress('alice', ALICE, [makeContract('router', ROUTER, 'swap', [pool])])];
    const result = validateAuthTree(tree);
    const err = result.errors.find((e) => e.code === 'CONTRACT_INCOMPLETE');
    expect(err).toBeDefined();
    expect(err!.nodeIds).toContain('pool');
  });
});

describe('validateAuthTree — ADDRESS_NOT_AT_ROOT', () => {
  it('flags an address node nested under a contract', () => {
    const nestedAddress = makeAddress('bob', 'GBOB', []);
    const router = { ...makeContract('router', ROUTER, 'swap'), children: [nestedAddress] };
    const result = validateAuthTree([makeAddress('alice', ALICE, [router])]);
    const err = result.errors.find((e) => e.code === 'ADDRESS_NOT_AT_ROOT');
    expect(err).toBeDefined();
    expect(err!.nodeIds).toContain('bob');
  });
});

describe('validateAuthTree — DEEP_NESTING warning', () => {
  it('warns when a root invocation nests deeper than MAX_RECOMMENDED_DEPTH', () => {
    // Build a chain of MAX_RECOMMENDED_DEPTH + 2 nested contract calls.
    let leaf: AuthTreeNode = makeContract('c0', ROUTER, 'fn0');
    for (let i = 1; i <= MAX_RECOMMENDED_DEPTH + 2; i++) {
      leaf = makeContract(`c${i}`, ROUTER, `fn${i}`, [leaf]);
    }
    const result = validateAuthTree([makeAddress('alice', ALICE, [leaf])]);
    expect(result.warnings.find((w) => w.code === 'DEEP_NESTING')).toBeDefined();
    // Depth warning does not block export.
    expect(result.valid).toBe(true);
  });

  it('does not warn for a shallow tree', () => {
    const result = validateAuthTree(makeAliceRouterPoolTree());
    expect(result.warnings.find((w) => w.code === 'DEEP_NESTING')).toBeUndefined();
  });
});

describe('validateAuthTree — MISSING_ARGS warning', () => {
  it('warns when a contract node has zero args', () => {
    const tree = [makeAddress('alice', ALICE, [makeContract('router', ROUTER, 'swap', [], [])])];
    const result = validateAuthTree(tree);
    expect(result.warnings.find((w) => w.code === 'MISSING_ARGS')).toBeDefined();
  });
});

describe('validateAuthTree — REENTRANT_CONTRACT_CALL warning', () => {
  it('warns when the same contract address repeats along one call chain', () => {
    const inner = makeContract('inner', ROUTER, 'callback', [], [{ name: 'x', type: 'u32', value: '1' }]);
    const outer = makeContract('outer', ROUTER, 'swap', [inner], [{ name: 'y', type: 'u32', value: '2' }]);
    const result = validateAuthTree([makeAddress('alice', ALICE, [outer])]);
    const warn = result.warnings.find((w) => w.code === 'REENTRANT_CONTRACT_CALL');
    expect(warn).toBeDefined();
    expect(warn!.nodeIds).toContain('inner');
  });
});

describe('validateAuthTree — DUPLICATE_SIBLING_CALL warning', () => {
  it('warns when two siblings call the same contract + function', () => {
    const a = makeContract('a', POOL, 'withdraw', [], [{ name: 'shares', type: 'i128', value: '1' }]);
    const b = makeContract('b', POOL, 'withdraw', [], [{ name: 'shares', type: 'i128', value: '2' }]);
    const router = makeContract('router', ROUTER, 'swap', [a, b], [{ name: 'x', type: 'u32', value: '1' }]);
    const result = validateAuthTree([makeAddress('alice', ALICE, [router])]);
    const warn = result.warnings.find((w) => w.code === 'DUPLICATE_SIBLING_CALL');
    expect(warn).toBeDefined();
    expect(warn!.nodeIds).toEqual(expect.arrayContaining(['a', 'b']));
  });
});

// ---------------------------------------------------------------------------
// scValFromArg
// ---------------------------------------------------------------------------

describe('scValFromArg', () => {
  it('encodes bool args as a real boolean', () => {
    expect(scValFromArg({ name: 'flag', type: 'bool', value: 'true' })).toEqual({
      type: 'bool',
      value: true,
    });
    expect(scValFromArg({ name: 'flag', type: 'bool', value: 'false' })).toEqual({
      type: 'bool',
      value: false,
    });
  });

  it('passes through other scalar types as strings', () => {
    expect(scValFromArg({ name: 'amount', type: 'i128', value: '1000' })).toEqual({
      type: 'i128',
      value: '1000',
    });
    expect(scValFromArg({ name: 'who', type: 'address', value: ALICE })).toEqual({
      type: 'address',
      value: ALICE,
    });
  });
});

// ---------------------------------------------------------------------------
// buildAuthEntries — the Alice -> Router -> Pool scenario from the issue
// ---------------------------------------------------------------------------

describe('buildAuthEntries — Alice -> Router -> Liquidity Pool', () => {
  it('produces exactly one SorobanAuthorizationEntry rooted at Alice', () => {
    const entries = buildAuthEntries(makeAliceRouterPoolTree());
    expect(entries).toHaveLength(1);
  });

  it('matches the exact SorobanAuthorizationEntry JSON shape', () => {
    const entries = buildAuthEntries(makeAliceRouterPoolTree(), {
      nonce: () => '42',
      signatureExpirationLedger: 999,
    });

    expect(entries[0]).toEqual({
      credentials: {
        type: 'address',
        address: ALICE,
        nonce: '42',
        signatureExpirationLedger: 999,
      },
      rootInvocation: {
        function: {
          type: 'contract_fn',
          contractAddress: ROUTER,
          functionName: 'swap',
          args: [{ type: 'i128', value: '1000' }],
        },
        subInvocations: [
          {
            function: {
              type: 'contract_fn',
              contractAddress: POOL,
              functionName: 'withdraw',
              args: [{ type: 'i128', value: '500' }],
            },
            subInvocations: [],
          },
        ],
      },
    });
  });

  it('defaults signatureExpirationLedger to 0 and nonce to an incrementing index when omitted', () => {
    const entries = buildAuthEntries(makeAliceRouterPoolTree());
    expect(entries[0].credentials.signatureExpirationLedger).toBe(0);
    expect(entries[0].credentials.nonce).toBe('0');
  });

  it('produces one entry per direct child when an address authorizes multiple root invocations', () => {
    const tree = [
      makeAddress('alice', ALICE, [
        makeContract('r1', ROUTER, 'swap', [], [{ name: 'a', type: 'u32', value: '1' }]),
        makeContract('r2', POOL, 'deposit', [], [{ name: 'a', type: 'u32', value: '2' }]),
      ]),
    ];
    const entries = buildAuthEntries(tree);
    expect(entries).toHaveLength(2);
    expect(entries[0].rootInvocation.function.functionName).toBe('swap');
    expect(entries[1].rootInvocation.function.functionName).toBe('deposit');
    // Both entries share the same signer credentials.
    expect(entries[0].credentials.address).toBe(ALICE);
    expect(entries[1].credentials.address).toBe(ALICE);
  });

  it('ignores address roots with no address value or no children', () => {
    const entries = buildAuthEntries([makeAddress('empty', ''), makeAddress('lonely', ALICE)]);
    expect(entries).toHaveLength(0);
  });
});
