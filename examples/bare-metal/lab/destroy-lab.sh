#!/usr/bin/env bash
#
# destroy-lab.sh - tear down the bare-metal validation lab.
#
# Destroys the lab VMs and network and removes the local state directory
# (kubeconfig, Talos config, generated patches, disk images).
#
# Usage: ./destroy-lab.sh [--force]
#   --force   skip the interactive confirmation prompt

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=examples/bare-metal/lab/cluster.env
source "${SCRIPT_DIR}/cluster.env"

LIBVIRT_URI="${LIBVIRT_URI:-qemu:///system}"
FORCE=0
for arg in "$@"; do
  case "${arg}" in
    --force) FORCE=1 ;;
    -h | --help)
      echo "Usage: ./destroy-lab.sh [--force]"
      exit 0
      ;;
    *) echo "unknown argument: ${arg}" >&2; exit 2 ;;
  esac
done

log() { printf '==> %s\n' "$*"; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "ERROR: '$1' is required" >&2; exit 1; }; }
need virsh

if [[ "${FORCE}" -ne 1 ]]; then
  read -r -p "Destroy the '${LAB_NAME}' lab (VMs, network, state in ${LAB_STATE_DIR})? [y/N] " reply
  case "${reply}" in
    [yY] | [yY][eE][sS]) ;;
    *) echo "aborted"; exit 0 ;;
  esac
fi

# Domains first, so the network can be released.
if virsh --connect "${LIBVIRT_URI}" list --all --name >/dev/null 2>&1; then
  while read -r domain; do
    [[ -n "${domain}" ]] || continue
    case "${domain}" in
      "${LAB_NAME}"-*)
        log "Destroying VM ${domain}"
        virsh --connect "${LIBVIRT_URI}" destroy "${domain}" >/dev/null 2>&1 || true
        virsh --connect "${LIBVIRT_URI}" undefine "${domain}" --remove-all-storage >/dev/null 2>&1 || true
        ;;
    esac
  done < <(virsh --connect "${LIBVIRT_URI}" list --all --name)
fi

log "Removing libvirt network ${LAB_NET}"
virsh --connect "${LIBVIRT_URI}" net-destroy "${LAB_NET}" >/dev/null 2>&1 || true
virsh --connect "${LIBVIRT_URI}" net-undefine "${LAB_NET}" >/dev/null 2>&1 || true

log "Removing state directory ${LAB_STATE_DIR}"
rm -rf "${LAB_STATE_DIR}"

log "Lab destroyed."
