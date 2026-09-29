# P2P Node Identity & Secrets Rotation via GitOps

> **Issue #290** · Complexity: High (200 Points)

---

## Overview

Long-lived validator seed keys are cryptographic liabilities. This document
describes how Stellar-K8s automates the rotation of ed25519 Stellar node
identities using GitOps principles, the External Secrets Operator (ESO), and
Kubernetes-native controls — **without manual downtime or quorum disruption**.

### Goals

| Goal | How |
|------|-----|
| Eliminate long-lived static seeds | 30-day automated rotation policy |
| GitOps-driven secret lifecycle | ESO syncs secrets from AWS/Vault into K8s |
| Zero quorum disruption | Staggered rotation with SCP ≥ 2/3 safety invariant |
| Horizon DB consistency | Automatic ingestion-worker key update |
| Audit trail | Fingerprint-only logs — raw seed never logged |

---

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│  External Secret Stores                                 │
│  ┌──────────────────┐   ┌────────────────────────────┐ │
│  │  AWS Secrets     │   │  HashiCorp Vault KV-v2     │ │
│  │  Manager         │   │  (Kubernetes auth)         │ │
│  └────────┬─────────┘   └──────────────┬─────────────┘ │
└───────────┼──────────────────────────────┼──────────────┘
            │  ESO pulls every 1h          │
            ▼                              ▼
┌──────────────────────────────────────────────────────────┐
│  Kubernetes Cluster                                      │
│  ┌──────────────────────────────────────────────────┐   │
│  │  External Secrets Operator                       │   │
│  │  ExternalSecret CRs → K8s Secrets               │   │
│  └─────────────────────┬────────────────────────────┘   │
│                        │ writes                         │
│                        ▼                                │
│  ┌──────────────────────────────────────────────────┐   │
│  │  K8s Secrets (etcd, encrypted at rest)           │   │
│  │  validator-{0..4}-seed  { seed, public_key }     │   │
│  └─────────────────────┬────────────────────────────┘   │
│                        │ watches (label selector)       │
│                        ▼                                │
│  ┌──────────────────────────────────────────────────┐   │
│  │  Stellar-K8s Operator                            │   │
│  │  EsoSecretWatcher → fingerprint diff             │   │
│  │       │                                          │   │
│  │  ClusterRotationController                       │   │
│  │  • Preflight quorum check                        │   │
│  │  • Staggered per-node rotation                   │   │
│  │  • Quorum gate between nodes                     │   │
│  │       │                                          │   │
│  │  NodeRotationWorker (per node)                   │   │
│  │  1. Fetch identity  5. Wait for rejoin           │   │
│  │  2. Un-peer         6. Update Horizon DB         │   │
│  │  3. Inject seed     7. Rollback on failure       │   │
│  │  4. Patch ConfigMap                              │   │
│  └──────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────┘
```

---

## Key Modules

### `controller/src/security/eso_integration.rs`

| Type | Description |
|------|-------------|
| `EsoBackend` | Trait abstraction over any ESO-compatible secret backend |
| `AwsEsoBackend` | AWS Secrets Manager via IRSA/static credentials |
| `VaultEsoBackend` | HashiCorp Vault KV-v2 via Kubernetes auth |
| `EsoNodeIdentity` | Parsed identity: public key + SHA-256 fingerprint (seed redacted) |
| `EsoIntegrationConfig` | Operator-level ESO wiring configuration |

**Transit security**: The raw seed travels `External Store → ESO pod (TLS) →
etcd (encrypted at rest) → operator pod memory`. It is **never logged**; only
the fingerprint and public key appear in log entries and status fields. The
`fmt::Debug` implementation for `EsoNodeIdentity` explicitly redacts the seed.

### `controller/src/security/rotation.rs`

| Type | Description |
|------|-------------|
| `QuorumParameters` | Computes SCP ⌈2/3 · n⌉ threshold and max simultaneous rotating |
| `NodeRotationWorker` | Single-node lifecycle: un-peer → inject → ConfigMap → rejoin |
| `ClusterRotationController` | Staggered cluster-wide rotation with quorum gating |
| `RotationScheduler` | Time-based trigger (rotation period in days) |
| `NodeIdentityRotationConfig` | Full configuration for the rotation controller |
| `HorizonDbOps` | Trait for updating Horizon PostgreSQL after rotation |

---

## Quorum Safety Invariant

For a cluster of `n` validators, SCP requires ≥ ⌈2/3 · n⌉ nodes to agree on
each ledger. The rotation controller enforces:

```
threshold        = ⌈2/3 × n⌉
max_rotating     = n − threshold
```

### Example — 5-node testnet cluster

| n | threshold | max simultaneously rotating |
|---|-----------|------------------------------|
| 5 | 4 | **1** (one at a time) |
| 3 | 2 | 1 |
| 7 | 5 | 2 |

The controller **aborts** remaining rotations if
`healthy_count − 1 < threshold` before any node's turn, preserving
liveness for the rest of the cluster.

---

## Configuration

### StellarNode CR

```yaml
spec:
  validatorConfig:
    seedSecretRef: "validator-0-seed"
    nodeIdentityRotation:
      enabled: true
      secretPath: "stellar/validators/validator-0"
      rotationPeriodDays: 30
      interNodeDelaySeconds: 120     # wait between successive nodes
      rejoinTimeoutSeconds: 300      # max time to rejoin consensus
      minAuthenticatedPeers: 1
      rollbackOnFailure: true
      updateHorizonDb: false
