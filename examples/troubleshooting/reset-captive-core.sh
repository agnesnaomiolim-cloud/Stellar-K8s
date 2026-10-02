#!/usr/bin/env bash
# reset-captive-core.sh
#
# Disaster recovery helper for a corrupt Captive Core state on a Horizon pod
# managed by the Stellar-K8s operator.
#
# What it does:
#   1. Scales the Horizon Deployment to zero replicas (stopping Captive Core).
#   2. Clears /var/lib/stellar inside the pod volume (ephemeral emptyDir) or,
#      when a dedicated captive-core PVC is detected, mounts the PVC in a
#      one-shot busybox pod and wipes it there.
#   3. Scales Horizon back up to the original replica count.
#   4. Tails the Horizon logs until Captive Core reports a successful catch-up
#      or the --timeout is reached.
#
# What it does NOT do:
#   - It does NOT touch the Horizon PostgreSQL database.
#   - It does NOT delete the Horizon data PVC (horizon-db-* or similar).
#   - It does NOT modify the StellarNode CRD spec.
#
# Usage:
#   ./reset-captive-core.sh --node my-horizon --namespace stellar [options]
#
# Full reference:
#   docs/operations/captive-core-rebuild.md

set -euo pipefail

SCRIPT_NAME="$(basename "$0")"
readonly SCRIPT_NAME

# ----------------------------------------------------------------------------
# Colour helpers
# ----------------------------------------------------------------------------
if [[ -t 1 ]]; then
  RED='\033[0;31m'
  YELLOW='\033[1;33m'
  GREEN='\033[0;32m'
  CYAN='\033[0;36m'
  BOLD='\033[1m'
  RESET='\033[0m'
else
  RED='' YELLOW='' GREEN='' CYAN='' BOLD='' RESET=''
fi

info()    { printf "${CYAN}[INFO]${RESET}  %s\n" "$*"; }
success() { printf "${GREEN}[OK]${RESET}    %s\n" "$*"; }
warn()    { printf "${YELLOW}[WARN]${RESET}  %s\n" "$*"; }
fatal()   { printf "${RED}[FATAL]${RESET} %s\n" "$*" >&2; exit 1; }
step()    { printf "\n${BOLD}==> %s${RESET}\n" "$*"; }

# ----------------------------------------------------------------------------
# Defaults
# ----------------------------------------------------------------------------
NODE=""
NAMESPACE="stellar"
DRY_RUN=0
TIMEOUT=1800            # seconds to wait for Horizon to become healthy (default 30 min)
SCALE_WAIT=120          # seconds to wait for pods to reach 0
TEMP_POD_IMAGE="busybox:1.36"
CAPTIVE_CORE_DIR="/var/lib/stellar"
HORIZON_PORT=8080       # internal health port

# ----------------------------------------------------------------------------
# Usage
# ----------------------------------------------------------------------------
usage() {
  cat <<EOF
${BOLD}Usage:${RESET}
  ${SCRIPT_NAME} --node <node-name> [options]

${BOLD}Required:${RESET}
  --node NAME          Name of the StellarNode resource (and Horizon Deployment prefix)

${BOLD}Options:${RESET}
  --namespace NS       Kubernetes namespace (default: ${NAMESPACE})
  --timeout SECS       Seconds to wait for Horizon to become healthy (default: ${TIMEOUT})
  --dry-run            Print the steps that would be executed without applying any changes
  --image IMAGE        Busybox image used for PVC wipe pod (default: ${TEMP_POD_IMAGE})
  -h, --help           Show this help

${BOLD}Examples:${RESET}
  # Interactive dry-run — see exactly what would happen
  ${SCRIPT_NAME} --node my-horizon --namespace stellar --dry-run

  # Live reset with a 20-minute recovery window
  ${SCRIPT_NAME} --node my-horizon --namespace stellar --timeout 1200

${BOLD}WARNING:${RESET}
  This script deletes the Captive Core ephemeral state under ${CAPTIVE_CORE_DIR}.
  It does NOT affect the Horizon PostgreSQL database.

Full documentation: docs/operations/captive-core-rebuild.md
EOF
}

