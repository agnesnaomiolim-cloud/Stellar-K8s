#!/usr/bin/env bash
# Copyright 2024 Stellar-K8s Contributors
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
# force-scp.sh — guarded wrapper for split-brain recovery steps
#
# Companion to docs/operations/split-brain-recovery.md. Automates the
# DESTRUCTIVE half of that runbook (Path A reset, or Path B force-scp flag)
# behind explicit confirmation gates. It cannot decide *for* you whether the
# node actually diverged — Step 0 of the runbook is yours to do by hand.
#
# Usage:
#   ./examples/troubleshooting/force-scp.sh --node <name> --namespace <ns> \
#       --trusted-ledger <seq> [--archive <url>] [--force-scp] [--yes]
#
#   --trusted-ledger  Last ledger sequence agreed on the canonical chain
#                     (checkpoint at or below the split point; verify against
#                     TWO independent sources before typing it here).
#   --force-scp       Opt in to Path B (plant the stellar-core force-scp
#                     flag). Default is Path A (new-db + catchup), which is
#                     the right answer in almost every incident.
#   --yes             Skip interactive confirmations (for scripted drills on
#                     a THROWAWAY testnet only). Never use against mainnet.
#
# Exit codes: 0 recovered/flag planted · 1 preflight or confirmation failure ·
# 2 usage error.

set -euo pipefail

SCRIPT_NAME="$(basename "$0")"
NODE=""
NS=""
TRUSTED_LEDGER=""
ARCHIVE=""
FORCE_SCP=false
ASSUME_YES=false
CORE_CONTAINER="stellar-node"
CORE_LABEL="app.kubernetes.io/instance"

usage() { grep -E '^# (Usage:|   )' "$0" | sed 's/^# \?//'; }

die() { echo "ERROR: $*" >&2; exit 1; }

info() { echo "==> $*"; }

warn() { echo "!!  $*" >&2; }

confirm() {
    $ASSUME_YES && return 0
    local answer
    read -r -p "$1 [type YES to proceed] " answer
    [ "$answer" = "YES" ] || die "aborted by operator at: $1"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --node) NODE="${2:?}"; shift 2 ;;
        --namespace) NS="${2:?}"; shift 2 ;;
        --trusted-ledger) TRUSTED_LEDGER="${2:?}"; shift 2 ;;
        --archive) ARCHIVE="${2:?}"; shift 2 ;;
        --force-scp) FORCE_SCP=true; shift ;;
        --yes) ASSUME_YES=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

[ -n "$NODE" ] || { usage >&2; die "--node is required"; }
[ -n "$NS" ] || { usage >&2; die "--namespace is required"; }
[ -n "$TRUSTED_LEDGER" ] || { usage >&2; die "--trusted-ledger is required (checkpoint at/below the split point)"; }
command -v jq >/dev/null || die "jq is required"
command -v kubectl >/dev/null || die "kubectl is required"

banner() {
    cat <<'EOF'
********************************************************************************
*  DESTRUCTIVE Stellar validator recovery (split-brain runbook)
*
*  What this script CAN do: wipe this node's local ledger DB and buckets
*  (`stellar-core new-db`), replay canonical history, and — only with
*  --force-scp — plant the stellar-core force flag for ONE supervised start.
*
*  What it CANNOT undo: running this against a healthy node, against the
*  wrong node, or while a second copy of the same seed key is alive anywhere.
*  Either mistake means hours of rebuild — or a network-visible fork.
*
*  Read docs/operations/split-brain-recovery.md first. If you have not done
*  Step 0 (confirm divergence) yourself, STOP here.
********************************************************************************
EOF
}

# --- Preflight ---------------------------------------------------------------

banner

POD="$(kubectl get pod -n "$NS" -l "${CORE_LABEL}=${NODE}" \
    -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)"
[ -n "$POD" ] || die "no pod found for node ${NODE} in namespace ${NS} (label ${CORE_LABEL}=${NODE})"

info "node=${NODE} ns=${NS} pod=${POD}"
info "trusted ledger: ${TRUSTED_LEDGER}"
[ -n "$ARCHIVE" ] && info "canonical archive: ${ARCHIVE}"
$FORCE_SCP && warn "Path B requested: the stellar-core force-scp flag WILL be planted."

# Gate 0: divergence must be plausible before anything destructive happens.
# Best-effort read of the local ledger head — if core is unreachable we
# cannot judge, and refuse to continue without explicit override.
set +e
LOCAL_LEDGER="$(kubectl exec -n "$NS" "$POD" -c "$CORE_CONTAINER" -- \
    stellar-core http-command 'info' 2>/dev/null | jq -r '.info.ledger.num // empty')"
set -e
if [ -z "$LOCAL_LEDGER" ]; then
    warn "stellar-core did not answer http-command info on ${POD}."
    warn "If core is still running under a supervisor, new-db is UNSAFE."
    confirm "Core unreachable — proceed anyway (only if core is confirmed STOPPED)?"
