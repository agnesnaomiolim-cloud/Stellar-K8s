#!/usr/bin/env bash
#
# verify.sh - run the bare-metal acceptance checklist against the lab cluster.
#
# This is the executable form of docs/infrastructure/bare-metal.md §8.3/§8.4.
# It validates the two things the issue calls out explicitly:
#   - a node can be provisioned from local disk volumes, and
#   - the data really survives pod recreation (node-local storage is only
#     trustworthy once that has been proven).
#
# Set LAB_SKIP_NETWORK_CHECKS=1 to skip the bond/VLAN reachability checks
# (useful when the host bridge cannot forward 802.1Q frames).
#
# Usage: ./verify.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
# shellcheck source=examples/bare-metal/lab/cluster.env
source "${SCRIPT_DIR}/cluster.env"

export KUBECONFIG="${KUBECONFIG_PATH}"
NODES_FILE="${LAB_STATE_DIR}/nodes.tsv"
PROBE_NAMESPACE="stellar-lab-probe"
PROBE_MANIFEST="${ROOT_DIR}/examples/bare-metal/lab/local-volume-probe.yaml"
STELLAR_PVC="validator-mainnet-nvme-data"
STELLAR_STS_LABEL="stellar.org/name=validator-mainnet-nvme"

PASSED=0
FAILED=0