# ----------------------------------------------------------------------------
# Argument parsing
# ----------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --node)        NODE="$2";            shift 2 ;;
    --namespace)   NAMESPACE="$2";       shift 2 ;;
    --timeout)     TIMEOUT="$2";         shift 2 ;;
    --dry-run)     DRY_RUN=1;            shift   ;;
    --image)       TEMP_POD_IMAGE="$2";  shift 2 ;;
    -h|--help)     usage; exit 0 ;;
    *) fatal "Unknown argument: $1. Run '${SCRIPT_NAME} --help' for usage." ;;
  esac
done

[[ -z "${NODE}" ]] && { usage; fatal "--node is required."; }

# ----------------------------------------------------------------------------
# Prerequisite checks
# ----------------------------------------------------------------------------
step "Checking prerequisites"

command -v kubectl >/dev/null 2>&1 || fatal "'kubectl' is not in PATH."
command -v jq      >/dev/null 2>&1 || fatal "'jq' is not in PATH."

# Verify we can talk to the cluster
kubectl cluster-info --request-timeout=10s >/dev/null 2>&1 \
  || fatal "Cannot reach the Kubernetes cluster. Check your kubeconfig."

# Verify the namespace exists
kubectl get namespace "${NAMESPACE}" >/dev/null 2>&1 \
  || fatal "Namespace '${NAMESPACE}' does not exist."

success "kubectl is configured and namespace '${NAMESPACE}' exists."

# ----------------------------------------------------------------------------
# Locate the Horizon Deployment
# ----------------------------------------------------------------------------
step "Locating Horizon Deployment for node '${NODE}'"

DEPLOY_NAME=$(kubectl get deployment -n "${NAMESPACE}" \
  -l "stellar.org/node=${NODE}" \
  -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)

if [[ -z "${DEPLOY_NAME}" ]]; then
  # Fallback: try the conventional name pattern used by the operator
  DEPLOY_NAME="horizon-${NODE}"
  kubectl get deployment "${DEPLOY_NAME}" -n "${NAMESPACE}" >/dev/null 2>&1 \
    || fatal "Cannot find a Deployment for node '${NODE}' in namespace '${NAMESPACE}'.
  Tried label selector 'stellar.org/node=${NODE}' and name 'horizon-${NODE}'.
  Check the node name and namespace, or supply the Deployment name directly."
fi

info "Found Deployment: ${DEPLOY_NAME}"

# ----------------------------------------------------------------------------
# Record current replica count so we can restore it later
# ----------------------------------------------------------------------------
ORIGINAL_REPLICAS=$(kubectl get deployment "${DEPLOY_NAME}" -n "${NAMESPACE}" \
  -o jsonpath='{.spec.replicas}')

info "Current replica count: ${ORIGINAL_REPLICAS}"

if [[ "${DRY_RUN}" -eq 1 ]]; then
  warn "DRY-RUN mode — no changes will be applied."
fi

# ----------------------------------------------------------------------------
# Step 1: Scale Horizon to zero
# ----------------------------------------------------------------------------
step "Step 1/4 — Scale Horizon to 0 replicas"

if [[ "${DRY_RUN}" -eq 0 ]]; then
  kubectl scale deployment "${DEPLOY_NAME}" -n "${NAMESPACE}" --replicas=0
  info "Waiting up to ${SCALE_WAIT}s for all Horizon pods to terminate..."

  DEADLINE=$(( $(date +%s) + SCALE_WAIT ))
  while true; do
    RUNNING=$(kubectl get pods -n "${NAMESPACE}" \
      -l "stellar.org/node=${NODE}" \
      --field-selector=status.phase=Running \
      -o jsonpath='{.items}' 2>/dev/null | jq 'length')
    [[ "${RUNNING}" -eq 0 ]] && break
    if [[ $(date +%s) -ge "${DEADLINE}" ]]; then
      fatal "Timed out waiting for Horizon pods to terminate. \
Investigate with: kubectl get pods -n ${NAMESPACE} -l stellar.org/node=${NODE}"
    fi
    info "  ${RUNNING} pod(s) still running — waiting..."
    sleep 5
  done
  success "All Horizon pods terminated."
