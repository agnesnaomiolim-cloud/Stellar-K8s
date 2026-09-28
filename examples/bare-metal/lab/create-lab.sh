#!/usr/bin/env bash
#
# create-lab.sh - build the bare-metal validation lab on a KVM host.
#
# Creates an isolated libvirt network plus CONTROLPLANES control-plane and
# WORKERS worker VMs, boots them from the Talos metal ISO, and applies the
# machine configuration derived from docs/infrastructure/bare-metal.md §4A.
# Worker VMs get two extra disks that stand in for local NVMe.
#
# Storage/network semantics exercised here are identical to production:
#   - Talos machine config (bond, VLAN, disk mounts, kubelet extraMounts)
#   - WaitForFirstConsumer + node-local provisioning (see bootstrap-cluster.sh)
#
# Requirements: a Linux host with KVM (/dev/kvm), libvirt, virt-install,
# talosctl, curl. Run as a user in the 'libvirt' group, or with sudo.
#
# Usage: ./create-lab.sh
#
# See examples/bare-metal/lab/README.md.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=examples/bare-metal/lab/cluster.env
source "${SCRIPT_DIR}/cluster.env"

LIBVIRT_URI="${LIBVIRT_URI:-qemu:///system}"
MAINTENANCE_TIMEOUT_SECONDS="${MAINTENANCE_TIMEOUT_SECONDS:-600}"
BOOTSTRAP_TIMEOUT_SECONDS="${BOOTSTRAP_TIMEOUT_SECONDS:-900}"

log() { printf '==> %s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed"; }

for tool in virsh virt-install talosctl kubectl curl; do need "${tool}"; done
virsh --connect "${LIBVIRT_URI}" version >/dev/null 2>&1 \
  || die "cannot talk to libvirt at ${LIBVIRT_URI} (is libvirtd running and are you in the 'libvirt' group?)"

case "${LAB_SUBNET}" in
  */24) ;;
  *) die "LAB_SUBNET must be a /24 (got '${LAB_SUBNET}')" ;;
esac

mkdir -p "${LAB_STATE_DIR}" "${TALOS_ISO_CACHE}"
NODES_FILE="${LAB_STATE_DIR}/nodes.tsv"
TALOS_DIR="${LAB_STATE_DIR}/talos"
mkdir -p "${TALOS_DIR}"

# ── Inventory ─────────────────────────────────────────────────────────────────
# Columns: role  name  ip  mac  scp_ip
build_inventory() {
  local index=0 i mac_suffix role name ip scp_ip
  : >"${NODES_FILE}"

  for ((i = 0; i < CONTROLPLANES; i++)); do
    role="controlplane"
    name="stellar-cp-$(printf '%02d' $((i + 1)))"
    ip="${LAB_SUBNET%.*}.$((CP_IP_BASE + i))"
    mac_suffix="$(printf '01:%02x' $((i + 1)))"
    scp_ip="${LAB_SCP_PREFIX}.$((LAB_SCP_HOST_BASE + index))"
    printf '%s\t%s\t%s\t%s\t%s\n' \
      "${role}" "${name}" "${ip}" "52:54:00:7a:00:${mac_suffix}" "${scp_ip}" >>"${NODES_FILE}"
    index=$((index + 1))
  done

  for ((i = 0; i < WORKERS; i++)); do
    role="worker"
    name="stellar-val-$(printf '%02d' $((i + 1)))"
    ip="${LAB_SUBNET%.*}.$((WORKER_IP_BASE + i))"
    mac_suffix="$(printf '02:%02x' $((i + 1)))"
    scp_ip="${LAB_SCP_PREFIX}.$((LAB_SCP_HOST_BASE + index))"
    printf '%s\t%s\t%s\t%s\t%s\n' \
      "${role}" "${name}" "${ip}" "52:54:00:7a:00:${mac_suffix}" "${scp_ip}" >>"${NODES_FILE}"
    index=$((index + 1))
  done

  log "Node inventory written to ${NODES_FILE}"
}

# ── Talos ISO ─────────────────────────────────────────────────────────────────
ensure_iso() {
  local iso="${TALOS_ISO_CACHE}/metal-amd64-${TALOS_VERSION}.iso"
  if [[ ! -s "${iso}" ]]; then
    log "Downloading Talos ${TALOS_VERSION} metal ISO"
    curl -fL --retry 5 --retry-delay 5 \
      -o "${iso}.part" \
      "https://github.com/siderolabs/talos/releases/download/${TALOS_VERSION}/metal-amd64.iso"
    mv "${iso}.part" "${iso}"
  fi
  printf '%s\n' "${iso}"
}

