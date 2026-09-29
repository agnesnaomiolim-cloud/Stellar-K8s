/**
 * Auth Validator — validation engine + JSON payload generator for the Soroban
 * Smart Contract Auth Hierarchy Builder.
 *
 * Soroban's `require_auth` model roots every authorization at a signing
 * `Address`. That address authorizes exactly one direct (root) contract
 * invocation; the contract may itself trigger further nested invocations
 * (sub-invocations) against other contracts, to an arbitrary depth. This
 * module models that shape as an `AuthTreeNode` tree, validates it against
 * the structural rules a `SorobanAuthorizationEntry` must satisfy, and
 * serialises a valid tree into the JSON payload shape `soroban-cli` /
 * `stellar-sdk` expect before XDR-encoding and signing.
 *
 * Usage:
 *   import { validateAuthTree, buildAuthEntries } from './auth_validator';
 *   const result = validateAuthTree(roots);
 *   if (result.valid) {
 *     const entries = buildAuthEntries(roots);
 *   }
 */

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/** The two node kinds that can appear in an auth tree. */
export type AuthNodeKind = 'address' | 'contract';

/** Scalar arg types supported by the inline node editor / JSON exporter. */
export type AuthArgType =
  | 'address'
  | 'string'
  | 'symbol'
  | 'bytes'
  | 'bool'
  | 'u32'
  | 'u64'
  | 'i128';

/** A single named argument passed to a contract function invocation. */
export interface AuthArg {
  /** Argument name, shown in the UI and carried through to the JSON payload for readability. */
  name: string;
  /** ScVal type used to encode `value` when building the payload. */
  type: AuthArgType;
  /** Raw string form of the value as entered in the UI (numbers/bools included). */
  value: string;
}

/**
 * A node in the authorization tree.
 *
 * `kind: 'address'` nodes represent the signing account that authorizes a
 * root invocation — they may only appear at the top level of the tree.
 * `kind: 'contract'` nodes represent a contract function call; when nested
 * under another contract node they represent a sub-invocation triggered
 * during execution of the parent call.
 */
export interface AuthTreeNode {
  /** Unique identifier, stable across renders/drags. */
  id: string;
  kind: AuthNodeKind;
  /** Signing account (G... / C... address). Required when kind === 'address'. */
  address?: string;
  /** Contract address being invoked. Required when kind === 'contract'. */
  contractAddress?: string;
  /** Contract function name being invoked. Required when kind === 'contract'. */
  functionName?: string;
  /** Function arguments, in call order. */
  args?: AuthArg[];
  /** Child invocations. For an address root, direct children are root invocations. */
  children: AuthTreeNode[];
}

// ---------------------------------------------------------------------------
// Validation result types
// ---------------------------------------------------------------------------

export interface ValidationError {
  /** Machine-readable error code, e.g. "NO_ROOT_ADDRESS". */
  code: string;
  /** Human-readable description shown in the validation panel. */
  message: string;
  /** IDs of the nodes involved in this error. */
  nodeIds: string[];
}

export interface ValidationWarning {
  /** Machine-readable warning code, e.g. "DEEP_NESTING". */
  code: string;
  /** Human-readable description shown in the validation panel. */
  message: string;
  /** IDs of the nodes involved in this warning. */
  nodeIds: string[];
}

export interface ValidationResult {
  /** True only when there are zero errors (warnings are permitted). */
  valid: boolean;
  errors: ValidationError[];
  warnings: ValidationWarning[];
}

// ---------------------------------------------------------------------------
// JSON payload types (mirrors stellar-sdk's SorobanAuthorizationEntry shape)
// ---------------------------------------------------------------------------

export interface ScValJSON {
  type: AuthArgType;
  value: string | boolean;
}

export interface AuthorizedFunctionJSON {
  type: 'contract_fn';
  contractAddress: string;
  functionName: string;
  args: ScValJSON[];
}

export interface AuthorizedInvocationJSON {
  function: AuthorizedFunctionJSON;
  subInvocations: AuthorizedInvocationJSON[];
}

export interface SorobanAuthEntryJSON {
  credentials: {
    type: 'address';
    address: string;
    nonce: string;
    signatureExpirationLedger: number;
  };
  rootInvocation: AuthorizedInvocationJSON;
}

