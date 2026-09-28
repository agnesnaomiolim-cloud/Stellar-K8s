# Secure Validator Seed Generation and Sharding

> Shamir's Secret Sharing (SSS) custody for validator seeds, HashiCorp Vault Transit
> assembly and Kubernetes injection, and air-gapped generation/backup ceremony.
>
> **See also:** [Credentials & Secrets (Central Reference)](credentials-and-secrets.md)
> for the full index of secret-management documentation.
>
> Tracking: Closes #251

A validator seed (`STELLAR_SEED`) is the ed25519 secret that signs every consensus
message a node emits. It is the single most valuable object an operator holds: theft
means an attacker can equivocate on your behalf, and loss means the validator identity
is gone and any accumulated stake/trust with peers must be re-established. This guide
describes how to generate that seed offline and split it so that **no single person,
host, backup, or cloud account can ever reconstruct it**.

Companion tooling: [`examples/scripts/shamir-split.py`](../../examples/scripts/shamir-split.py)
(standard-library only, safe to copy to an air-gapped host).

---

## 1. Scope and threat model

### 1.1 What is protected

| Asset | Description |
|---|---|
| Validator seed | 32-byte ed25519 secret, encoded as Stellar strkey `S...` |
| Public identity | ed25519 public key, strkey `G...` — published, not secret |
| Shares | `k`-of-`n` Shamir shares of the seed; **not** individually sensitive |

### 1.2 Threats addressed

| Threat | Mitigation in this guide |
|---|---|
| Single custodian is compromised or coerced | `k`-of-`n` threshold; any `k-1` shares reveal nothing |
| A disk, backup, laptop or safe is exfiltrated | Plaintext seed never persists on an internet-connected host |
| Cloud account / object store is breached | Shares are Transit-encrypted ciphertext at rest |
| Backup media is stolen in transit | Share ciphertext is useless without the Transit key |
| Insiders with `kubectl get secret` | Seed is assembled just-in-time and never stored in etcd (`csiRef`) |
| Transcription error during a ceremony | Share checksum + mandatory public-key verification on recovery |
| A crashed process leaks the seed to swap/core | `mlock` / core-dump suppression (best effort) and no disk I/O |

### 1.3 Non-goals

* **Threshold signing.** Stellar Core requires a single complete ed25519 secret at
  signing time; SSS reduces *at-rest* risk, it does not make signing threshold-based.
  The seed must be reassembled in one process's memory to run a validator.
* **HSM-backed key custody.** Where the platform supports it, a PKCS#11/HSM-backed
  signer is preferred over holding any seed material at all. SSS is the fallback for
  operators who must run unmodified `stellar-core`.
* **Constant-time execution.** The reference implementation is pure Python and is
  **not** constant-time. It is intended for dedicated, offline or tightly controlled
  hosts where an attacker cannot co-resident-measure timing. Do not expose it as a
  network service.

### 1.4 Design invariants

1. **I1 — No plaintext on disk.** On any internet-connected machine the seed exists
   only in process memory (and nowhere at all while at rest). The script reads it from
   stdin, an inherited fd, or an environment variable, and refuses a file path.
2. **I2 — No single point of reconstruction.** Any `k-1` shares reveal zero information
   about the seed (information-theoretic security, Shamir 1979).
3. **I3 — Verifiable recovery.** Every reconstruction is checked against the published
   `G...` public key; a wrong or mixed share set fails closed.
4. **I4 — Auditable assembly.** Share decryption and seed assembly are logged and
   alertable events, not routine background activity.
5. **I5 — Encrypted shares at rest.** Shares are stored as Vault Transit ciphertext
   (`vault:v1:...`), never as plaintext base32.

---

## 2. Shamir's Secret Sharing design

### 2.1 Parameters

The script shares each seed byte independently over **GF(2⁸)** with the AES reduction
polynomial `x⁸+x⁴+x³+x+1` (`0x11B`). A byte `b` becomes the constant term of a random
degree-`k-1` polynomial; share `x ∈ {1..n}` is the polynomial evaluated at `x`. Any `k`
of the `n` evaluation points interpolate `b` back at `x = 0`; `k-1` points leave every
candidate `b` equally likely.

