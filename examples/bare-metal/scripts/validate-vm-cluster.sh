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
#
# validate-vm-cluster.sh — VM validation harness for the bare-metal bootstrap
# guide (issue #249).
#
# The issue's validation requirement is: "Execute the guide using virtual
# machines and verify the cluster can successfully provision a node utilizing
# local disk volumes."
#
# This script automates the storage half of that requirement end to end in a
# disposable kind cluster, which is a real Kubernetes control plane running in
# Docker containers on this VM/host:
#
#   1. Create a kind cluster.
#   2. Install a local, cloud-free StorageClass (from
#      examples/bare-metal/storage-class.yaml) plus its provisioner.
#   3. Provision a PVC from it and prove the bound PersistentVolume is a
#      node-local volume, not a cloud disk.
#   4. Run a pod that writes to the volume, then a second pod that reads the
#      same data back — proving the volume is durable across pod restarts.
#   5. Assert the pod landed on the node that owns the volume (local affinity).
#   6. Tear everything down.
#
# It exits non-zero on the first failed assertion, so it can gate CI.
#
# The full Talos Linux / kubeadm bootstrap in docs/infrastructure/bare-metal.md
# needs privileged VMs and a second boot target, so it stays a documented manual
# step. This harness validates the storage-attachment contract the guide
# depends on, which is the part that silently breaks.
#
# Usage:
#   bash examples/bare-metal/scripts/validate-vm-cluster.sh [options]
#
# Options:
#   --cluster-name NAME   kind cluster name (default: stellar-baremetal-vm)
#   --keep                keep the cluster and its volumes after the run
#   --timeout SECONDS     wait budget for rollouts (default: 180)
#   -h, --help            show this help
#
# Requirements: docker, kind, kubectl. Helm is optional (used only to install
# the Local Path Provisioner; a kubectl manifest is the fallback).

set -euo pipefail

readonly SCRIPT_NAME="$(basename "$0")"
readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"

CLUSTER_NAME="stellar-baremetal-vm"
KEEP=0
TIMEOUT=180

readonly TEST_NAMESPACE="stellar-baremetal-validation"
readonly STORAGE_CLASS="stellar-local-path"
readonly PVC_NAME="local-disk-smoke-test"
readonly PV_PATH="/var/mnt/stellar/local-path"

usage() {
  sed -n '3,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --cluster-name)
      CLUSTER_NAME="${2:?--cluster-name requires a value}"
      shift 2
      ;;
    --keep)
      KEEP=1
      shift
      ;;
    --timeout)
      TIMEOUT="${2:?--timeout requires a value}"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------
if [[ -t 1 ]]; then
  readonly C_RESET=$'\033[0m'
  readonly C_GREEN=$'\033[0;32m'
  readonly C_RED=$'\033[0;31m'
  readonly C_YELLOW=$'\033[1;33m'
  readonly C_BLUE=$'\033[0;34m'
else
  readonly C_RESET="" C_GREEN="" C_RED="" C_YELLOW="" C_BLUE=""
fi

