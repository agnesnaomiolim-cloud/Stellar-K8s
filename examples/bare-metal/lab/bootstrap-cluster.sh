#!/usr/bin/env bash
#
# bootstrap-cluster.sh - install the on-prem add-ons a StellarNode needs.
#
# Runs after create-lab.sh and installs, in order:
#   1. Cilium (kube-proxy replacement, native routing)
#   2. Local Path Provisioner, pointed at the mounted data disk
#   3. The bare-metal StorageClasses (examples/bare-metal/storage-class.yaml)
#   4. The Stellar-K8s operator (no cloud dependencies)
#   5. A namespace + placeholder seed secret, then the example StellarNode
#
# MetalLB is intentionally NOT installed here: L2 VIP advertisement needs a
# real L2 segment, so §6.4 is validated on hardware, not in the nested lab.
#
# Usage: ./bootstrap-cluster.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
# shellcheck source=examples/bare-metal/lab/cluster.env
source "${SCRIPT_DIR}/cluster.env"

export KUBECONFIG="${KUBECONFIG_PATH}"
NODES_FILE="${LAB_STATE_DIR}/nodes.tsv"

log() { printf '==> %s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed"; }

for tool in kubectl helm; do need "${tool}"; done
[[ -f "${NODES_FILE}" ]] || die "run create-lab.sh first (missing ${NODES_FILE})"
kubectl get nodes >/dev/null || die "cannot reach the cluster with KUBECONFIG=${KUBECONFIG}"

install_cilium() {
  log "Installing Cilium (kube-proxy replacement, native routing)"
  helm repo add cilium https://helm.cilium.io/ >/dev/null 2>&1 || true
  helm repo update >/dev/null
  helm upgrade --install cilium "${CILIUM_CHART}" --namespace kube-system \
    --set kubeProxyReplacement=true \
    --set routingMode=native \
    --set autoDirectNodeRoutes=true \
    --set ipv4NativeRoutingCIDR=10.244.0.0/16 \
    --set devices='{bond0}' \
    --set bpf.masquerade=true \
    --wait --timeout 10m
  kubectl -n kube-system rollout status ds/cilium --timeout=10m
}

install_local_path_provisioner() {
  log "Installing Local Path Provisioner ${LOCAL_PATH_VERSION}"
  kubectl apply -f \
    "https://raw.githubusercontent.com/rancher/local-path-provisioner/${LOCAL_PATH_VERSION}/deploy/local-path-storage.yaml"
  kubectl -n local-path-storage rollout status deploy/local-path-provisioner --timeout=5m

  # Point the provisioner at the disk mounted by the Talos machine config.
  local node_paths="" role name ip mac scp_ip
  while IFS=$'\t' read -r role name ip mac scp_ip; do
    [[ "${role}" == "worker" ]] || continue
    node_paths+="      {\"node\": \"${name}\", \"paths\": [\"/var/mnt/stellar/local-path\"]},"$'\n'
  done <"${NODES_FILE}"

  log "Overriding local-path-config for the NVMe node paths"
  kubectl -n local-path-storage create configmap local-path-config \
    --from-literal=config.json="{
    \"nodePathMap\": [
${node_paths}      {\"node\": \"DEFAULT_PATH_FOR_NON_LISTED_NODES\", \"paths\": [\"/opt/local-path-provisioner\"]}
    ]
  }" \
    --dry-run=client -o yaml | kubectl apply -f -

  log "Applying bare-metal StorageClasses"
  kubectl apply -f "${ROOT_DIR}/examples/bare-metal/storage-class.yaml"
}

install_operator() {
  log "Installing the Stellar-K8s operator"
  kubectl apply -f "${ROOT_DIR}/config/crd" >/dev/null
  helm upgrade --install stellar-operator "${ROOT_DIR}/charts/stellar-operator" \
    --namespace stellar-system --create-namespace \
    --wait --timeout 10m
  kubectl -n stellar-system rollout status deploy/stellar-operator --timeout=5m
}

deploy_example_node() {
  log "Creating namespaces and a placeholder seed secret"
  kubectl create namespace stellar --dry-run=client -o yaml | kubectl apply -f -
  kubectl -n stellar create secret generic validator-seed-mainnet \
    --from-literal=STELLAR_CORE_SEED='SAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' \
    --dry-run=client -o yaml | kubectl apply -f -

  log "Applying examples/bare-metal/validator-nvme.yaml"
  # The placeholder seed is intentionally not a usable key, so stellar-core will
  # not reach Ready in the lab. Storage validation does not depend on it:
  # verify.sh checks the PVC binding and pod placement, then proves persistence
  # with an independent probe pod.
  kubectl apply -f "${ROOT_DIR}/examples/bare-metal/validator-nvme.yaml"
}

main() {
  install_cilium
  install_local_path_provisioner
  install_operator
  deploy_example_node

  cat <<EOF

Cluster add-ons installed.
  kubectl --kubeconfig ${KUBECONFIG} get nodes
  kubectl --kubeconfig ${KUBECONFIG} get storageclass
  kubectl --kubeconfig ${KUBECONFIG} -n stellar get stellarnodes,pvc,pods

Next: ./verify.sh
EOF
}

main "$@"