| Parameter | Allowed | Recommended | Notes |
|---|---|---|---|
| `k` (threshold) | 2–255 | 3 (small teams) / 5 (institutions) | Number of shares needed |
| `n` (shares) | `k`–255 | 5 / 9 | Number of custodians |
| Seed length | exactly 32 bytes | 32 | ed25519 |

Guidance:

* Never set `k = 1` — the script rejects it — and never store two shares with the same
  custodian, the same legal entity, or the same physical site.
* Prefer `n - k ≥ 2`: it tolerates two simultaneous custodian losses before the identity
  becomes unrecoverable.
* Every coefficient is drawn from `secrets.randbelow(256)` (the OS CSPRNG). Never reuse a
  polynomial across two secrets.

### 2.2 Share format

```
SSS1.<split-id>.<k>.<n>.<x>.<base64url(y)>.<chk16>
```

| Field | Meaning |
|---|---|
| `SSS1` | Format/version tag |
| `split-id` | Random per-split identifier; rejects mixed share sets |
| `k`, `n` | Threshold and total share count |
| `x` | Public evaluation point, `1..n` |
| `base64url(y)` | The share payload (one GF(2⁸) byte per seed byte) |
| `chk16` | First 16 hex chars of `SHA-256(canonical share)` |

> **The checksum is not a MAC.** It detects transcription errors (paper, OCR, serial
> console) and nothing else. Shamir shares carry no authentication by construction: a
> malicious holder of a share can corrupt a reconstruction. Integrity of the recovered
> identity comes from invariant **I3** — the ed25519 public key must match the published
> `G...` account id before the seed is used. A failed check means *discard and
> re-ceremony*, never "retry with different shares".

Example (do not use):

```
SSS1.3rTk9pQ2.3.5.3.aGVsbG8td29ybGQtZXhhbXBsZS1ieXRlcw.9f1c0d7e3a5b8264
```

### 2.3 Exposure model

| Shares held by adversary | Information gained |
|---|---|
| 0 | Nothing |
| 1 … k-1 | **Nothing** (not "less", nothing) |
| k | The complete seed |

This is the property that makes SSS preferable to "encrypt a copy per custodian":
encryption-based custody degrades to the weakest passphrase and every copy is a full
compromise if broken, whereas a stolen share is inert.

---

## 3. Air-gapped generation and backup ceremony

Run the entire ceremony on a **dedicated offline machine**. A validated live-USB OS
(e.g. an amnesiac distribution booted read-only), no configured network interfaces, and
a witness who signs the record are appropriate controls for a Tier-0 key.

### 3.1 Prerequisites

* Offline host: no NIC link, radios disabled, no cloud sync, no swap file.
* The script is copied in over read-only media (or `git archive` of this repo).
* `python3` only — the script has **no** third-party dependencies.
* Blank, labelled, tamper-evident media/safes for each custodian.
* A printed record with the future `G...` account id (filled in during the ceremony).

### 3.2 Steps

**Step 1 — Generate and split without ever writing the seed.**

```bash
# Offline host. The seed is piped from one process's memory into the next;
# no intermediate file is created. Shares are written to removable media.
python3 examples/scripts/shamir-split.py gen --out-format hex \
  | python3 examples/scripts/shamir-split.py split \
      -k 3 -n 5 --in-format hex \
      --out-dir /media/ceremony/shares --allow-disk
```

`gen` writes the public identity and a SHA-256 fingerprint of the seed to **stderr**;
`splits` writes the 5 share strings to `0600` files and prints the derived
`account_id=G...` to stderr. Record the `account_id` by hand as the validator's
published key.

> `--allow-disk` is an explicit acknowledgement that this host is the **air-gapped
> generation host**. Never pass it on an internet-connected machine.

**Step 2 — Verify the shares on the same offline host, before sealing.**

```bash
cat /media/ceremony/shares/share-0{1,3,5}-of-5.txt \
  | python3 examples/scripts/shamir-split.py recover \
      --expect-pubkey GDLVVGABQKYQVN6VJP7NHSLEA45A5YLS6PNKMIZFV4BBU2HXA5IRVHUR \
      --no-emit
# -> VERIFICATION OK: recovered seed matches expected public key
```

