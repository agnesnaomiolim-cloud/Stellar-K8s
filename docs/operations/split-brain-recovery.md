# Split-Brain & Divergent-Ledger Recovery Runbook

**Audience:** on-call operators responding to a live consensus incident. **Goal:** stop a
diverged validator from signing on a forked history and drag it back into the canonical
chain — without making the damage permanent.

> **⚠️ READ THIS FIRST — the commands in this runbook can permanently corrupt a node.**
>
> - Running the recovery steps against the **wrong** node (one that is healthy, or merely
>   slow) destroys its local ledger state and forces a full catchup — hours of downtime you
>   caused yourself.
> - Running `stellar-core` with `--force-scp` on a node while *any other copy of the same
>   seed key is running anywhere* is a network-visible double-sign and can **fork the
>   network**. Exactly one process, exactly one copy of the key, ever (see the
>   [rule at the top of the disaster recovery runbook](disaster-recovery.md)).
> - `stellar-core force-scp` plants a flag in the node's database so that, on its next
>   start, the node *emits* SCP messages from its own last known ledger instead of waiting
>   to hear one from the network. If the local ledger is wrong and enough of the slice
>   follows along, you have just published a fork. Treat it as a network-level last resort
>   (Path B below), never as routine repair.
> - When in doubt, stop. Escalate to the operator team before touching the database.

This runbook covers the **split-brain** case: a quorum misconfiguration or network
partition caused part of your deployment to keep closing ledgers on a *different history*.
It complements, and does not replace, the
[Disaster Recovery & Quorum Loss Runbook](disaster-recovery.md):

| Symptom | Runbook |
| --- | --- |
| Node closes **no** ledgers; stuck `Out of sync`; quorum never met | [disaster-recovery.md, Scenario 3](disaster-recovery.md#scenario-3-total-quorum-loss--forcing-a-sync-from-archives) |
| Node (or a whole slice) **is closing ledgers**, but on a history that disagrees with the rest of the network | **this document** |

A split-brain is worse than quorum loss: a silently diverging validator signs invalid
transactions from the network's point of view, and its history archives (if it publishes
any) become poison. Recovery is a two-part job: **(1)** roll the node's local state back
onto the canonical chain, and **(2)** fix the quorum configuration so the partition cannot
recur at the next network hiccup.

## Contents

