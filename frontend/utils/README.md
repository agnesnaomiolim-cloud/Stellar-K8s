# frontend/utils

Shared utility modules for the Stellar-K8s frontend tooling.

---

## manifest_builder.ts

Generates valid Kubernetes YAML manifests for `StellarNode` custom resources directly from the topology configurator's in-memory state, without any external YAML serialisation library.

### Exported API

| Export | Signature | Description |
|---|---|---|
| `buildManifests` | `(state: TopologyState, namespace?: string) => string` | Produces concatenated YAML for every placed node in the topology, with documents separated by `---`. Each node emits a `StellarNode` CRD manifest and a matching `PodDisruptionBudget`. |
| `buildNodeManifest` | `(node: PlacedStellarNode, zones: AvailabilityZone[], workerNodes: WorkerNode[]) => string` | Builds a single `StellarNode` YAML manifest for one placed node. Includes resource requests/limits, storage config, topology spread constraints, optional pod anti-affinity, and optional validator config. |
| `buildPodDisruptionBudget` | `(node: PlacedStellarNode) => string` | Generates a `PodDisruptionBudget` YAML manifest that enforces `minAvailable` for the node's pods. |
| `escapeYaml` | `(str: string) => string` | Normalises line endings and ensures a single trailing newline for safe embedding in YAML literal block scalars (`\|`). |

### Generated manifest structure

**StellarNode** (`stellar.org/v1alpha1`):
- `metadata.labels` always includes `app.kubernetes.io/managed-by: topology-configurator`
- `spec.topologySpreadConstraints` — one entry per zone, `whenUnsatisfiable: DoNotSchedule`
- `spec.affinity.podAntiAffinity` — `Hard` (required) or `Soft` (preferred, weight 100); omitted when set to `None`
- `spec.validatorConfig` — only present for `Validator` node types; includes quorum set as a YAML block scalar when defined

**PodDisruptionBudget** (`policy/v1`):
- Named `<node-name>-pdb`
- `spec.minAvailable` taken from the node's `minAvailable` field
- `spec.selector.matchLabels.app` matches the node's DNS-safe name

### Usage

```ts
import { buildManifests } from '../utils/manifest_builder';
import { useTopology } from './configurator/src/topology_builder/topology_store';

const [state] = useTopology();
const yaml = buildManifests(state, 'stellar-production');

// Write to a file, copy to clipboard, etc.
navigator.clipboard.writeText(yaml);
```

### Design notes

- All YAML is assembled from template literals for deterministic output with no runtime dependencies.
- Node names are normalised to valid Kubernetes DNS labels (lowercase, hyphens only).
- The `namespace` parameter in `buildManifests` overrides each node's individual namespace, making it easy to target staging vs. production clusters from the same topology definition.

---

## auth_validator.ts

Validation engine + JSON payload generator for the Soroban Smart Contract Auth Hierarchy Builder (`frontend/builder/auth_tree/`). Models a `require_auth` invocation tree — a signing `Address` authorizing a root contract call, which may trigger arbitrarily deep nested sub-invocations — and serialises a valid tree into the JSON shape `soroban-cli` / `stellar-sdk` expect prior to XDR encoding.

### Exported API

| Export | Signature | Description |
|---|---|---|
| `validateAuthTree` | `(roots: AuthTreeNode[]) => ValidationResult` | Runs 5 structural error checks and 4 warning checks against a forest of Address-rooted trees. |
| `buildAuthEntries` | `(roots: AuthTreeNode[], opts?: BuildAuthEntriesOptions) => SorobanAuthEntryJSON[]` | Serialises the tree into one `SorobanAuthorizationEntry`-shaped JSON object per (Address, direct child) pair. Does not re-validate — call `validateAuthTree` first. |
| `scValFromArg` | `(arg: AuthArg) => ScValJSON` | Converts one UI-entered argument into its ScVal-shaped JSON form (`bool` becomes a real boolean; other scalar types pass through as strings). |

### Validation rules

**Errors** (block JSON export):
1. `NO_ROOT_ADDRESS` — the tree has no Address root.
2. `ADDRESS_MISSING_VALUE` — an Address root has no address string set.
3. `ADDRESS_NO_ROOT_INVOCATION` — an Address root has no children.
4. `CONTRACT_INCOMPLETE` — a contract node is missing its contract address or function name.
5. `ADDRESS_NOT_AT_ROOT` — an Address node is nested under a contract.

**Warnings** (informational, export still allowed):
6. `DEEP_NESTING` — a root invocation nests deeper than `MAX_RECOMMENDED_DEPTH` (6).
7. `MISSING_ARGS` — a contract node has zero configured arguments.
8. `REENTRANT_CONTRACT_CALL` — the same contract address repeats along one call chain.
9. `DUPLICATE_SIBLING_CALL` — two sibling nodes call the same contract + function.

### JSON payload shape

Field names (`credentials`, `rootInvocation`, `subInvocations`, `function.contractAddress`, `function.functionName`, `args`) mirror `stellar-sdk`'s `SorobanAuthorizationEntry` / `SorobanAuthorizedInvocation` object shape, so the output maps field-for-field onto `xdr.SorobanAuthorizationEntry` construction. One entry is produced per direct child of an Address root — an address with multiple root invocations yields multiple entries sharing the same `credentials`.

### Usage

```ts
import { validateAuthTree, buildAuthEntries } from '../utils/auth_validator';
import { useAuthTree } from './builder/auth_tree/src/auth_tree_store';

const [state] = useAuthTree();
const result = validateAuthTree(state.roots);
if (result.valid) {
  const entries = buildAuthEntries(state.roots, { signatureExpirationLedger: currentLedger + 300 });
  // Hand `entries` to the SDK/CLI step that maps them onto real ScVal + XDR construction and signs.
}
```
