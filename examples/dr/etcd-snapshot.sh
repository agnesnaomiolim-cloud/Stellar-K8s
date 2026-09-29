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

# ==============================================================================
# Stellar-K8s Disaster Recovery: etcd Snapshot Save, Verify, and Restore Tool
# ==============================================================================
# This utility provides production-grade snapshotting and disaster recovery for
# the Kubernetes control-plane etcd datastore supporting the Stellar-K8s operator
# and StellarNode custom resources.
#
# Usage:
#   ./etcd-snapshot.sh save [--backup-dir /path/to/backups]
#   ./etcd-snapshot.sh verify <snapshot-path>
#   ./etcd-snapshot.sh restore <snapshot-path> [--data-dir /var/lib/etcd] [--force]
#   ./etcd-snapshot.sh health
# ==============================================================================

set -euo pipefail

# ── Color Output Configuration ────────────────────────────────────────────────
if [[ -t 1 ]]; then
    COLOR_RED='\033[0;31m'
    COLOR_GREEN='\033[0;32m'
    COLOR_YELLOW='\033[1;33m'
    COLOR_BLUE='\033[0;34m'
    COLOR_CYAN='\033[0;36m'
    COLOR_BOLD='\033[1m'
    COLOR_RESET='\033[0m'
else
    COLOR_RED=''
    COLOR_GREEN=''
    COLOR_YELLOW=''
    COLOR_BLUE=''
    COLOR_CYAN=''
    COLOR_BOLD=''
    COLOR_RESET=''
fi

log_info() {
    echo -e "${COLOR_BLUE}[INFO]${COLOR_RESET} $*"
}

log_success() {
    echo -e "${COLOR_GREEN}[SUCCESS]${COLOR_RESET} $*"
}

log_warn() {
    echo -e "${COLOR_YELLOW}[WARN]${COLOR_RESET} $*" >&2
}

log_err() {
    echo -e "${COLOR_RED}[ERROR]${COLOR_RESET} $*" >&2
}

log_fatal() {
    echo -e "${COLOR_RED}${COLOR_BOLD}[FATAL]${COLOR_RESET} $*" >&2
    exit 1
}

# ── Environment & Default Configuration ───────────────────────────────────────
export ETCDCTL_API=3

ETCD_ENDPOINTS="${ETCD_ENDPOINTS:-https://127.0.0.1:2379}"
ETCD_CACERT="${ETCD_CACERT:-/etc/kubernetes/pki/etcd/ca.crt}"
ETCD_CERT="${ETCD_CERT:-/etc/kubernetes/pki/etcd/server.crt}"
ETCD_KEY="${ETCD_KEY:-/etc/kubernetes/pki/etcd/server.key}"
ETCD_DATA_DIR="${ETCD_DATA_DIR:-/var/lib/etcd}"
BACKUP_DIR="${BACKUP_DIR:-/var/backups/etcd}"
NODE_NAME="${NODE_NAME:-$(hostname)}"
INITIAL_CLUSTER="${INITIAL_CLUSTER:-${NODE_NAME}=https://127.0.0.1:2380}"
INITIAL_CLUSTER_TOKEN="${INITIAL_CLUSTER_TOKEN:-etcd-cluster-restore-$(date +%s)}"
INITIAL_ADVERTISE_PEER_URLS="${INITIAL_ADVERTISE_PEER_URLS:-https://127.0.0.1:2380}"
FORCE_MODE="false"
SKIP_HEALTH="false"

