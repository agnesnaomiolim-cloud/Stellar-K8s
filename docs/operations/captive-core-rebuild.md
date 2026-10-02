# Captive Core State Rebuild: Disaster Recovery Guide

**Audience:** on-call operators responding to a Horizon API outage caused by a
corrupt or locked Captive Core state. **Goal:** safely wipe only the ephemeral
Captive Core local ledger directory and trigger a fresh ledger catch-up —
without touching the Horizon PostgreSQL database.

> **⚠️ Critical constraint:** This guide instructs you to delete the local
> Captive Core directory (`/var/lib/stellar`) inside the Horizon pod. It does
> **not** delete the Horizon PostgreSQL database. Deleting the PostgreSQL
> database is a separate, destructive operation that requires a full re-ingestion
> of all history and is almost never the right action for a Captive Core
> corruption event. If you are unsure which component is failing, read the
> [diagnostic section](#1-identify-captive-core-corruption) carefully before
> proceeding.

## Contents

- [Background](#background)
- [1. Identify Captive Core Corruption](#1-identify-captive-core-corruption)
- [2. Scale Down Horizon Safely](#2-scale-down-horizon-safely)
- [3. Delete the Captive Core State](#3-delete-the-captive-core-state)
- [4. Restart and Verify Catch-up](#4-restart-and-verify-catch-up)
- [5. Confirm Full Recovery](#5-confirm-full-recovery)
- [6. Post-Incident Steps](#6-post-incident-steps)
- [Automated Script](#automated-script)
- [Validation Procedure](#validation-procedure)
- [Related Documents](#related-documents)

---

## Background

Horizon embeds **Captive Core**: a Stellar Core process it manages in-process
(or as a subprocess) that maintains a local ledger SQLite database and
BucketList state under `/var/lib/stellar`. This directory is **ephemeral** —
Captive Core can always rebuild it from the network's history archives.

If Horizon crashes mid-write (OOM kill, SIGKILL during a rolling upgrade, node
eviction), the BucketList or SQLite state files can be left partially written.
On the next startup Captive Core detects the inconsistency, logs an
unrecoverable error, and refuses to start, stalling Horizon indefinitely.

The Horizon PostgreSQL database (`horizon` schema tables such as `history_*`,
`accounts`, `offers`, etc.) is **not affected** by Captive Core corruption.
The correct recovery action is to wipe only the Captive Core local state and
let it re-sync from the network.

---

## 1. Identify Captive Core Corruption

### Signature log patterns

Run the following to tail recent Horizon logs:

```bash
# Replace <name> and <namespace> with your StellarNode resource values
export NODE=my-horizon
export NS=stellar

kubectl logs -n "${NS}" -l "app.kubernetes.io/name=horizon,stellar.org/node=${NODE}" \
  --tail=200 --container horizon
```

A corrupt Captive Core state produces **one or more** of these log signatures:

```
# Locked process file — previous crash left a stale lock
FATAL src/work/WorkScheduler.cpp:... "Unable to acquire lock: /var/lib/stellar/stellar-core.lock"

# Corrupt BucketList header
ERROR src/bucket/BucketList.cpp:... "BucketList hash mismatch; expected=... got=..."

# SQLite WAL inconsistency
ERROR src/database/Database.cpp:... "database disk image is malformed"

# Captive Core subprocess exited during Horizon startup
level=error msg="error starting captive core" error="stellar-core exited with status 1"

# Horizon reports ingestion stall because Captive Core is not advancing
level=error msg="captive core is not producing new ledgers" last_ledger=... deadline=...
```

### Distinguish Captive Core failure from PostgreSQL failure

| Symptom | Likely cause |
|---------|-------------|
| Horizon pod in `CrashLoopBackOff`, logs show `stellar-core` lock or hash error | Captive Core corruption → **this guide** |
| Horizon pod running but `/health` returns `{"status":"syncing"}` for > 30 min | Normal catch-up; wait before acting |
| `pq: connection refused` or `FATAL: database "horizon" does not exist` | PostgreSQL problem — do **not** follow this guide |
| `level=error msg="reingestion required"` | Schema migration needed, not state corruption |

### Check Captive Core's local state directly

If the pod is still running (not yet in `CrashLoopBackOff`):

```bash
kubectl exec -n "${NS}" deploy/horizon-${NODE} -c horizon -- \
  ls -lh /var/lib/stellar/

# Expected healthy output — you will see files like:
# stellar-core.lock, buckets/, stellar.db, stellar.db-wal, stellar.db-shm

# A malformed state may show a zero-byte stellar.db or a stale .lock file:
# -rw-r--r-- 1 stellar stellar 0 Jan 01 00:00 stellar.db
# -rw-r--r-- 1 stellar stellar 4 Jan 01 00:00 stellar-core.lock
```

Simulate corruption for testing (see [Validation Procedure](#validation-procedure)):

```bash
# Corrupt the SQLite database header — use ONLY in non-production for drill purposes
kubectl exec -n "${NS}" deploy/horizon-${NODE} -c horizon -- \
  dd if=/dev/urandom of=/var/lib/stellar/stellar.db bs=512 count=1 conv=notrunc
```

---

## 2. Scale Down Horizon Safely

Before deleting the Captive Core state you must ensure no Horizon process is
writing to it. The operator manages Horizon as a Kubernetes `Deployment` (for
API nodes) or `StatefulSet` (for validators with captive core).

### Option A — Scale via the operator (recommended)

Set `spec.maintenanceMode: true` on the `StellarNode` resource. The operator
will drain traffic and scale replicas to zero:

```bash
kubectl patch stellarnode "${NODE}" -n "${NS}" \
  --type=merge \
  -p '{"spec":{"maintenanceMode":true}}'

# Wait for the operator to drain the pods
kubectl rollout status deployment/horizon-${NODE} -n "${NS}" --timeout=120s
```

### Option B — Scale the Deployment directly

Use this if you need to act faster than the operator reconcile loop allows, or
if the `StellarNode` CRD is not responding:

```bash
kubectl scale deployment/horizon-${NODE} -n "${NS}" --replicas=0

# Confirm all pods are gone before continuing
kubectl get pods -n "${NS}" -l "stellar.org/node=${NODE}" --watch
# Wait until the list is empty, then Ctrl-C
```

> **Do not skip this step.** Deleting `/var/lib/stellar` while Captive Core is
> running will immediately corrupt any in-flight BucketList merge and may leave
> the node in a state that is harder to recover.

---

## 3. Delete the Captive Core State

With Horizon scaled to zero, bring up a temporary maintenance pod that mounts
the same volume (if Captive Core state is on a PVC) or use a short-lived debug
pod to clear the ephemeral directory.

### Case A — Captive Core state is ephemeral (emptyDir / hostPath)

Because the state lives inside the container filesystem or a pod-local
`emptyDir`, simply deleting the pod is enough — Kubernetes will recreate it
with a fresh, empty `/var/lib/stellar` on next startup. Skip to
[step 4](#4-restart-and-verify-catch-up).

### Case B — Captive Core state is on a named PVC

Check whether Captive Core uses a dedicated PVC:

```bash
kubectl get pvc -n "${NS}" -l "stellar.org/node=${NODE}"
# Look for a PVC named like:  captive-core-<node>  or  stellar-data-<node>
```

If a dedicated `captive-core` PVC exists, clear it without deleting the volume:

```bash
# Identify the PVC name
CAPTIVE_PVC=$(kubectl get pvc -n "${NS}" \
  -l "stellar.org/component=captive-core,stellar.org/node=${NODE}" \
  -o jsonpath='{.items[0].metadata.name}')

echo "Clearing PVC: ${CAPTIVE_PVC}"

# Spin up a one-shot busybox pod to wipe the directory
kubectl run captive-core-reset-${NODE} \
  --namespace="${NS}" \
  --image=busybox:1.36 \
  --restart=Never \
  --overrides="{
    \"spec\": {
      \"volumes\": [{\"name\": \"captive-data\", \"persistentVolumeClaim\": {\"claimName\": \"${CAPTIVE_PVC}\"}}],
      \"containers\": [{
        \"name\": \"reset\",
        \"image\": \"busybox:1.36\",
        \"command\": [\"sh\", \"-c\", \"rm -rf /var/lib/stellar/* && echo DONE\"],
        \"volumeMounts\": [{\"name\": \"captive-data\", \"mountPath\": \"/var/lib/stellar\"}]
      }]
    }
  }"

# Wait for the pod to complete and confirm output
kubectl wait pod/captive-core-reset-${NODE} -n "${NS}" \
  --for=condition=Succeeded --timeout=60s

kubectl logs -n "${NS}" captive-core-reset-${NODE}
# Expected: DONE

# Clean up the temporary pod
kubectl delete pod/captive-core-reset-${NODE} -n "${NS}"
```

> **Do not delete the Horizon PVC** (`horizon-db-*` or any PVC backed by
> PostgreSQL). That volume holds the Horizon schema and would require a
> full re-ingestion from genesis to restore.

---

## 4. Restart and Verify Catch-up

### Scale Horizon back up

```bash
# If you used maintenanceMode, re-enable normal operation:
kubectl patch stellarnode "${NODE}" -n "${NS}" \
  --type=merge \
  -p '{"spec":{"maintenanceMode":false}}'

# Or if you scaled directly:
kubectl scale deployment/horizon-${NODE} -n "${NS}" --replicas=1
```

### Monitor the startup sequence

Captive Core will now start fresh and begin catching up from the network's
history archives. This process can take **5–30 minutes** depending on the
network and your archive bandwidth.

```bash
kubectl logs -n "${NS}" -l "stellar.org/node=${NODE}" \
  --container horizon -f --tail=100
```

**Expected log sequence — healthy fresh catch-up:**

```
# 1. Captive Core starts and reads the config
level=info msg="starting captive core" binary=/usr/bin/stellar-core

# 2. Captive Core contacts the history archive and begins downloading
INFO src/historywork/GetHistoryArchiveStateWork.cpp:... "Fetching state from archive"
INFO src/historywork/DownloadBucketsWork.cpp:... "Downloading bucket ..."

# 3. BucketList application begins
INFO src/bucket/BucketManager.cpp:... "Applying buckets to database"

# 4. Captive Core reports it has applied the last checkpoint and is catching up live
INFO src/ledger/LedgerManagerImpl.cpp:... "Loaded ledger from history archive"
INFO src/ledger/LedgerManagerImpl.cpp:... "Applying transactions"

# 5. Horizon detects Captive Core is live and starts ingesting
level=info msg="ingestion state" current_ledger=... latest_ledger=...
level=info msg="catching up to latest ledger"

# 6. Horizon marks itself ready once within configured lag tolerance
level=info msg="ingestion is up to date" lag_seconds=...
```

---

## 5. Confirm Full Recovery

### Health endpoint

```bash
kubectl exec -n "${NS}" deploy/horizon-${NODE} -c horizon -- \
  curl -s http://localhost:8080/health | jq .
```

Expected response when fully recovered:

```json
{
  "status": "healthy",
  "version": "...",
  "horizon_sequence": 12345678,
  "core_sequence": 12345678,
  "core_latest_ledger": 12345678
}
```

The `horizon_sequence` and `core_sequence` fields must be equal (or within a
few ledgers of each other) and the `status` must be `"healthy"`.

### Ledger lag metric

```bash
# Check the Prometheus metric for ingestion lag
kubectl exec -n "${NS}" deploy/horizon-${NODE} -c horizon -- \
  curl -s http://localhost:8080/metrics | grep ingest_ledger_lag
# Expected: a value close to 0 (typically < 5 seconds on a healthy network)
```

### End-to-end API smoke test

```bash
HORIZON_SVC=$(kubectl get svc -n "${NS}" -l "stellar.org/node=${NODE}" \
  -o jsonpath='{.items[0].metadata.name}')

kubectl run smoke-test-${NODE} \
  --namespace="${NS}" \
  --image=curlimages/curl:8.6.0 \
  --restart=Never \
  --rm -it \
  -- curl -s "http://${HORIZON_SVC}:8000/" | jq '.core_sequence'
# Expected: a recent mainnet/testnet ledger number
```

---

## 6. Post-Incident Steps

1. **Document the incident** using the [post-mortem template](../templates/post-mortem-template.md).
2. **Check why Captive Core crashed mid-write.** Common causes:
   - Node memory pressure triggering OOM kill — review resource limits in `spec.resources`.
   - Abrupt pod eviction during a rolling upgrade — consider setting `spec.strategy.type: Recreate` for Horizon deployments.
   - Storage I/O errors on the underlying node — check node events with `kubectl describe node <node>`.
3. **Review the Horizon deployment's `terminationGracePeriodSeconds`** — it should be at least `120s` to give Captive Core time to flush its state on graceful shutdown.
4. **Consider enabling PodDisruptionBudgets** so cluster drain operations don't forcibly evict Horizon during a write. See [pod-disruption-budget.md](../pod-disruption-budget.md).
5. **Set up alerting** on the `captive_core_is_stale` and `horizon_ingest_ledger_lag` metrics so the next occurrence is caught before it becomes a customer-visible outage.

---

## Automated Script

For a scriptable version of steps 2–4 above, see
[`examples/troubleshooting/reset-captive-core.sh`](../../examples/troubleshooting/reset-captive-core.sh).

The script:
- Accepts `--node`, `--namespace`, and `--dry-run` flags.
- Scales Horizon to zero, clears `/var/lib/stellar`, scales Horizon back up.
- Tails the logs until Captive Core reports successful catch-up or the timeout is reached.
- Exits non-zero on failure so it can be used in automation pipelines.

---

## Validation Procedure

To validate this guide against a real cluster (required before marking the
DR drill as complete in the [DR results template](../dr-results-template.md)):

1. Deploy a Horizon node to a non-production namespace.
2. Wait for it to reach `status: healthy`.
3. Corrupt the Captive Core SQLite header to simulate a crash mid-write:
   ```bash
   # Identify the running Horizon pod
   POD=$(kubectl get pods -n "${NS}" -l "stellar.org/node=${NODE}" \
     -o jsonpath='{.items[0].metadata.name}')

   # Overwrite the first 512 bytes of the SQLite database with random data
   kubectl exec -n "${NS}" "${POD}" -c horizon -- \
     dd if=/dev/urandom of=/var/lib/stellar/stellar.db \
        bs=512 count=1 conv=notrunc

   # Force-kill the pod to trigger a restart with the corrupt state
   kubectl delete pod -n "${NS}" "${POD}"
   ```
4. Observe the pod enter `CrashLoopBackOff` and confirm the expected error
   log signatures from [section 1](#1-identify-captive-core-corruption).
5. Execute the rebuild procedure (sections 2–5) and confirm Horizon returns
   to `status: healthy` within the expected time window.
6. Record the actual RTO in the DR results template.

---

## Related Documents

- [Disaster Recovery & Quorum Loss Runbook](disaster-recovery.md)
- [DR Failover Guide](../dr-failover.md)
- [Backup and Disaster Recovery Runbook](../backup-disaster-recovery-runbook.md)
- [Pod Disruption Budgets](../pod-disruption-budget.md)
- [Capacity Planning](capacity-planning.md)
- [DR Results Template](../dr-results-template.md)
- [Post-Mortem Template](../templates/post-mortem-template.md)