else
    DIFF=$((LOCAL_LEDGER - TRUSTED_LEDGER))
    info "local ledger head: ${LOCAL_LEDGER} (canonical: ${TRUSTED_LEDGER}, diff: ${DIFF})"
    if [ "$DIFF" -lt 0 ]; then
        warn "local ledger is BEHIND the trusted ledger by $((-DIFF)) — that is"
        warn "ordinary catchup lag, NOT a split brain. This script is the wrong tool."
        confirm "Behind-canonical node — are you SURE it diverged (Step 0 done)?"
    fi
fi

# Gate 1: double-sign check. Two copies of one seed key running is the
# network-fork scenario; refuse to add a third by recovering in place.
EXTRA="$(kubectl get pods -A -l "${CORE_LABEL}=${NODE}" --no-headers 2>/dev/null \
    | grep -v "^${NS} " | grep -vc '^$' || true)"
if [ "${EXTRA:-0}" -gt 0 ]; then
    kubectl get pods -A -l "${CORE_LABEL}=${NODE}"
    die "pods for ${NODE} exist OUTSIDE namespace ${NS} — stop them completely first (double-sign hazard)"
fi

# Gate 2: freeze the operator so it does not reconcile around the repair.
info "setting maintenanceMode=true on ${NODE}"
kubectl patch stellarnode "$NODE" -n "$NS" --type merge \
    -p '{"spec":{"maintenanceMode":true}}' \
    || die "could not set maintenanceMode (aborting before any destructive step)"
confirm "Operator frozen. Proceed to DESTRUCTIVE phase on pod ${POD}?"

# --- Destructive phase (inside the pod) --------------------------------------

KUBECTL_EXEC=(kubectl exec -n "$NS" "$POD" -c "$CORE_CONTAINER" --)

info "reinitializing local ledger state (stellar-core new-db)"
"${KUBECTL_EXEC[@]}" stellar-core new-db \
    || die "stellar-core new-db failed — state may be half-wiped; see runbook Step 4"

if $FORCE_SCP; then
    echo
    warn "PATH B: planting the stellar-core force-scp flag."
    warn "This node will EMIT SCP from its own last known ledger on next start."
    warn "Upstream: force-scp does NOT relax quorum; SCP completes only if a"
    warn "quorum of the slice emits on the same ledger. Exactly ONE node in the"
    warn "slice may be forced; all others must recover via Path A."
    confirm "Plant force-scp flag on ${NODE}?"
    "${KUBECTL_EXEC[@]}" stellar-core force-scp \
        || die "stellar-core force-scp failed — do NOT retry blindly; escalate"
    info "flag planted. Start core once under supervision, verify slice"
    info "convergence, then clear it: stellar-core force-scp --reset"
else
    TARGET="${TRUSTED_LEDGER}/0"
    if [ -n "$ARCHIVE" ]; then
        info "replaying canonical history up to ${TARGET} from ${ARCHIVE}"
    else
        info "replaying canonical history up to ${TARGET} from the node's configured archive"
    fi
    "${KUBECTL_EXEC[@]}" stellar-core catchup "$TARGET" \
        || die "catchup failed — inspect core logs; do NOT re-run new-db without re-reading the runbook"
    info "catchup complete. Verify info shows the expected ledger head, then"
    info "restart the pod so core resumes under its normal supervisor:"
    info "  kubectl delete pod ${POD} -n ${NS}"
fi

# --- Handback ----------------------------------------------------------------

if ! $FORCE_SCP; then
    set +e
    HEAD="$(kubectl exec -n "$NS" "$POD" -c "$CORE_CONTAINER" -- \
        stellar-core http-command 'info' 2>/dev/null | jq -r '.info.ledger.num // empty')"
    set -e
    if [ -n "$HEAD" ]; then
        info "post-recovery local ledger head: ${HEAD} (trusted: ${TRUSTED_LEDGER})"
    fi
fi

cat <<EOF

Next steps (manual, see docs/operations/split-brain-recovery.md):
  1. Restart the pod so core runs supervised:  kubectl delete pod ${POD} -n ${NS}
  2. Watch it rejoin and AGREE with a reference node for at least an hour.
  3. Fix spec.validatorConfig.quorumSet BEFORE normal operation resumes —
     a split brain is a configuration that only looks redundant.
  4. Clear maintenanceMode FIRST, verify kubectl stellar status, then remove
     the probe override (Recovery Mode step 3 in the runbook).
EOF

if $FORCE_SCP; then
    cat <<'EOF'
  5. After the slice converges, clear the force flag:
       stellar-core force-scp --reset
     A node left configured to force will re-fork the network at the next
     outage.
EOF
fi

info "done — the rest is manual by design."