else
  info "[DRY-RUN] Would run: kubectl scale deployment ${DEPLOY_NAME} -n ${NAMESPACE} --replicas=0"
fi

# ----------------------------------------------------------------------------
# Step 2: Detect storage type and clear Captive Core state
# ----------------------------------------------------------------------------
step "Step 2/4 — Clear Captive Core state under ${CAPTIVE_CORE_DIR}"

CAPTIVE_PVC=$(kubectl get pvc -n "${NAMESPACE}" \
  -l "stellar.org/component=captive-core,stellar.org/node=${NODE}" \
  -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)

if [[ -n "${CAPTIVE_PVC}" ]]; then
  # Storage is a named PVC — use a one-shot pod to wipe it
  info "Detected dedicated captive-core PVC: ${CAPTIVE_PVC}"
  RESET_POD="captive-core-reset-${NODE}"

  if [[ "${DRY_RUN}" -eq 0 ]]; then
    # Delete any leftover reset pod from a previous failed run
    kubectl delete pod "${RESET_POD}" -n "${NAMESPACE}" --ignore-not-found

    kubectl run "${RESET_POD}" \
      --namespace="${NAMESPACE}" \
      --image="${TEMP_POD_IMAGE}" \
      --restart=Never \
      --overrides="$(cat <<JSON
{
  "spec": {
    "volumes": [{
      "name": "captive-data",
      "persistentVolumeClaim": {"claimName": "${CAPTIVE_PVC}"}
    }],
    "containers": [{
      "name": "reset",
      "image": "${TEMP_POD_IMAGE}",
      "command": ["sh", "-c",
        "echo 'Contents before wipe:' && ls -lh ${CAPTIVE_CORE_DIR}/ && rm -rf ${CAPTIVE_CORE_DIR}/* && echo 'WIPE_COMPLETE'"],
      "volumeMounts": [{
        "name": "captive-data",
        "mountPath": "${CAPTIVE_CORE_DIR}"
      }]
    }]
  }
}
JSON
)"

    kubectl wait "pod/${RESET_POD}" -n "${NAMESPACE}" \
      --for=condition=Succeeded --timeout=120s \
      || fatal "Reset pod did not complete successfully. \
Check logs: kubectl logs -n ${NAMESPACE} ${RESET_POD}"

    RESET_OUTPUT=$(kubectl logs -n "${NAMESPACE}" "${RESET_POD}")
    echo "${RESET_OUTPUT}"

    if echo "${RESET_OUTPUT}" | grep -q "WIPE_COMPLETE"; then
      success "Captive Core PVC wiped successfully."
    else
      fatal "Wipe pod completed but WIPE_COMPLETE sentinel was not found in output."
    fi

    kubectl delete pod "${RESET_POD}" -n "${NAMESPACE}" --ignore-not-found
    success "Temporary reset pod cleaned up."
  else
    info "[DRY-RUN] Would spin up pod '${RESET_POD}' mounting PVC '${CAPTIVE_PVC}' and run:"
    info "[DRY-RUN]   rm -rf ${CAPTIVE_CORE_DIR}/*"
    info "[DRY-RUN] Would then delete pod '${RESET_POD}'"
  fi
else
  # Ephemeral storage (emptyDir / local container fs) — a fresh pod starts clean
  info "No dedicated captive-core PVC found for '${NODE}'."
  info "Captive Core state is ephemeral (emptyDir or container filesystem)."
  info "A new pod will start with a clean ${CAPTIVE_CORE_DIR} automatically."
fi

# ----------------------------------------------------------------------------
# Step 3: Scale Horizon back up
# ----------------------------------------------------------------------------
step "Step 3/4 — Scale Horizon back up (replicas=${ORIGINAL_REPLICAS})"

if [[ "${DRY_RUN}" -eq 0 ]]; then
  kubectl scale deployment "${DEPLOY_NAME}" -n "${NAMESPACE}" \
    --replicas="${ORIGINAL_REPLICAS}"
  kubectl rollout status deployment "${DEPLOY_NAME}" -n "${NAMESPACE}" \
    --timeout=120s
  success "Horizon Deployment scaled back to ${ORIGINAL_REPLICAS} replica(s)."