This is the acceptance test for the ceremony: a 3-of-5 reconstruction must reproduce
the **identical** ed25519 public key. If it does not, destroy the media and restart
from Step 1.

**Step 3 — Seal and distribute.**

* One share per custodian, each on separate media, in a separate tamper-evident bag.
* Record on the bag: share index `x`, split id, `k`/`n`, ceremony date, and both
  witnesses. Record the `x → custodian` mapping in the custody register (never on the
  share itself).
* Optionally encrypt each share with Vault Transit before writing it (see §4.1) so the
  physical medium alone is inert.
* Distribute through different couriers/geographies. Never transport `k` shares together.

**Step 4 — Destroy the workspace.**

* `poweroff` the host (zeroes volatile memory), remove the live-USB, and witness the
  destruction of any scratch media.
* Never keep a "convenience copy" of the seed, nor a `k`-of-`n` reconstruction, on any
  online host.

### 3.3 Backup and rotation

* Keep **two** separate physical copies of each share, in independent safes/vaults, one
  of which is geographically remote. Since `k` shares reconstruct, treat any two copies
  of the *same* share as one share for `k`-counting purposes — and keep the copies of a
  share in a way that a single theft cannot capture `k` distinct shares.
* Paper/steel backup of the share string is acceptable (a share is not secret); paper
  backup of the **seed** is not.
* Re-run the ceremony (new split id, new randomness) on:
  * custodian join/leave or a change in `k`/`n`;
  * any suspicion of share compromise;
  * a scheduled interval (e.g. every 24 months) to bound the value of a long-held share.
  Re-splitting requires a full reconstruction ceremony on the offline host.

---

## 4. Vault Transit integration and dynamic assembly

Vault never stores the seed; it stores **share ciphertext** and holds the key material
that decrypts it. Assembly is a short-lived, audited job that reconstructs the seed in
memory and hands it to the cluster — the seed is never a durable object in Vault KV, on
a node's disk, or in etcd.

### 4.1 Encrypting shares with Transit

```bash
vault secrets enable transit
vault write -f transit/keys/validator-share type=aes256-gcm96

# One ciphertext per share; store the output, never the input.
vault write -field=ciphertext transit/encrypt/validator-share \
  plaintext="$(printf '%s' "$SHARE_STRING" | base64)"
# -> vault:v1:8SDd3WHDOjf7mq69CyCqYjBXAiQQAVZRkFM13ok481zoCmHnSeDX9vyhz7MMWSwa
```

Decryption requires `update` capability on `transit/decrypt/validator-share`:

```hcl
# policy: seed-assembler
path "transit/decrypt/validator-share" {
  capabilities = ["update"]
}
path "secret/data/stellar/shares/*" {
  capabilities = ["read"]
}
# Deny everything else by default — no list, no create, no delete on share paths.
```

### 4.2 Preserving the threshold across security domains

A single Transit key that can decrypt *all* shares reintroduces a single point of
failure — whoever controls the key can reconstruct the seed from the stored ciphertexts.
Preserve the `k`-of-`n` property operationally:

* Split shares across **independent Vault namespaces or clusters** — ideally one per
  custody domain, each with its own Transit key, policies, auditors, and unseal quorum —
  so no single Vault administrator can decrypt `k` shares.
* Bind the assembly role to a **dedicated Kubernetes ServiceAccount** and a short TTL
  (`ttl=15m`, `max_ttl=1h`).
* Keep Vault's own unseal keys under a quorum that is **disjoint** from the share
  custodians. Co-locating both quorums means one seizure reconstitutes everything.
* Use response wrapping (`vault kv get -wrap-ttl=120s`) for any human-driven step so an
  approval yields a single-use, expiring token instead of a durable credential.

```bash
vault write auth/kubernetes/role/seed-assembler \
  bound_service_account_names=seed-assembler \
  bound_service_account_namespaces=stellar-system \
  policies=seed-assembler \
  ttl=15m max_ttl=1h
```

### 4.3 Just-in-time assembly job

The job logs in with its ServiceAccount token, pulls exactly `k` share ciphertexts,
decrypts and reconstructs them in memory, then publishes the assembled seed to the path
the operator already consumes. No volume is mounted, `readOnlyRootFilesystem` is on, and
the container never opens a file for the secret.

```yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: seed-assembler
  namespace: stellar-system
---
apiVersion: batch/v1
kind: Job
metadata:
  name: seed-assembler
  namespace: stellar-system
spec:
  backoffLimit: 0
  ttlSecondsAfterFinished: 300          # remove the (empty) pod record quickly
  template:
    metadata:
      labels: { app: seed-assembler }
    spec:
      serviceAccountName: seed-assembler
      automountServiceAccountToken: true   # used ONLY to log in to Vault
      restartPolicy: Never
      securityContext:
        runAsNonRoot: true
        runAsUser: 65532
        fsGroup: 65532
        seccompProfile: { type: RuntimeDefault }
      containers:
        - name: assemble
          # Illustrative — build/pin your own image by digest.
          image: ghcr.io/example/stellar-seed-assembler:1.0.0
          args: ["assemble", "--threshold", "3"]
          env:
            - name: VAULT_ADDR
              value: https://vault.vault.svc:8200
            - name: VAULT_ROLE
              value: seed-assembler
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities: { drop: ["ALL"] }
```

The assembler's only write is the assembled seed to Vault KV — **never** to its own
filesystem:

```bash
vault kv put secret/stellar/validator seed="$RECOVERED_SEED"
```

Restrict egress with a NetworkPolicy so the pod can reach only Vault and the DNS resolver:

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: seed-assembler-egress
  namespace: stellar-system
spec:
  podSelector:
    matchLabels: { app: seed-assembler }
  policyTypes: ["Egress"]
  egress:
    - to:
        - namespaceSelector: { matchLabels: { kubernetes.io/metadata.name: vault } }
      ports: [{ port: 8200, protocol: TCP }]
    - to:
        - namespaceSelector: { matchLabels: { kubernetes.io/metadata.name: kube-system } }
      ports: [{ port: 53, protocol: UDP }]
```

### 4.4 Consuming the assembled seed in `StellarNode`

`seedSecretSource` supports exactly one of `localRef`, `externalRef`, `csiRef`, or
`vaultRef` (see [`src/crd/seed_secret.rs`](../../src/crd/seed_secret.rs)). For a sharded
seed, prefer the two paths that do not leave the seed in etcd as a base64 Secret:

**Option A — Vault Agent Injector (`vaultRef`).** The Agent renders the assembled seed
into `/vault/secrets/` and rolls the StatefulSet when the KV version changes:

```yaml
spec:
  validatorConfig:
    seedSecretSource:
      vaultRef:
        role: stellar-validator
        secretPath: secret/data/stellar/validator
        secretKey: seed
        secretFileName: stellar-seed
        restartOnSecretRotation: true
```

**Option B — Secrets Store CSI Driver (`csiRef`).** The seed is mounted directly from
Vault into the pod and **never written to etcd at all**; the operator injects
`STELLAR_SEED_FILE` pointing at the mount path:

```yaml
spec:
  validatorConfig:
    seedSecretSource:
      csiRef:
        secretProviderClassName: stellar-validator-seed-vault
        mountPath: /mnt/secrets/validator
        seedFileName: seed
```

> `localRef` (a plain Kubernetes Secret) is documented as **development only** in the
> CRD and is not an acceptable target for a sharded validator identity. If etcd is used
> at all it must have KMS-backed encryption at rest enabled, and the Secret must be
> readable only by the validator's ServiceAccount.

### 4.5 Assembly flow

```text
 custodians            Vault (independent domains)          cluster
 ──────────            ────────────────────────────          ───────
 share 1  ──transport──►  secret/data/stellar/shares/1  (transit ciphertext)
 share 3  ──transport──►  secret/data/stellar/shares/3  (transit ciphertext)
 share 5  ──transport──►  secret/data/stellar/shares/5  (transit ciphertext)
                                │
                                │  seed-assembler Job (SA token, ttl=15m)
                                │  transit/decrypt × k  →  SSS interpolate (RAM)
                                ▼
                          secret/data/stellar/validator   (short-lived KV v2)
                                │
                 ┌──────────────┴──────────────┐
                 ▼                             ▼
        Vault Agent (vaultRef)          CSI driver (csiRef)
        tmpfs /vault/secrets            /mnt/secrets/validator
                 └──────────────┬──────────────┘
                                ▼
                        stellar-core (STELLAR_SEED_FILE)