export interface BuildAuthEntriesOptions {
  /** Nonce generator; defaults to a monotonically increasing counter per address. */
  nonce?: (address: string, index: number) => string;
  /** Ledger sequence at which the signature expires; defaults to 0 (caller must set before signing). */
  signatureExpirationLedger?: number;
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/**
 * Soft recommended depth limit for a single root invocation's sub-invocation
 * chain. Not a protocol-enforced limit, but every extra frame adds CPU
 * instruction and auth-verification cost that counts against the
 * transaction's resource budget — deep trees are a common cause of
 * `resource limit exceeded` failures on testnet/mainnet.
 */
export const MAX_RECOMMENDED_DEPTH = 6;

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

function isBlank(value: string | undefined): boolean {
  return !value || value.trim().length === 0;
}

/** Depth of the deepest sub-invocation chain under `node` (0 = node itself). */
function subtreeDepth(node: AuthTreeNode): number {
  if (node.children.length === 0) return 0;
  return 1 + Math.max(...node.children.map(subtreeDepth));
}

// ---------------------------------------------------------------------------
// Validation rules — errors
// ---------------------------------------------------------------------------

/**
 * RULE 1 — ERROR: NO_ROOT_ADDRESS
 *
 * Every SorobanAuthorizationEntry is anchored to a signing Address. A tree
 * with no address-kind roots has nothing to export.
 */
function checkNoRootAddress(roots: AuthTreeNode[]): ValidationError | null {
  const addressRoots = roots.filter((n) => n.kind === 'address');
  if (addressRoots.length > 0) return null;
  return {
    code: 'NO_ROOT_ADDRESS',
    message:
      'The tree has no signing Address at its root. Add at least one Address node ' +
      'before configuring root invocations.',
    nodeIds: [],
  };
}

/**
 * RULE 2 — ERROR: ADDRESS_MISSING_VALUE
 *
 * An address root must carry the actual G.../C... account string that will
 * appear in the entry's credentials.
 */
function checkAddressMissingValue(roots: AuthTreeNode[]): ValidationError[] {
  return roots
    .filter((n) => n.kind === 'address' && isBlank(n.address))
    .map((n) => ({
      code: 'ADDRESS_MISSING_VALUE',
      message: `Address node "${n.id}" has no address value set.`,
      nodeIds: [n.id],
    }));
}

/**
 * RULE 3 — ERROR: ADDRESS_NO_ROOT_INVOCATION
 *
 * An address with zero children authorizes nothing — Soroban requires the
 * root invocation to be present for the entry to have any effect.
 */
function checkAddressNoRootInvocation(roots: AuthTreeNode[]): ValidationError[] {
  return roots
    .filter((n) => n.kind === 'address' && n.children.length === 0)
    .map((n) => ({
      code: 'ADDRESS_NO_ROOT_INVOCATION',
      message: `Address node "${n.id}" has no root contract invocation. Drag a Contract ` +
        'tile onto it to define what it authorizes.',
      nodeIds: [n.id],
    }));
}

/**
 * RULE 4 — ERROR: CONTRACT_INCOMPLETE
 *
 * A contract node must specify both the contract being called and the
 * function name — these become `InvokeContractArgs.contractAddress` and
 * `.functionName` in the exported payload.
 */
function checkContractIncomplete(node: AuthTreeNode, errors: ValidationError[]): void {
  if (node.kind === 'contract' && (isBlank(node.contractAddress) || isBlank(node.functionName))) {
    errors.push({
      code: 'CONTRACT_INCOMPLETE',
      message: `Contract node "${node.id}" is missing its contract address or function name.`,
      nodeIds: [node.id],
    });
  }
  for (const child of node.children) checkContractIncomplete(child, errors);
}

/**
 * RULE 5 — ERROR: ADDRESS_NOT_AT_ROOT
 *
 * Only the signing account may be an `address`-kind node; contracts cannot
 * have an Address as a sub-invocation target (require_auth checks a
 * contract, never re-nests another signer's authorization inside it).
 */
function checkAddressNotAtRoot(node: AuthTreeNode, errors: ValidationError[]): void {
  for (const child of node.children) {
    if (child.kind === 'address') {
      errors.push({
        code: 'ADDRESS_NOT_AT_ROOT',
        message: `Address node "${child.id}" is nested under "${node.id}". Address nodes ` +
          'may only appear at the top level of the tree.',
        nodeIds: [child.id, node.id],
      });
    }
    checkAddressNotAtRoot(child, errors);
  }
}

// ---------------------------------------------------------------------------
// Validation rules — warnings
// ---------------------------------------------------------------------------

/**
 * RULE 6 — WARNING: DEEP_NESTING
 *
 * Sub-invocation chains longer than MAX_RECOMMENDED_DEPTH risk exceeding the
 * transaction's CPU-instruction / auth-verification resource budget.
 */
function checkDeepNesting(roots: AuthTreeNode[]): ValidationWarning[] {
  const warnings: ValidationWarning[] = [];
  for (const root of roots) {
    if (root.kind !== 'address') continue;
    for (const child of root.children) {
      const depth = subtreeDepth(child);
      if (depth > MAX_RECOMMENDED_DEPTH) {
        warnings.push({
          code: 'DEEP_NESTING',
          message: `Root invocation "${child.id}" nests ${depth} levels deep, above the ` +
            `recommended ${MAX_RECOMMENDED_DEPTH}. Deep chains cost more CPU instructions ` +
            'and are more likely to exceed the transaction resource budget.',
          nodeIds: [child.id],
        });
      }
    }
  }
  return warnings;
}

/**
 * RULE 7 — WARNING: MISSING_ARGS
 *
 * A contract node with zero args is valid (many functions take none) but is
 * flagged so the author double-checks it wasn't left unconfigured.
 */
function checkMissingArgs(node: AuthTreeNode, warnings: ValidationWarning[]): void {
  if (node.kind === 'contract' && !isBlank(node.functionName) && (node.args ?? []).length === 0) {
    warnings.push({
      code: 'MISSING_ARGS',
      message: `Contract node "${node.id}" (${node.functionName}) has no arguments configured.`,
      nodeIds: [node.id],
    });
  }
  for (const child of node.children) checkMissingArgs(child, warnings);
}

/**
 * RULE 8 — WARNING: REENTRANT_CONTRACT_CALL
 *
 * The same contract address appearing twice along one root-to-leaf path
 * indicates a reentrant call. Soroban's runtime guards against unauthorized
 * reentrancy, but the pattern is unusual enough to surface explicitly.
 */
function checkReentrantCalls(
  node: AuthTreeNode,
  ancestry: string[],
  warnings: ValidationWarning[],
): void {
  if (node.kind === 'contract' && node.contractAddress) {
    if (ancestry.includes(node.contractAddress)) {
      warnings.push({
        code: 'REENTRANT_CONTRACT_CALL',
        message: `Contract "${node.contractAddress}" appears more than once in the same ` +
          `invocation chain (node "${node.id}"). Confirm this reentrant call is intentional.`,
        nodeIds: [node.id],
      });
    }
  }
  const nextAncestry =
    node.kind === 'contract' && node.contractAddress
      ? [...ancestry, node.contractAddress]
      : ancestry;
  for (const child of node.children) checkReentrantCalls(child, nextAncestry, warnings);
}

/**
 * RULE 9 — WARNING: DUPLICATE_SIBLING_CALL
 *
 * Two sibling contract nodes invoking the same contract + function under the
 * same parent are usually a copy/paste mistake in the builder.
 */
function checkDuplicateSiblingCalls(node: AuthTreeNode, warnings: ValidationWarning[]): void {
  const seen = new Map<string, string>();
  for (const child of node.children) {
    if (child.kind !== 'contract' || !child.contractAddress || !child.functionName) continue;
    const key = `${child.contractAddress}#${child.functionName}`;
    const existingId = seen.get(key);
    if (existingId) {
      warnings.push({
        code: 'DUPLICATE_SIBLING_CALL',
        message: `Nodes "${existingId}" and "${child.id}" both call ` +
          `${child.contractAddress}.${child.functionName} under the same parent.`,
        nodeIds: [existingId, child.id],
      });
    } else {
      seen.set(key, child.id);
    }
  }
  for (const child of node.children) checkDuplicateSiblingCalls(child, warnings);
}

// ---------------------------------------------------------------------------
// Public API — validation
// ---------------------------------------------------------------------------

/**
 * Validates an authorization tree against Soroban's structural requirements.
 *
 * **Errors** (block JSON export):
 * 1. `NO_ROOT_ADDRESS`         — tree has no Address root.
 * 2. `ADDRESS_MISSING_VALUE`   — an Address root has no address string set.
 * 3. `ADDRESS_NO_ROOT_INVOCATION` — an Address root has no children.
 * 4. `CONTRACT_INCOMPLETE`     — a contract node is missing address/function.
 * 5. `ADDRESS_NOT_AT_ROOT`     — an Address node appears nested under a contract.
 *
 * **Warnings** (informational, export still allowed):
 * 6. `DEEP_NESTING`            — a root invocation nests deeper than recommended.
 * 7. `MISSING_ARGS`            — a contract node has zero configured args.
 * 8. `REENTRANT_CONTRACT_CALL` — the same contract repeats along one call chain.
 * 9. `DUPLICATE_SIBLING_CALL`  — two siblings call the same contract + function.
 *
 * @param roots - Top-level nodes of the tree (expected to all be `kind: 'address'`).
 */
export function validateAuthTree(roots: AuthTreeNode[]): ValidationResult {
  const errors: ValidationError[] = [];
  const noRoot = checkNoRootAddress(roots);
  if (noRoot) errors.push(noRoot);
  errors.push(...checkAddressMissingValue(roots));
  errors.push(...checkAddressNoRootInvocation(roots));
  for (const root of roots) {
    checkContractIncomplete(root, errors);
    checkAddressNotAtRoot(root, errors);
  }

  const warnings: ValidationWarning[] = [];
  warnings.push(...checkDeepNesting(roots));
  for (const root of roots) {
    checkMissingArgs(root, warnings);
    checkReentrantCalls(root, [], warnings);
    checkDuplicateSiblingCalls(root, warnings);
  }

  return { valid: errors.length === 0, errors, warnings };
}

// ---------------------------------------------------------------------------
// Public API — JSON payload generation
// ---------------------------------------------------------------------------

/** Converts one AuthArg into the ScVal-shaped JSON used in the payload. */
export function scValFromArg(arg: AuthArg): ScValJSON {
  if (arg.type === 'bool') {
    return { type: 'bool', value: arg.value === 'true' };
  }
  return { type: arg.type, value: arg.value };
}

function buildInvocation(node: AuthTreeNode): AuthorizedInvocationJSON {
  return {
    function: {
      type: 'contract_fn',
      contractAddress: node.contractAddress ?? '',
      functionName: node.functionName ?? '',
      args: (node.args ?? []).map(scValFromArg),
    },
    subInvocations: node.children
      .filter((c) => c.kind === 'contract')
      .map(buildInvocation),
  };
}

/**
 * Serialises a validated auth tree into the JSON payload shape expected by
 * `soroban-cli` / `stellar-sdk` prior to XDR encoding and signing. Field
 * names (`credentials`, `rootInvocation`, `subInvocations`, `contractAddress`,
 * `functionName`, `args`) mirror `stellar-sdk`'s `SorobanAuthorizationEntry`
 * / `SorobanAuthorizedInvocation` object shape so the output can be mapped
 * field-for-field onto `xdr.SorobanAuthorizationEntry.fromXDR` construction.
 *
 * One entry is produced per (Address root, direct child) pair — an address
 * with multiple root invocations yields multiple entries sharing the same
 * credentials, matching how a single transaction can carry several
 * `SorobanAuthorizationEntry` values for the same signer.
 *
 * Callers should run `validateAuthTree(roots)` first; this function does not
 * re-validate and will happily serialise blank fields from an invalid tree.
 *
 * @param roots - Top-level Address nodes of the tree.
 * @param opts  - Optional nonce generator and signature expiration ledger.
 */
export function buildAuthEntries(
  roots: AuthTreeNode[],
  opts: BuildAuthEntriesOptions = {},
): SorobanAuthEntryJSON[] {
  const entries: SorobanAuthEntryJSON[] = [];
  let counter = 0;

  for (const root of roots) {
    if (root.kind !== 'address') continue;
    for (const child of root.children) {
      if (child.kind !== 'contract') continue;
      const index = counter++;
      entries.push({
        credentials: {
          type: 'address',
          address: root.address ?? '',
          nonce: opts.nonce ? opts.nonce(root.address ?? '', index) : String(index),
          signatureExpirationLedger: opts.signatureExpirationLedger ?? 0,
        },
        rootInvocation: buildInvocation(child),
      });
    }
  }

  return entries;
}