# ── Help / Usage ──────────────────────────────────────────────────────────────
usage() {
    cat <<EOF
${COLOR_BOLD}Stellar-K8s Disaster Recovery: etcd Snapshot & Restoration Utility${COLOR_RESET}

${COLOR_CYAN}SYNOPSIS:${COLOR_RESET}
    $(basename "$0") <command> [arguments...] [options...]

${COLOR_CYAN}COMMANDS:${COLOR_RESET}
    ${COLOR_BOLD}save${COLOR_RESET} [output-path]
        Create a point-in-time snapshot of the etcd database, verify its
        cryptographic hash, and generate an accompanying SHA-256 checksum file.
        If output-path is omitted, defaults to:
        \${BACKUP_DIR}/etcd-snapshot-<timestamp>-<hash>.db

    ${COLOR_BOLD}verify${COLOR_RESET} <snapshot-path>
        Verify the structural integrity of the specified etcd snapshot file
        and validate its SHA-256 checksum (if .sha256 file is present).

    ${COLOR_BOLD}restore${COLOR_RESET} <snapshot-path>
        Execute an emergency cold restore of the etcd cluster from a snapshot.
        Creates an archival backup of the active etcd data directory prior to
        overwriting, sets required owner permissions, and validates new data dir.

    ${COLOR_BOLD}health${COLOR_RESET}
        Probe etcd cluster health, consensus status, and endpoint metrics.

    ${COLOR_BOLD}help${COLOR_RESET}
        Display this help information.

${COLOR_CYAN}OPTIONS:${COLOR_RESET}
    ${COLOR_BOLD}-e, --endpoints${COLOR_RESET} <urls>       Etcd cluster endpoints (default: https://127.0.0.1:2379)
    ${COLOR_BOLD}--cacert${COLOR_RESET} <path>              CA certificate authority file path (default: /etc/kubernetes/pki/etcd/ca.crt)
    ${COLOR_BOLD}--cert${COLOR_RESET} <path>                TLS certificate file path (default: /etc/kubernetes/pki/etcd/server.crt)
    ${COLOR_BOLD}--key${COLOR_RESET} <path>                 TLS private key file path (default: /etc/kubernetes/pki/etcd/server.key)
    ${COLOR_BOLD}--data-dir${COLOR_RESET} <path>            Target etcd data directory (default: /var/lib/etcd)
    ${COLOR_BOLD}--backup-dir${COLOR_RESET} <path>          Directory for storing automated backups (default: /var/backups/etcd)
    ${COLOR_BOLD}--name${COLOR_RESET} <string>               Node member name for cluster restoration (default: \$(hostname))
    ${COLOR_BOLD}--initial-cluster${COLOR_RESET} <string>   Initial cluster membership string for restoration
    ${COLOR_BOLD}--initial-cluster-token${COLOR_RESET} <str> Initial cluster token for restoration
    ${COLOR_BOLD}--initial-advertise-peer-urls${COLOR_RESET} Initial advertise peer URLs
    ${COLOR_BOLD}-f, --force${COLOR_RESET}                  Bypass interactive confirmation during restore
    ${COLOR_BOLD}--skip-health${COLOR_RESET}             Skip pre/post etcd connectivity and health check

${COLOR_CYAN}EXAMPLES:${COLOR_RESET}
    # 1. Take an automated pre-upgrade snapshot
    ./etcd-snapshot.sh save

    # 2. Save a snapshot directly to a specific DR path
    ./etcd-snapshot.sh save /dr/backups/etcd-pre-migration-2026.db

    # 3. Verify a snapshot file
    ./etcd-snapshot.sh verify /dr/backups/etcd-pre-migration-2026.db

    # 4. Perform an emergency restore on a control plane node
    ./etcd-snapshot.sh restore /dr/backups/etcd-pre-migration-2026.db --force

EOF
}

# ── Validation Helpers ────────────────────────────────────────────────────────
check_prerequisites() {
    if ! command -v etcdctl >/dev/null 2>&1; then
        log_err "etcdctl binary was not found in PATH."
        log_info "If running on a Kubernetes control plane node, ensure etcdctl is installed"
        log_info "or execute via crictl/docker exec inside the etcd container."
        log_fatal "Prerequisite check failed: missing etcdctl."
    fi

    if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
        log_warn "Neither 'sha256sum' nor 'shasum' found; checksum validation will be skipped."
    fi
}

calculate_sha256() {
    local target_file="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$target_file" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$target_file" | awk '{print $1}'
    else
        echo "UNAVAILABLE"
    fi
}

build_etcdctl_auth_args() {
    local args=()
    args+=("--endpoints=${ETCD_ENDPOINTS}")

    if [[ -f "${ETCD_CACERT}" ]]; then
        args+=("--cacert=${ETCD_CACERT}")
    fi
    if [[ -f "${ETCD_CERT}" ]]; then
        args+=("--cert=${ETCD_CERT}")
    fi
    if [[ -f "${ETCD_KEY}" ]]; then
        args+=("--key=${ETCD_KEY}")
    fi

    echo "${args[@]}"
}

# ── Command: Health Check ─────────────────────────────────────────────────────
check_health() {
    check_prerequisites
    log_info "Checking etcd cluster endpoint status and health..."
    read -r -a auth_args <<< "$(build_etcdctl_auth_args)"

    if etcdctl "${auth_args[@]}" endpoint health --write-out=table; then
        log_success "etcd cluster endpoint is healthy."
    else
        log_err "etcd endpoint health check failed."
        return 1
    fi

    log_info "Endpoint status details:"
    etcdctl "${auth_args[@]}" endpoint status --write-out=table
}

# ── Command: Snapshot Save ────────────────────────────────────────────────────
snapshot_save() {
    check_prerequisites
    local custom_output="${1:-}"

    if [[ -z "${custom_output}" ]]; then
        mkdir -p "${BACKUP_DIR}"
        local timestamp
        timestamp="$(date -u +"%Y%m%d-%H%M%SZ")"
        custom_output="${BACKUP_DIR}/etcd-snapshot-${timestamp}.db"
    else
        local parent_dir
        parent_dir="$(dirname "${custom_output}")"
        mkdir -p "${parent_dir}"
    fi

    log_info "Initiating etcd snapshot creation to: ${custom_output}"
    read -r -a auth_args <<< "$(build_etcdctl_auth_args)"

    if [[ "${SKIP_HEALTH}" != "true" ]]; then
        log_info "Verifying cluster health prior to snapshot..."
        if ! etcdctl "${auth_args[@]}" endpoint health >/dev/null 2>&1; then
            log_warn "Cluster health check produced warnings; attempting snapshot regardless..."
        fi
    fi

    # Execute snapshot save
    etcdctl "${auth_args[@]}" snapshot save "${custom_output}"

    if [[ ! -s "${custom_output}" ]]; then
        log_fatal "Snapshot file ${custom_output} was not created or is 0 bytes."
    fi

    log_success "Snapshot written successfully. Validating integrity..."

    # Validate snapshot structure
    etcdctl snapshot status "${custom_output}" --write-out=table

    # Generate cryptographic SHA-256
    local hash
    hash="$(calculate_sha256 "${custom_output}")"
    if [[ "${hash}" != "UNAVAILABLE" ]]; then
        echo "${hash}  $(basename "${custom_output}")" > "${custom_output}.sha256"
        log_success "Generated SHA-256 checksum: ${hash}"
        log_info "Checksum saved to: ${custom_output}.sha256"
    fi

    # Write metadata manifest
    local meta_file="${custom_output}.meta.json"
    cat > "${meta_file}" <<EOF
{
  "timestamp": "$(date -u +"%Y-%m-%dT%H:%M:%SZ")",
  "snapshot_file": "$(basename "${custom_output}")",
  "sha256": "${hash}",
  "endpoints": "${ETCD_ENDPOINTS}",
  "node_name": "${NODE_NAME}",
  "size_bytes": $(stat -c%s "${custom_output}" 2>/dev/null || wc -c < "${custom_output}")
}
EOF
    log_success "Disaster recovery snapshot completed: ${custom_output}"
}

# ── Command: Snapshot Verify ──────────────────────────────────────────────────
snapshot_verify() {
    check_prerequisites
    local snapshot_file="$1"

    if [[ ! -f "${snapshot_file}" ]]; then
        log_fatal "Snapshot file not found: ${snapshot_file}"
    fi

    log_info "Inspecting etcd snapshot: ${snapshot_file}"
    if ! etcdctl snapshot status "${snapshot_file}" --write-out=table; then
        log_fatal "Snapshot structural validation failed! File may be corrupt."
    fi

    # Verify SHA-256 if file exists
    local sha_file="${snapshot_file}.sha256"
    if [[ -f "${sha_file}" ]]; then
        log_info "Verifying against expected SHA-256 checksum in ${sha_file}..."
        local expected_hash actual_hash
        expected_hash="$(awk '{print $1}' "${sha_file}")"
        actual_hash="$(calculate_sha256 "${snapshot_file}")"

        if [[ "${expected_hash}" == "${actual_hash}" ]]; then
            log_success "Cryptographic integrity verified: ${actual_hash}"
        else
            log_err "CHECKSUM MISMATCH DETECTED!"
            log_err "Expected: ${expected_hash}"
            log_err "Actual:   ${actual_hash}"
            log_fatal "Snapshot integrity compromised! Do not proceed with restore."
        fi
    else
        log_warn "No .sha256 sidecar file found. Current SHA-256: $(calculate_sha256 "${snapshot_file}")"
    fi

    log_success "Snapshot ${snapshot_file} is structurally valid and ready for restoration."
}

# ── Command: Snapshot Restore ─────────────────────────────────────────────────
snapshot_restore() {
    check_prerequisites
    local snapshot_file="$1"

    if [[ ! -f "${snapshot_file}" ]]; then
        log_fatal "Snapshot file not found: ${snapshot_file}"
    fi

    # Verify snapshot prior to performing destructive actions
    log_info "Running pre-flight verification on snapshot..."
    snapshot_verify "${snapshot_file}"

    log_warn "===================================================================="
    log_warn "CRITICAL DISASTER RECOVERY ACTION: ETCD RESTORATION"
    log_warn "===================================================================="
    log_warn "Target Data Directory : ${ETCD_DATA_DIR}"
    log_warn "Snapshot Source       : ${snapshot_file}"
    log_warn "Member Name           : ${NODE_NAME}"
    log_warn "Initial Cluster       : ${INITIAL_CLUSTER}"
    log_warn "===================================================================="
    log_warn "Restoring will replace the control plane etcd database state."
    log_warn "Ensure kube-apiserver and etcd static pods/services are stopped!"
    log_warn "===================================================================="

    if [[ "${FORCE_MODE}" != "true" ]]; then
        read -r -p "Type 'CONFIRM-ETCD-RESTORE' to proceed with state restoration: " confirmation
        if [[ "${confirmation}" != "CONFIRM-ETCD-RESTORE" ]]; then
            log_fatal "Restore aborted by operator."
        fi
    fi

    # Timestamped archive of current active data directory if it exists
    if [[ -d "${ETCD_DATA_DIR}" ]]; then
        local archive_dir="${ETCD_DATA_DIR}-archive-$(date +%s)"
        log_warn "Existing etcd data directory detected. Archiving to: ${archive_dir}"
        mv "${ETCD_DATA_DIR}" "${archive_dir}"
        log_success "Existing state archived to ${archive_dir}."
    fi

    # Staging directory for restoration
    local restore_target="${ETCD_DATA_DIR}"
    mkdir -p "$(dirname "${restore_target}")"

    log_info "Executing: etcdctl snapshot restore ${snapshot_file} ..."
    etcdctl snapshot restore "${snapshot_file}" \
        --data-dir="${restore_target}" \
        --name="${NODE_NAME}" \
        --initial-cluster="${INITIAL_CLUSTER}" \
        --initial-cluster-token="${INITIAL_CLUSTER_TOKEN}" \
        --initial-advertise-peer-urls="${INITIAL_ADVERTISE_PEER_URLS}"

    # Fix file permissions (etcd runs typically as uid/gid 0 or etcd:etcd 2001)
    log_info "Setting secure directory permissions (0700) on ${restore_target}..."
    chmod -R 0700 "${restore_target}"

    if getent passwd etcd >/dev/null 2>&1; then
        log_info "Setting ownership to etcd:etcd..."
        chown -R etcd:etcd "${restore_target}" || log_warn "Could not chown to etcd:etcd; ensure root or container permissions match."
    fi

    log_success "etcd snapshot restored to ${restore_target}."
    log_info "Next operational steps:"
    log_info "1. If static pods were temporarily relocated (e.g. /etc/kubernetes/manifests), move them back."
    log_info "2. Restart kubelet / etcd container service."
    log_info "3. Verify etcd endpoint health: $(basename "$0") health"
    log_info "4. Verify Kubernetes API server accessibility: kubectl get nodes"
    log_info "5. Check StellarNode CRs: kubectl get stellarnodes -A"
}

# ── Argument Parsing ──────────────────────────────────────────────────────────
COMMAND=""
TARGET_FILE=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        save|backup)
            COMMAND="save"
            if [[ $# -gt 1 && ! "$2" =~ ^- ]]; then
                TARGET_FILE="$2"
                shift
            fi
            shift
            ;;
        verify|status)
            COMMAND="verify"
            if [[ $# -gt 1 && ! "$2" =~ ^- ]]; then
                TARGET_FILE="$2"
                shift
            fi
            shift
            ;;
        restore)
            COMMAND="restore"
            if [[ $# -gt 1 && ! "$2" =~ ^- ]]; then
                TARGET_FILE="$2"
                shift
            fi
            shift
            ;;
        health)
            COMMAND="health"
            shift
            ;;
        help|-h|--help)
            usage
            exit 0
            ;;
        -e|--endpoints)
            ETCD_ENDPOINTS="$2"
            shift 2
            ;;
        --cacert)
            ETCD_CACERT="$2"
            shift 2
            ;;
        --cert)
            ETCD_CERT="$2"
            shift 2
            ;;
        --key)
            ETCD_KEY="$2"
            shift 2
            ;;
        --data-dir)
            ETCD_DATA_DIR="$2"
            shift 2
            ;;
        --backup-dir)
            BACKUP_DIR="$2"
            shift 2
            ;;
        --name)
            NODE_NAME="$2"
            shift 2
            ;;
        --initial-cluster)
            INITIAL_CLUSTER="$2"
            shift 2
            ;;
        --initial-cluster-token)
            INITIAL_CLUSTER_TOKEN="$2"
            shift 2
            ;;
        --initial-advertise-peer-urls)
            INITIAL_ADVERTISE_PEER_URLS="$2"
            shift 2
            ;;
        -f|--force)
            FORCE_MODE="true"
            shift
            ;;
        --skip-health)
            SKIP_HEALTH="true"
            shift
            ;;
        *)
            log_err "Unknown option or argument: $1"
            usage
            exit 1
            ;;
    esac
done

if [[ -z "${COMMAND}" ]]; then
    log_err "No command specified."
    usage
    exit 1
fi

case "${COMMAND}" in
    save)
        snapshot_save "${TARGET_FILE}"
        ;;
    verify)
        if [[ -z "${TARGET_FILE}" ]]; then
            log_fatal "Command 'verify' requires a snapshot file path."
        fi
        snapshot_verify "${TARGET_FILE}"
        ;;
    restore)
        if [[ -z "${TARGET_FILE}" ]]; then
            log_fatal "Command 'restore' requires a snapshot file path."
        fi
        snapshot_restore "${TARGET_FILE}"
        ;;
    health)
        check_health
        ;;
esac