log() { printf '==> %s\n' "$*"; }
pass() { PASSED=$((PASSED + 1)); printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { FAILED=$((FAILED + 1)); printf '  \033[31mFAIL\033[0m %s\n' "$*"; }

check() {
  local description="$1"
  shift
  if "$@"; then
    pass "${description}"
  else
    fail "${description}"
  fi
}

# Run a command inside a hostPath debug pod pinned to a specific node.
host_debug() {
  local node="$1"
  shift
  local pod="lab-debug-${node}"
  cat <<EOF | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata:
  name: ${pod}
  namespace: ${PROBE_NAMESPACE}
  labels:
    app: lab-debug
spec:
  nodeName: ${node}
  restartPolicy: Never
  hostNetwork: true
  containers:
    - name: debug
      image: busybox:1.36
      command: ["sh", "-c", "sleep 300"]
      volumeMounts:
        - name: host-root
          mountPath: /host
          readOnly: true
  volumes:
    - name: host-root
      hostPath:
        path: /
EOF
  kubectl -n "${PROBE_NAMESPACE}" wait --for=condition=Ready "pod/${pod}" --timeout=120s >/dev/null
  local status=0
  kubectl -n "${PROBE_NAMESPACE}" exec "${pod}" -- "$@" || status=$?
  kubectl -n "${PROBE_NAMESPACE}" delete "pod/${pod}" --wait=false >/dev/null 2>&1 || true
  return "${status}"
}

node_for_pod() {
  kubectl -n "$1" get pod "$2" -o jsonpath='{.spec.nodeName}'
}

main() {
  [[ -f "${NODES_FILE}" ]] || { echo "run create-lab.sh first" >&2; exit 1; }
  kubectl get nodes >/dev/null || { echo "cannot reach cluster" >&2; exit 1; }

  local expected_nodes role name ip mac scp_ip
  expected_nodes=$((CONTROLPLANES + WORKERS))

  log "1/9 Cluster is healthy"
  check "all ${expected_nodes} nodes are Ready" \
    bash -c "[[ \$(kubectl get nodes --no-headers | awk '\$2 == \"Ready\"' | wc -l) -eq ${expected_nodes} ]]"

  log "2/9 Local StorageClass is configured for node-local binding"
  check "stellar-nvme-local exists" kubectl get storageclass stellar-nvme-local
  check "stellar-nvme-local uses WaitForFirstConsumer" \
    bash -c "[[ \$(kubectl get storageclass stellar-nvme-local -o jsonpath='{.volumeBindingMode}') == 'WaitForFirstConsumer' ]]"
  check "stellar-nvme-local retains data" \
    bash -c "[[ \$(kubectl get storageclass stellar-nvme-local -o jsonpath='{.reclaimPolicy}') == 'Retain' ]]"

  log "3/9 A volume can be provisioned and bound on local disk"
  kubectl create namespace "${PROBE_NAMESPACE}" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  kubectl apply -f "${PROBE_MANIFEST}" >/dev/null
  check "probe PVC binds" \
    kubectl -n "${PROBE_NAMESPACE}" wait --for=jsonpath='{.status.phase}'=Bound \
      pvc/local-volume-probe --timeout=180s
  check "probe pod becomes Ready" \
    kubectl -n "${PROBE_NAMESPACE}" wait --for=condition=Ready \
      pod/local-volume-probe --timeout=180s

  log "4/9 The volume lands on the NVMe-labelled node"
  local probe_node
  probe_node="$(node_for_pod "${PROBE_NAMESPACE}" local-volume-probe)"
  if [[ -z "${probe_node}" ]]; then
    fail "could not determine the probe pod's node"
  else
    check "probe node ${probe_node} carries stellar.io/nvme=true" \
      bash -c "[[ \$(kubectl get node '${probe_node}' -o jsonpath='{.metadata.labels.stellar\\.io/nvme}') == 'true' ]]"
    check "volume directory exists on ${probe_node}:/var/mnt/stellar/local-path" \
      host_debug "${probe_node}" test -d /host/var/mnt/stellar/local-path
  fi

  log "5/9 Data survives pod recreation (the check that matters)"
  kubectl -n "${PROBE_NAMESPACE}" exec local-volume-probe -- \
    sh -c 'echo persistence-probe > /data/.probe && sync'
  kubectl -n "${PROBE_NAMESPACE}" delete pod local-volume-probe --wait=true >/dev/null
  kubectl apply -f "${PROBE_MANIFEST}" >/dev/null
  kubectl -n "${PROBE_NAMESPACE}" wait --for=condition=Ready pod/local-volume-probe --timeout=180s >/dev/null
  check "probe file survived pod re-creation" \
    bash -c "[[ \$(kubectl -n ${PROBE_NAMESPACE} exec local-volume-probe -- cat /data/.probe 2>/dev/null) == persistence-probe ]]"

  log "6/9 The example StellarNode provisions its own local volume"
  check "StellarNode data PVC binds" \
    kubectl -n stellar wait --for=jsonpath='{.status.phase}'=Bound \
      "pvc/${STELLAR_PVC}" --timeout=300s 2>/dev/null

  log "7/9 The StellarNode pod is scheduled on a local-disk node"
  local sts_node
  sts_node="$(kubectl -n stellar get pod -l "${STELLAR_STS_LABEL}" \
    -o jsonpath='{.items[0].spec.nodeName}' 2>/dev/null || true)"
  if [[ -z "${sts_node}" ]]; then
    fail "no StellarNode pod is scheduled yet"
  else
    check "StellarNode pod placed on ${sts_node} (nvme=true)" \
      bash -c "[[ \$(kubectl get node '${sts_node}' -o jsonpath='{.metadata.labels.stellar\\.io/nvme}') == 'true' ]]"
  fi

  log "8/9 SCP VLAN and bonding"
  if [[ "${LAB_SKIP_NETWORK_CHECKS:-0}" == "1" ]]; then
    log "   skipped (LAB_SKIP_NETWORK_CHECKS=1)"
  else
    local first_node first_scp peer_scp
    first_node="$(awk -F'\t' 'NR == 1 { print $2 }' "${NODES_FILE}")"
    first_scp="$(awk -F'\t' 'NR == 1 { print $5 }' "${NODES_FILE}")"
    peer_scp="$(awk -F'\t' 'NR == 2 { print $5 }' "${NODES_FILE}")"
    check "${first_node} bond0 has an address" \
      host_debug "${first_node}" sh -c 'ip -brief addr show bond0 | grep -q "inet "'
    check "${first_node} bond0 has a VLAN ${LAB_SCP_VLAN} sub-interface" \
      host_debug "${first_node}" test -d "/sys/class/net/bond0.${LAB_SCP_VLAN}"
    check "${first_scp} can reach ${peer_scp} over VLAN ${LAB_SCP_VLAN}" \
      host_debug "${first_node}" ping -c 2 -W 3 "${peer_scp}"
  fi

  log "9/9 No cloud provider dependency"
  check "no cloud provider errors in cluster events" \
    bash -c "! kubectl get events -A -o jsonpath='{range .items[*]}{.message}{\"\\n\"}{end}' | grep -qiE 'aws|gce|azure|cloud-controller'"

  printf '\n'
  printf '─────────────────────────────────────────────\n'
  printf 'Result: %d passed, %d failed\n' "${PASSED}" "${FAILED}"
  printf '─────────────────────────────────────────────\n'

  if [[ "${FAILED}" -gt 0 ]]; then
    printf 'Bare-metal validation FAILED\n'
    exit 1
  fi
  printf 'Bare-metal validation PASSED\n'
}

main "$@"