# ── libvirt network ───────────────────────────────────────────────────────────
define_network() {
  local net_xml="${LAB_STATE_DIR}/network.xml" role name ip mac scp_ip
  {
    printf '%s\n' '<network>'
    printf '  <name>%s</name>\n' "${LAB_NET}"
    printf '%s\n' "  <forward mode='nat'/>"
    printf "  <bridge name='%s' stp='on' delay='0'/>\n" "${LAB_BRIDGE}"
    printf "  <domain name='%s' localOnly='no'/>\n" "${LAB_DOMAIN}"
    printf "  <ip address='%s' netmask='255.255.255.0'>\n" "${LAB_GATEWAY}"
    printf '%s\n' '    <dhcp>'
    printf "      <range start='%s' end='%s'/>\n" "${LAB_DHCP_START}" "${LAB_DHCP_END}"
    while IFS=$'\t' read -r role name ip mac scp_ip; do
      printf "      <host mac='%s' name='%s' ip='%s'/>\n" "${mac}" "${name}" "${ip}"
    done <"${NODES_FILE}"
    printf '%s\n' '    </dhcp>'
    printf '%s\n' '  </ip>'
    printf '%s\n' '</network>'
  } >"${net_xml}"

  log "Defining libvirt network ${LAB_NET}"
  virsh --connect "${LIBVIRT_URI}" net-destroy "${LAB_NET}" >/dev/null 2>&1 || true
  virsh --connect "${LIBVIRT_URI}" net-undefine "${LAB_NET}" >/dev/null 2>&1 || true
  virsh --connect "${LIBVIRT_URI}" net-define "${net_xml}" >/dev/null
  virsh --connect "${LIBVIRT_URI}" net-start "${LAB_NET}" >/dev/null
  virsh --connect "${LIBVIRT_URI}" net-autostart "${LAB_NET}" >/dev/null
}