```

### Operator-level ESO config

```yaml
esoIntegration:
  enabled: true
  aws:
    region: us-east-1
    roleArn: arn:aws:iam::ACCOUNT_ID:role/stellar-node-identity-reader
    seedField: seed
    timeoutSecs: 10
  rotationPeriodDays: 30
  versionRetention: 3
```

---

## GitOps Workflow

### 1. Provision the seed in the external store

**AWS Secrets Manager**

```bash
aws secretsmanager create-secret \
  --name stellar/validators/validator-0 \
  --secret-string '{
    "seed": "SCZANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZGVQSKTXJCQC4D3BBZDEXM",
    "public_key": "GABC..."
  }'
```

**HashiCorp Vault**

```bash
vault kv put secret/stellar/validators/validator-0 \
  seed="SCZANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZGVQSKTXJCQC4D3BBZDEXM" \
  public_key="GABC..."
```

### 2. Apply the ESO manifests

```bash
kubectl apply -f examples/security/node-identity-eso-external-secrets.yaml
```

Verify ESO materialised the secrets:

```bash
kubectl get secret validator-0-seed -n stellar \
  -o jsonpath='{.data.public_key}' | base64 -d
```

### 3. Trigger rotation (manual or automated)

Store a new seed version in the external store. ESO detects the change
within its `refreshInterval` (default: 1h), updates the K8s Secret, and
the operator detects the fingerprint change and initiates rotation.

```bash
# AWS — put a new version
aws secretsmanager put-secret-value \
  --secret-id stellar/validators/validator-0 \
  --secret-string '{"seed":"SNEWSEED...","public_key":"GNEWPUBKEY..."}'

# Vault — put a new KV version
vault kv put secret/stellar/validators/validator-0 \
  seed="SNEWSEED..." \
  public_key="GNEWPUBKEY..."
```

### 4. Automated 30-day policy

With `rotationPeriodDays: 30`, configure your secret management platform to
rotate the seed every 30 days. ESO synchronises the new value and the
operator rotates the cluster automatically, one node at a time.

---

## Rotation Lifecycle (per node)

```
1. Preflight check     — cluster has ≥ threshold healthy nodes?
       ↓ yes
2. Fetch new identity  — pull from EsoBackend; skip if fingerprint unchanged
       ↓ changed
3. Read current seed   — store previous seed for rollback
       ↓
4. Un-peer node        — graceful peer disconnect
       ↓
5. Inject new seed     — write to K8s Secret → Stellar Core config-reload
       ↓ failure? → rollback + abort
6. Patch ConfigMap     — update stellar-core.cfg (NODE_SEED, public key)
       ↓ failure? → rollback + abort
7. Wait for rejoin     — poll consensus_info until synced + min_peers
       ↓ timeout? → rollback
8. Horizon DB update   — UPDATE accounts/signers with new public key (optional)
       ↓