```

Audit both ends: enable a Vault audit device on every custody domain and alert on
`transit/decrypt` outside the assembly window, and enable Kubernetes audit logging for
`secret` reads/writes in the validator namespace.

---

## 5. Recovery and drills

Reconstruct only on a trusted host, and only with a quorum physically present.

```bash
# k custodian shares, piped in; nothing is written to disk.
cat share-1.txt share-3.txt share-5.txt \
  | python3 examples/scripts/shamir-split.py recover \
      --expect-pubkey GDLVVGABQKYQVN6VJP7NHSLEA45A5YLS6PNKMIZFV4BBU2HXA5IRVHUR \
      --fd 3   # optional: --no-emit to verify without printing the seed
```

Run a **restoration drill every 6–12 months**: physically retrieve `k` shares, run the
command above on a freshly booted offline host, confirm the public key matches, then
return the shares. Record the drill date, participants, `account_id`, and result. A
backup that has never been restored is an assumption, not a backup.

---

## 6. Detection and incident response

Signals that should page an operator:

| Signal | Why it matters |
|---|---|
| `seed-assembler` Job created outside a change window | Unexpected reconstruction attempt |
| `transit/decrypt/validator-share` denied or repeated | Probing of the share ciphertexts |
| Vault policy or role binding changed on a share path | Privilege escalation toward assembly |
| Non-operator ServiceAccount reads the seed Secret | Exfiltration attempt (Option A/B) |
| Share media reported missing, opened, or tampered | Custody breach |

If you believe the seed was exposed, treat the identity as compromised and rotate:
generate a new seed in a new ceremony, register the new validator key with the network,
and decommission the old identity. Do not attempt to "clean up" a suspected host and keep
signing with the exposed key.

---

## 7. Validation

The reference script is validated by three independent checks:

```bash
python3 examples/scripts/shamir-split.py selftest
```

```
rfc8032 vector 1: ok
rfc8032 vector 2: ok
rfc8032 vector 3: ok
round trip 2-of-2: ok (public key match=yes)
round trip 2-of-3: ok (public key match=yes)
round trip 3-of-5: ok (public key match=yes)
round trip 5-of-9: ok (public key match=yes)
SELFTEST PASSED: ed25519 derivation and Shamir round trips verified
```

1. **RFC 8032 vectors** — the pure-Python ed25519 derivation matches the published
   test vectors, so the `G...` comparison used to verify recovery is itself trusted.
2. **Round trips** — for several `k`/`n` combinations, `k` shares reconstruct a seed
   whose public key equals the original's.
3. **End-to-end recovery** — a `3-of-5` split written to disk and reconstructed from
   shares 1, 3, and 5 reproduces the expected `G...`; supplying only `k-1` shares fails
   closed, and a wrong `--expect-pubkey` exits non-zero.

---

## 8. What not to do

* Do not run `split --allow-disk` on an internet-connected machine.
* Do not store the plaintext seed, a `k`-of-`n` reconstruction, or the share-generating
  polynomial anywhere durable.
* Do not keep two shares with one custodian, one site, or one cloud account.
* Do not rely on the share checksum as authentication, and do not use a seed unless the
  recovered public key matched the published account id.
* Do not co-locate the Vault unseal quorum with the SSS share quorum.
* Do not log, paste into tickets, or commit shares — they are inert, but a leaked share
  still reduces the number of shares an attacker must obtain.

## References

* A. Shamir, *How to share a secret*, Communications of the ACM 22(11), 1979.
* RFC 8032 — *Edwards-Curve Digital Signature Algorithm (Ed25519)*.
* NIST SP 800-57 Part 1 Rev. 5 — *Recommendation for Key Management*.
* HashiCorp Vault — [Transit secrets engine](https://developer.hashicorp.com/vault/docs/secrets/transit).
* [Secret rotation](../secret-rotation.md) and
  [Credentials & Secrets](credentials-and-secrets.md) for the surrounding lifecycle.