# ── VMs ───────────────────────────────────────────────────────────────────────
create_vm() {
  local role="$1" name="$2" ip="$3" mac="$4" iso="$5"
  local disk_dir="${LAB_STATE_DIR}/${name}"
  mkdir -p "${disk_dir}"

  log "Creating VM ${name} (${role}, ${ip})"
  virsh --connect "${LIBVIRT_URI}" destroy "${name}" >/dev/null 2>&1 || true
  virsh --connect "${LIBVIRT_URI}" undefine "${name}" --remove-all-storage >/dev/null 2>&1 || true
  rm -f "${disk_dir}"/*.qcow2

  local -a args=(
    --connect "${LIBVIRT_URI}"
    --name "${name}"
    --memory "${VM_MEMORY_MB}"
    --vcpus "${VM_CPUS}"
    --cpu host-passthrough
    --boot cdrom,hd
    --network "network=${LAB_NET},model=virtio,mac=${mac}"
    --network "network=${LAB_NET},model=virtio"
    --disk "path=${disk_dir}/root.qcow2,size=${VM_ROOT_DISK_GB},format=qcow2,bus=virtio"
    --graphics none
    --console "pty,target_type=serial"
    --noautoconsole
  )

  # Workers carry two extra disks that stand in for local NVMe. On real
  # hardware use bus=nvme (see docs §8.1); virtio keeps the lab portable across
  # libvirt versions and the storage semantics under test are identical.
  if [[ "${role}" == "worker" ]]; then
    args+=(--disk "path=${disk_dir}/data-1.qcow2,size=${VM_DATA_DISK_GB},format=qcow2,bus=virtio")
    args+=(--disk "path=${disk_dir}/data-2.qcow2,size=${VM_DATA_DISK_GB},format=qcow2,bus=virtio")
  fi

  virt-install "${args[@]}" --cdrom "${iso}"
}

# ── Talos configuration ───────────────────────────────────────────────────────
cluster_patch() {
  local cp1_ip="$1"
  cat >"${TALOS_DIR}/patch-cluster.yaml" <<EOF
cluster:
  network:
    cni:
      name: none
    podSubnets:
      - 10.244.0.0/16
    serviceSubnets:
      - 10.96.0.0/12
  proxy:
    disabled: true
  apiServer:
    certSANs:
      - ${cp1_ip}
      - 127.0.0.1
  allowSchedulingOnControlPlanes: false
EOF
}

node_patch() {
  local role="$1" name="$2" ip="$3" scp_ip="$4"
  local out="${TALOS_DIR}/patch-${name}.yaml"
  local class="${role}"

  {
    cat <<EOF
machine:
  install:
    disk: /dev/vda
    image: ghcr.io/siderolabs/installer:${TALOS_VERSION}
    wipe: true
  network:
    hostname: ${name}
    nameservers:
      - 1.1.1.1
    interfaces:
      - interface: bond0
        mtu: 1500
        bond:
          mode: active-backup
          miimon: 100
          deviceSelectors:
            - driver: virtio
        addresses:
          - ${ip}/24
        routes:
          - network: 0.0.0.0/0
            gateway: ${LAB_GATEWAY}
        vlans:
          - vlanId: ${LAB_SCP_VLAN}
            mtu: 1500
            addresses:
              - ${scp_ip}/24
EOF

    if [[ "${role}" == "worker" ]]; then
      cat <<EOF
  nodeLabels:
    stellar.io/node-class: ${class}
    stellar.io/nvme: "true"
    stellar.io/nvme-count: "2"
    topology.kubernetes.io/zone: lab
  disks:
    - device: /dev/vdb
      partitions:
        - mountpoint: /var/mnt/stellar
    - device: /dev/vdc
      partitions:
        - mountpoint: /var/mnt/archive
  kubelet:
    extraMounts:
      - destination: /var/mnt/stellar
        type: bind
        source: /var/mnt/stellar
        options:
          - bind
          - rshared
          - rw
      - destination: /var/mnt/archive
        type: bind
        source: /var/mnt/archive
        options:
          - bind
          - rshared
          - rw
EOF
    else
      cat <<EOF
  nodeLabels:
    stellar.io/node-class: ${class}
    topology.kubernetes.io/zone: lab
EOF
    fi
  } >"${out}"

  printf '%s\n' "${out}"
}

wait_for_maintenance() {
  local ip="$1" deadline=$((SECONDS + MAINTENANCE_TIMEOUT_SECONDS))
  log "Waiting for ${ip} to enter maintenance mode (timeout ${MAINTENANCE_TIMEOUT_SECONDS}s)"
  while ((SECONDS < deadline)); do
    if talosctl --insecure --nodes "${ip}" version >/dev/null 2>&1; then
      return 0
    fi
    sleep 5
  done
  die "node ${ip} did not reach maintenance mode"
}

configure_cluster() {
  local cp1_ip="$1" role name ip mac scp_ip role_file patch
  log "Generating Talos cluster configuration"
  rm -rf "${TALOS_DIR:?}/config"
  # The installer image is set explicitly in each node patch, so no flag is needed here.
  talosctl gen config "${LAB_NAME}" "https://${cp1_ip}:6443" \
    --output-dir "${TALOS_DIR}/config" >/dev/null

  cluster_patch "${cp1_ip}"

  while IFS=$'\t' read -r role name ip mac scp_ip; do
    patch="$(node_patch "${role}" "${name}" "${ip}" "${scp_ip}")"
    role_file="${TALOS_DIR}/config/worker.yaml"
    [[ "${role}" == "controlplane" ]] && role_file="${TALOS_DIR}/config/controlplane.yaml"

    log "Applying machine config to ${name} (${ip})"
    talosctl apply-config --insecure \
      --nodes "${ip}" \
      --file "${role_file}" \
      --config-patch "@${TALOS_DIR}/patch-cluster.yaml" \
      --config-patch "@${patch}"
  done <"${NODES_FILE}"

  log "Bootstrapping etcd on ${cp1_ip}"
  talosctl bootstrap --nodes "${cp1_ip}" --endpoints "${cp1_ip}"
}

wait_for_kubeconfig() {
  local cp1_ip="$1" deadline=$((SECONDS + BOOTSTRAP_TIMEOUT_SECONDS))
  log "Waiting for the Kubernetes API to become available"
  while ((SECONDS < deadline)); do
    if talosctl kubeconfig "${KUBECONFIG_PATH}" \
      --nodes "${cp1_ip}" --endpoints "${cp1_ip}" --force >/dev/null 2>&1 \
      && KUBECONFIG="${KUBECONFIG_PATH}" kubectl get --raw /readyz >/dev/null 2>&1; then
      return 0
    fi
    sleep 10
  done
  die "Kubernetes API did not become ready within ${BOOTSTRAP_TIMEOUT_SECONDS}s"
}

main() {
  build_inventory
  local iso cp1_ip
  iso="$(ensure_iso)"
  cp1_ip="$(awk -F'\t' '$1 == "controlplane" { print $3; exit }' "${NODES_FILE}")"
  [[ -n "${cp1_ip}" ]] || die "no control-plane node found in inventory"

  define_network

  local role name ip mac scp_ip
  while IFS=$'\t' read -r role name ip mac scp_ip; do
    create_vm "${role}" "${name}" "${ip}" "${mac}" "${iso}"
  done <"${NODES_FILE}"

  while IFS=$'\t' read -r role name ip mac scp_ip; do
    wait_for_maintenance "${ip}"
  done <"${NODES_FILE}"

  configure_cluster "${cp1_ip}"
  wait_for_kubeconfig "${cp1_ip}"

  export KUBECONFIG="${KUBECONFIG_PATH}"
  log "Talos cluster is up. Nodes:"
  kubectl get nodes -o wide || true

  cat <<EOF

Lab infrastructure is ready.
  KUBECONFIG=${KUBECONFIG_PATH}
  State:     ${LAB_STATE_DIR}

Next:
  ./bootstrap-cluster.sh    # CNI, storage provisioner, operator, StellarNode
  ./verify.sh               # run the acceptance checklist
  ./destroy-lab.sh          # tear everything down
EOF
}

main "$@"