9. Inter-node delay    — wait interNodeDelaySeconds before next node
```

---

## Security Review: Transit Pathways

| Segment | Mechanism | Notes |
|---------|-----------|-------|
| External Store → ESO pod | TLS 1.2+ (AWS SDK / Vault HTTPS) | mTLS optional for Vault |
| ESO pod → Kubernetes API | In-cluster TLS | Least-privilege ServiceAccount |
| Kubernetes API → etcd | AES-256 or KMS encryption at rest | Operator-configured |
| etcd → Operator pod | In-cluster TLS | Read via K8s API only |
| Operator pod memory | Rust `String` | Dropped after Secret write; never logged |
| Log entries | Fingerprint + public key only | `Debug` for `EsoNodeIdentity` redacts seed |
| ConfigMap | Public key only | Seed is stored only in the K8s Secret |

**Recommendations**:

- Enable [Kubernetes Secret encryption at rest](https://kubernetes.io/docs/tasks/administer-cluster/encrypt-data/) with a KMS provider.
- Use namespace-scoped `SecretStore` (not `ClusterSecretStore`) to limit blast radius.
- Restrict RBAC: the operator ServiceAccount should only `get`/`watch` the specific
  node-identity Secrets by `resourceNames`.
- Enable Vault audit logging and AWS CloudTrail to record every `GetSecretValue` call.
- Set a short `refreshInterval` on ESO (1h) with a longer `rotationPeriodDays` (30)
  on the operator — ESO re-reads frequently but the operator only rotates when the
  fingerprint actually changes.

---

## Horizon Database Update

When `updateHorizonDb: true`, the controller calls `HorizonDbOps::update_node_key`,
which updates the Horizon PostgreSQL instance so ingestion workers track the
correct validator identity after rotation.

```sql
-- Conceptual update executed by HorizonDbOps
UPDATE accounts SET account_id = $new_pubkey WHERE account_id = $old_pubkey;
UPDATE signers  SET account_id = $new_pubkey WHERE account_id = $old_pubkey;
```

This step is **non-fatal**: if it fails, the validator is already using its new
identity and a warning is logged. Operators should monitor for
`HorizonDbUpdateFailed` log events and reconcile manually if needed.

---

## Observability

### Prometheus Metrics

```
stellar_node_identity_rotation_total{namespace,node,outcome}
stellar_node_identity_rotation_duration_seconds{namespace,node}
stellar_node_identity_rotation_last_success_timestamp{namespace,node}
stellar_node_identity_rotation_quorum_gate_aborts_total{cluster}
stellar_eso_secret_refresh_total{namespace,node,backend}
stellar_eso_secret_refresh_errors_total{namespace,node,backend}
stellar_eso_identity_age_seconds{namespace,node}
```

### Structured Log Events

```json
{"level":"INFO","node":"validator-0","fingerprint":"a1b2c3…","version":"aabb-1234","msg":"Fetched new identity from ESO backend"}
{"level":"INFO","node":"validator-0","msg":"Node un-peered"}
{"level":"INFO","node":"validator-0","fingerprint":"a1b2c3…","msg":"New identity injected into K8s Secret"}
{"level":"INFO","node":"validator-0","ledger":1001,"peers":4,"msg":"Node rejoined consensus"}
{"level":"INFO","cluster":"testnet","rotated":5,"skipped":0,"failed":0,"msg":"Cluster rotation completed"}
```

---

## 30-day Testnet Validation

Satisfies the Definition of Done:

1. Deploy 5 validators with `nodeIdentityRotation.enabled: true`, `rotationPeriodDays: 30`.
2. Store seeds in AWS Secrets Manager / Vault using the JSON format above.
3. Apply `examples/security/node-identity-eso-external-secrets.yaml`.
4. After 30 days (or by updating the secret), the operator automatically rotates
   all 5 nodes **one at a time**.
5. Ledger advancement continues throughout — the quorum gate ensures
   `healthy − rotating ≥ 4` at all times for a 5-node cluster.
6. `ClusterRotationReport.is_fully_successful()` returns `true` when all 5 nodes rotated.

---

## Related Files

| File | Description |
|------|-------------|
| `controller/src/security/eso_integration.rs` | ESO backend trait + AWS + Vault implementations |
| `controller/src/security/rotation.rs` | Staggered rotation controller + quorum safety |
| `controller/src/security/mod.rs` | Module re-exports |
| `examples/security/node-identity-eso-external-secrets.yaml` | Ready-to-apply ESO manifests |
| `docs/secret-rotation.md` | General secret rotation docs |
| `docs/security/credentials-and-secrets.md` | Credentials management guide |

---

## References

- [External Secrets Operator](https://external-secrets.io/latest/)
- [Stellar SCP White Paper](https://www.stellar.org/papers/stellar-consensus-protocol)
- [AWS Secrets Manager Rotation](https://docs.aws.amazon.com/secretsmanager/latest/userguide/rotating-secrets.html)
- [HashiCorp Vault Kubernetes Auth](https://developer.hashicorp.com/vault/docs/auth/kubernetes)
- [Kubernetes — Encrypting Secret Data at Rest](https://kubernetes.io/docs/tasks/administer-cluster/encrypt-data/)