- [Prerequisites](#prerequisites)
- [Step 0: Confirm You Actually Have a Split Brain](#step-0-confirm-you-actually-have-a-split-brain)
- [Step 1: Stop the Bleeding](#step-1-stop-the-bleeding)
- [Step 2: Establish the Trusted Ledger](#step-2-establish-the-trusted-ledger)
- [Path A: The Node Is Wrong — Reset and Rejoin](#path-a-the-node-is-wrong--reset-and-rejoin)
- [Path B: `--force-scp` — Network Last Resort](#path-b---force-scp--network-last-resort)
- [Step 5: Fix the Quorum Set Before You Restart](#step-5-fix-the-quorum-set-before-you-restart)
- [Step 6: Post-Recovery Verification](#step-6-post-recovery-verification)
- [References](#references)

## Prerequisites

- `kubectl` configured against the affected cluster with admin access to the `StellarNode`
  resource and its Pods/PVCs; the [`kubectl stellar` plugin](../kubectl-plugin.md) helps
  but is not required.
- Comfort working inside the `stellar-core` container: this runbook drives
  [`stellar-core` HTTP admin commands](https://github.com/stellar/stellar-core/blob/master/docs/admin.md)
  directly.
- Know the node's name and namespace. Commands below assume:
  ```bash
  export NODE=my-validator
  export NS=stellar-nodes
  ```
- The helper script
  [`examples/troubleshooting/force-scp.sh`](../../examples/troubleshooting/force-scp.sh)
  wraps Steps 1–4 with confirmation gates. **Read it before running it** — it performs the
  same destructive commands documented here, and the confirmations it asks for are the
  procedure, not an annoyance.

## Step 0: Confirm You Actually Have a Split Brain

Every step after this one is destructive. A node that is merely catching up, or in
harmless `Out of sync` after a restart, does **not** need this runbook.

```bash
POD=$(kubectl get pod -n $NS -l app.kubernetes.io/instance=$NODE \
  -o jsonpath='{.items[0].metadata.name}')

# 1. What does the node think is happening?
kubectl exec -n $NS $POD -c stellar-node -- \
  stellar-core http-command 'info' | jq '{state: .info.state, ledger: .info.ledger.num, age: .info.ledger.age}'

# 2. Is SCP intersecting the network, or has it split off?
kubectl exec -n $NS $POD -c stellar-node -- \
  stellar-core http-command 'quorum' | jq .
```

**Split-brain signatures:**

- `info` shows `state: "Synced!"` and `ledger.num` **advancing**, but the number disagrees
  with a reference node (any healthy validator you trust, or a block explorer) by more
  than the normal couple of ledgers of latency:
  ```bash
  kubectl exec -n $NS $POD -c stellar-node -- \
    stellar-core http-command 'info' | jq '.info.ledger.num'
  # compare against the network; a persistent gap that *grows* is a fork
  ```
- `quorum` reports the node has **not enough** agreement, yet `info` still shows ledgers
  closing (`age` staying small). A node closing ledgers *without* quorum agreement is the
  definition of the dangerous state — this is what a stray `--force-scp` flag produces.
- Logs show `Ledger closed` with a sequence that other validators never emit:
  ```bash
  kubectl logs -n $NS $POD -c stellar-node --tail=500 | grep "Ledger closed" | tail -5
  ```
- The [operator's quorum analysis](../../docs/quorum-validation-engine.md) flags it:
  ```bash
  kubectl get stellarnode $NODE -n $NS \
    -o jsonpath='{.status.quorumFragility}'; echo
  # 1.0 = maximally fragile — investigate even if ledgers look normal
  ```

**Rule out the cheap explanations first:** clock skew (NTP broken on the node), a
stale/duplicated pod from an old StatefulSet still running with the same seed (the
double-sign trap), or a history archive that this node trusts but nobody else does. Those
mimic split-brain symptoms and are fixed without touching the database.

## Step 1: Stop the Bleeding

The diverged node must stop signing *now*, before you diagnose anything else.

1. **Apply Recovery Mode exactly as documented** in
   [disaster-recovery.md](disaster-recovery.md#recovery-mode): relax the readiness probe
   *first*, then set `maintenanceMode: true` *second*. The operator freezes and stops
   reconciling the node, so your manual steps stick.
2. **Confirm exactly one copy of the seed key is running.** This is the double-sign check,
   and it is mandatory:
   ```bash
   # every pod that could be this validator, in every cluster you operate
   kubectl get pods -A -l app.kubernetes.io/instance=$NODE
   # and nothing unexpected sharing the identity:
   kubectl get stellarnode -A | grep $NODE
   ```
   If a second copy exists anywhere (old cluster, failover clone, leftover debug pod),
   stop it *completely* before anything else.
3. **Verify `stellar-core` has actually stopped** before touching its database — never run
   `new-db` against a live core process:
   ```bash
   kubectl exec -n $NS $POD -c stellar-node -- stellar-core http-command 'info' || \
     echo "core is not answering — good (stopped)"
   ```
   If core is still running (e.g. as the container's foreground process), scale the
   StatefulSet to zero instead of killing the process in-place:
   ```bash
   kubectl scale statefulset $NODE -n $NS --replicas=0
   kubectl wait --for=delete pod/$POD -n $NS --timeout=120s
   kubectl scale statefulset $NODE -n $NS --replicas=1
   kubectl wait --for=jsonpath='{.status.phase}'=Running pod -n $NS \
     -l app.kubernetes.io/instance=$NODE --timeout=120s
   ```
   The replacement pod starts core under the supervisor in a *fresh* exec session; do the
   destructive commands through a second exec while core is *stopped* inside it —
   coordinate this per your image's process supervisor.

## Step 2: Establish the Trusted Ledger

You need one unambiguous fact before recovering: **the last ledger sequence on the
canonical chain that everyone agrees on.**

- Stellar history archives publish checkpoints every 64 ledgers (~5 minutes on mainnet);
  the checkpoint *at or below* the split point is your rollback target.
- Take the number from **at least two independent sources**: a healthy validator in a
  different failure domain, and the canonical archive
  (`curl -fs https://<archive>/.well-known/stellar-history.json` gives you the current
  checkpoint ledger).
- If your node publishes archives, note its **last published checkpoint** — after
  recovery you must verify it did not publish anything post-split (see
  [Step 6](#step-6-post-recovery-verification)).

```bash
export TRUSTED_LEDGER=<sequence>   # e.g. 48157440 — from your reference sources
```

Everything below rolls the node back to `TRUSTED_LEDGER` (or cleanly past it via catchup).

## Path A: The Node Is Wrong — Reset and Rejoin

**Use when:** the network's canonical chain is healthy and only this node (or your
slice) diverged. This is the overwhelmingly common case, and it does **not** use
`--force-scp` at all: the node reinitializes, replays canonical history from the archive,
and rejoins as a follower.

```bash
# Inside the pod, with stellar-core STOPPED (Step 1):

# 1. Destroy the diverged local state — SQL DB and bucket references.
#    There is no undo. You did confirm $POD is the right node, and that the
#    seed key has no second copy running, didn't you?
stellar-core new-db

# 2. Replay canonical history from the trusted archive up to the latest
#    checkpoint. 'current/0' = catch up fully, trimming nothing.
stellar-core catchup current/0

#    To stop exactly at your trusted ledger instead of the archive head:
#    stellar-core catchup $TRUSTED_LEDGER/0

# 3. Sanity check: fresh DB, expected state.
stellar-core http-command 'info'
# expect: state NOT "Synced!" yet, ledger.num at/near the archive checkpoint

# 4. Exit; restart the pod so core comes back up under its supervisor.
kubectl delete pod $POD -n $NS
```

On restart, watch it rejoin as a *follower* — it should reach `Synced!` within a few
minutes and agree with the reference node:

```bash
kubectl logs -n $NS -l app.kubernetes.io/instance=$NODE -c stellar-node -f | \
  grep -E "Synced|Ledger closed"
```

If it closes ledgers matching the network: recovery succeeded, skip to
[Step 5](#step-5-fix-the-quorum-set-before-you-restart). If it **again** drifts onto a
different history, you do not have a node problem — you have a quorum-configuration
problem, and Path B will not save you. Go directly to
[Step 5](#step-5-fix-the-quorum-set-before-you-restart), fix the quorum set, then repeat
Path A once more.

## Path B: `stellar-core force-scp` — Network Last Resort

> **⚠️ You should expect never to use this.** `stellar-core force-scp` sets a flag in the
> node's database: the *next* time core starts, it opens SCP from its own last known
> ledger rather than waiting for the network to tell it what the ledger is. Upstream is
> explicit that this does **not** lower quorum requirements — SCP still won't complete
> unless a quorum of other nodes is also emitting messages on that same ledger. That is
> exactly the point: it exists for the narrow case where **no quorum can form anywhere**
> (e.g. an entire quorum slice was halted after a partition and must be restarted from
> the slice's own last-known-good ledger), and it is run on **one designated node, once**,
> by someone who can explain out loud why the local ledger is the correct one. On a
> healthy network, a forced node is a fork factory — the network simply ignores it unless
> your slice carries enough weight to make the wrong ledger win.

Conditions — all of them, every time:

1. The canonical chain is **not** reachable/producible (Path A is impossible), and the
   operators of **every validator in the slice** have agreed on the restart plan and on
   which node runs the force flag.
2. Exactly **one** node in the slice will force; every other node does Path A (`new-db` +
   catchup) so they follow rather than compete.
3. The designated node's local ledger has been verified against independent records
   (explorer snapshots, a second archive, published checkpoint hashes) — imposing an
   unverified ledger is how a temporary partition becomes a permanent fork.
4. The node is otherwise healthy: no corruption, buckets intact, and (once it is back up
   in a safe, isolated state) `stellar-core http-command 'checkdb'` reports clean.

Procedure:

```bash
# On the ONE designated node, stellar-core stopped, after Step 1:

# Recommended pre-flight: the local state should be internally consistent and
# explainable against independent records before you ask the network to
# accept it. These read local history only (no network connection):
stellar-core offline-info        # last known ledger of this node, offline
cmp <(stellar-core offline-info | jq -r '.info.ledger.num') \
  <(echo $TRUSTED_LEDGER) && echo "local ledger == agreed ledger" || echo "STOP: mismatch"

# Plant the force flag in the database (does NOT start anything yet):
stellar-core force-scp

# Now start core normally (your image's supervisor / `stellar-core run`).
# It emits SCP from its last known ledger; peers rejoining via Path A will
# follow it because they have no other source of truth.
```

Do **not** persist the force setting into the node's configuration — no `FORCE_SCP=true`
in the rendered `stellar-core.cfg`, no extra arg in the StatefulSet command. The flag
lives in the database, applies to exactly one start, and should be cleared once the
slice is back in normal operation:

```bash
stellar-core force-scp --reset   # clears the persisted flag (see upstream command docs)
```

A node configured to always force will silently re-fork the network at the next outage.

Then watch the slice converge on the imposed ledger:

```bash
kubectl exec -n $NS $POD -c stellar-node -- stellar-core http-command 'quorum' | jq .
```

If peers fail to follow, or `quorum` shows non-intersecting sets after five minutes,
**stop forcing**: clear the flag (`stellar-core force-scp --reset`), restart core
normally, keep the node halted, and escalate. Repeated forcing against a disagreeing
network is a fork machine.

## Step 5: Fix the Quorum Set Before You Restart

A split-brain is almost never a random failure — it is a configuration that only *looks*
redundant. If you restart into the same quorum set, you will be back in this runbook
within the week. Fix it **via the CRD**, not by hand-editing rendered ConfigMaps; the
operator renders `stellar-core.cfg` from `spec.validatorConfig.quorumSet` and will
reconcile hand-edits away.

```bash
kubectl get stellarnode $NODE -n $NS -o jsonpath='{.spec.validatorConfig.quorumSet}'
```

```yaml
# Current, hopefully-fixed shape (stellar-core .cfg syntax):
spec:
  validatorConfig:
    quorumSet: |
      [QUORUM_SET]
      THRESHOLD_PERCENT=67
      VALIDATORS=[
        "$sdf1", "$sdf2", "$sdf3"
      ]
```

The failure modes that produced this incident, in rough order of how often they bite:

- **Threshold too high for the partition you actually get.** `THRESHOLD_PERCENT=100` (or
  a top-level set containing a nested set that must fully agree) means one unavailable
  validator stops consensus *or* — worse, when combined with a mis-scoped nested set —
  splits agreement instead of stopping it. 67% of a top-level set of independent
  validators is the standard starting point.
- **Nested sets that make quorum transitive instead of intersectional.** If your inner
  sets can each reach their own threshold independently, two inner sets can validate
  *different* histories and the outer set never notices. Keep the nesting shallow and
  make sure every inner set shares members with every other (that's what "intact
  intersection" means — `stellar-core http-command 'quorum'` shows this directly).
- **Dependencies you don't control, weighted like ones you do.** A validator run by
  someone else is a partition-in-waiting: if they lose connectivity, your threshold math
  changes. Treat external validators as extra voices, not quorum-critical members, or run
  your own in multiple failure domains.
- **Stale members after a failover/clone.** A retired validator still listed in
  `VALIDATORS` (or a cloned testnet node with a production identity) produces exactly the
  "two copies of one seed" hazard from Step 1. Audit the list against what actually
  exists after *every* DR drill.
- **homeDomain/archive mismatches** that make peers unable to verify each other, so
  agreement silently narrows to whoever *can* verify.

Apply the fix through the CRD (`kubectl apply -f <fixed-cr.yaml>` or
`kubectl patch stellarnode`), then restart the node so the operator renders the corrected
`stellar-core.cfg` (see [Step 1](#step-1-stop-the-bleeding) for the restart mechanics).
Keep `maintenanceMode` set until the pod is up and `stellar-core` is running — that way
the operator stays out of the way during the restart, but the fresh process still picks
up the rendered config from the CRD fix.

Before trusting the new set, validate it against the history you still trust —
stellar-core can infer and cross-check quorum from recorded history without a network
connection:

```bash
stellar-core infer-quorum    # what quorum does history say this node actually has?
stellar-core check-quorum    # is there intact intersection over all validators?
```

If `check-quorum` reports missing intersection, the set is still forkable — do not
return to normal operation with it. After the node rejoins and you clear
`maintenanceMode`, run the operator's quorum analysis and confirm the fragility score
dropped:

```bash
kubectl get stellarnode $NODE -n $NS -o jsonpath='{.status.quorumFragility}'; echo
kubectl get stellarnode $NODE -n $NS -o jsonpath='{.status.quorumAnalysisTimestamp}'; echo
```

## Step 6: Post-Recovery Verification

- **Ledger agreement over time, not just at t=0.** Compare `info`'s `ledger.num` against
  the reference node every few minutes for the first hour; a second divergence means the
  quorum fix didn't take. Trigger a background consistency check of the recovered
  database with `stellar-core http-command 'checkdb'` and make sure it comes back clean.
- **If the node publishes history archives:** verify it did **not** publish any
  post-split checkpoints before letting it publish again. A diverged archive poisons
  every future recovery that trusts it. Compare the newest checkpoint hash in its archive
  against the canonical archive for the same sequence; if they differ (or a foreign
  checkpoint exists at all), cordon the node's publisher and rebuild the archive from the
  canonical one before resuming publication.
- **Reverse Recovery Mode** in the documented order: clear `maintenanceMode` first, watch
  the operator reconcile the node back to a healthy phase
  (`kubectl stellar status $NODE -n $NS`), then remove the probe override.
- **Close the loop on monitoring:** the topology dashboards in
  [SCP Consensus Topology and Monitoring](../scp-consensus-topology-and-monitoring.md)
  and the checks in [quorum-validation-engine.md](../quorum-validation-engine.md) should
  show the node intersecting properly. Set an alert on `quorumFragility` so the next
  misconfiguration pages you *before* the next partition, not after.
- File the post-mortem with the actual quorum diff (before/after `quorumSet`) attached —
  the [post-mortem template](../incident-response/post-mortem.md) is the starting point.

## References

- [Disaster Recovery & Quorum Loss Runbook](disaster-recovery.md) — quorum loss, `new-db`
  catchup mechanics, Recovery Mode sequencing
- [Core Blue/Green Runbook](core-blue-green-runbook.md) — upgrading validators without
  stopping consensus (the safest time to fix a quorum set)
- [SCP Consensus Topology and Monitoring](../scp-consensus-topology-and-monitoring.md)
- [Quorum Validation Engine](../quorum-validation-engine.md)
- [`examples/troubleshooting/force-scp.sh`](../../examples/troubleshooting/force-scp.sh) —
  guarded wrapper for Steps 1–4
- Upstream:
  [`stellar-core` admin commands](https://github.com/stellar/stellar-core/blob/master/docs/admin.md),
  [quorum and nesting explained](https://github.com/stellar/stellar-core/blob/master/docs/learn/admin.md)