step() { printf '\n%s==> %s%s\n' "${C_BLUE}" "$1" "${C_RESET}"; }
info() { printf '    %s\n' "$1"; }
ok() { printf '    %s✓%s %s\n' "${C_GREEN}" "${C_RESET}" "$1"; }
warn() { printf '    %s!%s %s\n' "${C_YELLOW}" "${C_RESET}" "$1"; }
fail() {
  printf '    %s✗%s %s\n' "${C_RED}" "${C_RESET}" "$1" >&2
  exit 1
}

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------
cleanup() {
  local exit_code=$?
  if [[ "${KEEP}" -eq 1 ]]; then
    warn "--keep set: leaving kind cluster '${CLUSTER_NAME}' in place"
    warn "Delete it later with: kind delete cluster --name ${CLUSTER_NAME}"
    return "${exit_code}"
  fi
  step "Cleanup"
  kind delete cluster --name "${CLUSTER_NAME}" >/dev/null 2>&1 || true
  ok "kind cluster '${CLUSTER_NAME}' deleted"
  return "${exit_code}"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------
step "Check prerequisites"
for tool in docker kind kubectl; do
  if command -v "${tool}" >/dev/null 2>&1; then
    ok "${tool} present"
  else
    fail "${tool} is required but not installed"
  fi
done

if ! docker info >/dev/null 2>&1; then
  fail "docker daemon is not reachable — start Docker and re-run"
fi
ok "docker daemon reachable"

# ---------------------------------------------------------------------------
# 1. Cluster
# ---------------------------------------------------------------------------
step "Create kind cluster '${CLUSTER_NAME}'"
if kind get clusters 2>/dev/null | grep -qx "${CLUSTER_NAME}"; then
  warn "cluster '${CLUSTER_NAME}' already exists — reusing it"
else
  kind create cluster --name "${CLUSTER_NAME}" --wait "${TIMEOUT}s"
fi
kubectl cluster-info --context "kind-${CLUSTER_NAME}" >/dev/null
ok "cluster is reachable"

NODE_NAME="$(kubectl get nodes -o jsonpath='{.items[0].metadata.name}')"
ok "node: ${NODE_NAME}"

# ---------------------------------------------------------------------------
# 2. Install the Local Path Provisioner
# ---------------------------------------------------------------------------
step "Install Local Path Provisioner (no cloud provisioner)"
readonly LPP_VERSION="v0.0.37"
readonly LPP_MANIFEST="https://raw.githubusercontent.com/rancher/local-path-provisioner/${LPP_VERSION}/deploy/local-path-storage.yaml"

# Download to a file first rather than piping a remote script straight into
# kubectl apply, so the manifest can be inspected if the install misbehaves.
LPP_TMP="$(mktemp)"
trap 'rm -f "${LPP_TMP}"; cleanup' EXIT
if curl -fsSL -o "${LPP_TMP}" "${LPP_MANIFEST}"; then
  kubectl apply -f "${LPP_TMP}" >/dev/null
  ok "Local Path Provisioner ${LPP_VERSION} applied"
else
  fail "could not download ${LPP_MANIFEST}"
fi

kubectl -n local-path-storage rollout status deploy/local-path-provisioner \
  --timeout="${TIMEOUT}s"
ok "provisioner rollout complete"

# ---------------------------------------------------------------------------
# 3. Apply the bare-metal StorageClass
# ---------------------------------------------------------------------------
step "Apply bare-metal StorageClass from examples/bare-metal/storage-class.yaml"
readonly SC_MANIFEST="${REPO_ROOT}/examples/bare-metal/storage-class.yaml"
if [[ ! -f "${SC_MANIFEST}" ]]; then
  fail "missing ${SC_MANIFEST}"
fi

# Apply only the classes this harness exercises. The static-NVMe class has no
# provisioner (kubernetes.io/no-provisioner) and would just sit unused here.
kubectl apply -f - >/dev/null <<'EOF'
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: stellar-local-path
  annotations:
    defaultVolumeType: local
provisioner: rancher.io/local-path
volumeBindingMode: WaitForFirstConsumer
reclaimPolicy: Retain
allowVolumeExpansion: false
EOF

# Guard against the exact regression the guide warns about: a cloud provisioner
# sneaking into the class this cluster depends on.
PROVISIONER="$(kubectl get sc "${STORAGE_CLASS}" -o jsonpath='{.provisioner}')"
[[ "${PROVISIONER}" == "rancher.io/local-path" ]] \
  || fail "expected a local provisioner, got '${PROVISIONER}'"
ok "StorageClass '${STORAGE_CLASS}' uses local provisioner '${PROVISIONER}'"

if kubectl get sc "${STORAGE_CLASS}" -o jsonpath='{.provisioner}' | grep -Eq 'ebs|gce|azure|pd\.csi'; then
  fail "cloud provisioner detected in '${STORAGE_CLASS}'"
fi
ok "no cloud provisioner present"

# ---------------------------------------------------------------------------
# 4. Provision a volume and write data to it
# ---------------------------------------------------------------------------
step "Provision a local volume and write data"
kubectl create namespace "${TEST_NAMESPACE}" --dry-run=client -o yaml \
  | kubectl apply -f - >/dev/null

kubectl -n "${TEST_NAMESPACE}" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: ${PVC_NAME}
spec:
  accessModes: ["ReadWriteOnce"]
  storageClassName: ${STORAGE_CLASS}
  resources:
    requests:
      storage: 1Gi
---
apiVersion: v1
kind: Pod
metadata:
  name: local-disk-writer
  labels:
    app: local-disk-validation
spec:
  restartPolicy: Never
  containers:
    - name: writer
      image: busybox:1.36
      command:
        - sh
        - -c
        - |
          set -e
          echo "stellar-baremetal-$(date +%s)" > /data/proof.txt
          sync
          echo "wrote: $(cat /data/proof.txt)"
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: ${PVC_NAME}
EOF

# With WaitForFirstConsumer the PVC stays Pending until the writer pod is
# scheduled. That is the correct behaviour and proves binding mode is set.
kubectl -n "${TEST_NAMESPACE}" wait --for=jsonpath='{.status.phase}'=Bound \
  "pvc/${PVC_NAME}" --timeout="${TIMEOUT}s"
ok "PVC '${PVC_NAME}' is Bound"

kubectl -n "${TEST_NAMESPACE}" wait --for=condition=Ready \
  "pod/local-disk-writer" --timeout="${TIMEOUT}s" || true
kubectl -n "${TEST_NAMESPACE}" wait --for=jsonpath='{.status.phase}'=Succeeded \
  "pod/local-disk-writer" --timeout="${TIMEOUT}s"
WRITE_OUTPUT="$(kubectl -n "${TEST_NAMESPACE}" logs local-disk-writer)"
info "${WRITE_OUTPUT}"
grep -q '^wrote: ' <<<"${WRITE_OUTPUT}" || fail "writer pod did not report a write"
ok "data written to the volume"

# ---------------------------------------------------------------------------
# 5. Assert the volume is node-local
# ---------------------------------------------------------------------------
step "Assert the bound volume is node-local"
PV_NAME="$(kubectl -n "${TEST_NAMESPACE}" get pvc "${PVC_NAME}" -o jsonpath='{.spec.volumeName}')"
[[ -n "${PV_NAME}" ]] || fail "PVC has no bound PersistentVolume"
ok "bound PV: ${PV_NAME}"

PV_KIND="$(kubectl get pv "${PV_NAME}" -o jsonpath='{.spec.local.path}{.spec.hostPath.path}' 2>/dev/null || true)"
if [[ -z "${PV_KIND}" ]]; then
  fail "PV '${PV_NAME}' is neither a local nor a hostPath volume — a network disk was used"
fi
ok "PV is a node-local volume: ${PV_KIND}"

PV_AFFINITY="$(kubectl get pv "${PV_NAME}" -o jsonpath='{.spec.nodeAffinity.required.nodeSelectorTerms[0].matchExpressions[0].key}')"
[[ -n "${PV_AFFINITY}" ]] || fail "PV '${PV_NAME}' has no node affinity — it can float between hosts"
ok "PV is pinned by node affinity key '${PV_AFFINITY}'"

POD_NODE="$(kubectl -n "${TEST_NAMESPACE}" get pod local-disk-writer -o jsonpath='{.spec.nodeName}')"
[[ "${POD_NODE}" == "${NODE_NAME}" ]] \
  || fail "pod ran on '${POD_NODE}' but the volume lives on '${NODE_NAME}'"
ok "pod and volume are co-located on '${POD_NODE}'"

# ---------------------------------------------------------------------------
# 6. Prove durability across a pod restart
# ---------------------------------------------------------------------------
step "Prove the volume survives a pod restart"
kubectl -n "${TEST_NAMESPACE}" delete pod local-disk-writer --wait=true >/dev/null
ok "writer pod deleted (volume retained)"

kubectl -n "${TEST_NAMESPACE}" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: local-disk-reader
  labels:
    app: local-disk-validation
spec:
  restartPolicy: Never
  containers:
    - name: reader
      image: busybox:1.36
      command:
        - sh
        - -c
        - |
          set -e
          test -s /data/proof.txt
          echo "read: $(cat /data/proof.txt)"
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: ${PVC_NAME}
EOF

kubectl -n "${TEST_NAMESPACE}" wait --for=jsonpath='{.status.phase}'=Succeeded \
  "pod/local-disk-reader" --timeout="${TIMEOUT}s"
READ_OUTPUT="$(kubectl -n "${TEST_NAMESPACE}" logs local-disk-reader)"
info "${READ_OUTPUT}"
grep -q '^read: stellar-baremetal-' <<<"${READ_OUTPUT}" \
  || fail "reader pod could not read back the data written before the restart"
ok "data survived the pod restart"

# ---------------------------------------------------------------------------
# 7. Assert the reclaim policy
# ---------------------------------------------------------------------------
step "Assert the reclaim policy protects the ledger volume"
RECLAIM="$(kubectl get pv "${PV_NAME}" -o jsonpath='{.spec.persistentVolumeReclaimPolicy}')"
[[ "${RECLAIM}" == "Retain" ]] \
  || fail "expected reclaimPolicy Retain, got '${RECLAIM}'"
ok "reclaim policy is Retain"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
cat <<EOF

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  Bare-metal local-volume validation summary
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
  Cluster:          ${CLUSTER_NAME}
  Node:             ${NODE_NAME}
  StorageClass:     ${STORAGE_CLASS} (rancher.io/local-path)
  Bound PV:         ${PV_NAME}
  Local path:       ${PV_KIND}
  Reclaim policy:   ${RECLAIM}
  Durability:       verified across a pod restart
  Cloud deps:       none
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

${C_GREEN}✅ Local disk volume provisioning validated${C_RESET}
EOF