else
  info "[DRY-RUN] Would run: kubectl scale deployment ${DEPLOY_NAME} \
-n ${NAMESPACE} --replicas=${ORIGINAL_REPLICAS}"
fi

# ----------------------------------------------------------------------------
# Step 4: Wait for Captive Core to catch up and Horizon to become healthy
# ----------------------------------------------------------------------------
step "Step 4/4 — Waiting for Horizon to become healthy (timeout=${TIMEOUT}s)"

if [[ "${DRY_RUN}" -eq 1 ]]; then
  info "[DRY-RUN] Would tail logs and poll http://localhost:${HORIZON_PORT}/health"
  info "[DRY-RUN] until status==healthy or timeout=${TIMEOUT}s is reached."
  success "Dry-run complete. No changes were made."
  exit 0
fi

# Give the pod a moment to start
sleep 10

# Find the new Horizon pod name
POD_NAME=""
POLL_DEADLINE=$(( $(date +%s) + 60 ))
while [[ -z "${POD_NAME}" ]]; do
  POD_NAME=$(kubectl get pods -n "${NAMESPACE}" \
    -l "stellar.org/node=${NODE}" \
    --field-selector=status.phase=Running \
    -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)
  if [[ $(date +%s) -ge "${POLL_DEADLINE}" ]]; then
    fatal "Horizon pod did not enter Running phase within 60s. \
Check: kubectl get pods -n ${NAMESPACE} -l stellar.org/node=${NODE}"
  fi
  [[ -z "${POD_NAME}" ]] && sleep 3
done

info "Horizon pod: ${POD_NAME}"

# Tail logs in background so the operator can observe catch-up progress
kubectl logs -n "${NAMESPACE}" "${POD_NAME}" -c horizon -f --tail=50 &
LOG_PID=$!
# Ensure the background log tailer is killed when this script exits
trap 'kill "${LOG_PID}" 2>/dev/null || true' EXIT

HEALTH_DEADLINE=$(( $(date +%s) + TIMEOUT ))
HEALTHY=0

while [[ $(date +%s) -lt "${HEALTH_DEADLINE}" ]]; do
  HEALTH_RESPONSE=$(kubectl exec -n "${NAMESPACE}" "${POD_NAME}" \
    -c horizon -- \
    curl -s --max-time 5 "http://localhost:${HORIZON_PORT}/health" 2>/dev/null || true)

  if [[ -n "${HEALTH_RESPONSE}" ]]; then
    STATUS=$(echo "${HEALTH_RESPONSE}" | jq -r '.status // "unknown"' 2>/dev/null || echo "parse_error")
    H_SEQ=$(echo "${HEALTH_RESPONSE}" | jq -r '.horizon_sequence // 0' 2>/dev/null || echo "0")
    C_SEQ=$(echo "${HEALTH_RESPONSE}" | jq -r '.core_sequence // 0' 2>/dev/null || echo "0")
    info "  health=${STATUS}  horizon_seq=${H_SEQ}  core_seq=${C_SEQ}"

    if [[ "${STATUS}" == "healthy" ]]; then
      HEALTHY=1
      break
    fi
  else
    info "  Health endpoint not yet reachable — Captive Core still initialising..."
  fi

  sleep 15
done

# Stop the log tailer
kill "${LOG_PID}" 2>/dev/null || true
trap - EXIT

echo ""
if [[ "${HEALTHY}" -eq 1 ]]; then
  success "Horizon is healthy. Captive Core catch-up complete."
  success "Node '${NODE}' is back in service."
  exit 0
else
  warn "Horizon did not reach 'healthy' within ${TIMEOUT}s."
  warn "The node may still be catching up — monitor with:"
  warn "  kubectl logs -n ${NAMESPACE} ${POD_NAME} -c horizon -f"
  warn "  kubectl exec -n ${NAMESPACE} ${POD_NAME} -c horizon -- curl -s http://localhost:${HORIZON_PORT}/health | jq ."
  warn "Refer to docs/operations/captive-core-rebuild.md for manual recovery steps."
  exit 1
fi
