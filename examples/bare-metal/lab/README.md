# Bare-Metal Validation Lab

A reproducible libvirt/QEMU lab that executes the [Bare-Metal Kubernetes Bootstrap
Guide](../../../docs/infrastructure/bare-metal.md) and verifies that a `StellarNode`
is provisioned from **local disk volumes**.

This is the executable form of §8 of the guide, and the evidence required by the
validation item in issue #249.

---

## What the lab proves

| Check | Automated by |
|---|---|
| Talos machine config applies (bond, VLAN, disk mounts, kubelet `extraMounts`) | `create-lab.sh` |
| A node-local volume binds through `WaitForFirstConsumer` | `verify.sh` |
| The volume lands on the NVMe-labelled node | `verify.sh` |
| **Data written to the volume survives pod re-creation** | `verify.sh` |
| The example `StellarNode` gets its own bound local PVC and a placed pod | `verify.sh` |
| SCP VLAN (`802.1Q`) is reachable between nodes over the bond | `verify.sh` |

## What it does not prove

The lab is a faithful rehearsal of the **storage** path and of the Talos network
*configuration*, but a nested libvirt bridge is not a switch:

- **LACP (`802.3ad`) negotiation is not tested.** There is no LACP partner, so the
  lab uses `mode: active-backup`. Production uses `802.3ad` (§6.2).
- **Jumbo frames are not tested.** The libvirt bridge stays at MTU 1500, so the lab
  configures bond MTU 1500. Production uses 9000 on the storage/SCP VLANs (§6.1).
- **MetalLB L2 VIPs are not tested.** A VIP needs a real L2 segment, so §6.4 is
  validated on hardware. `bootstrap-cluster.sh` deliberately skips MetalLB.

Everything else in §5 and §6 maps onto this lab.

---

## Requirements

A Linux host with:

- KVM: `/dev/kvm` must exist
- `libvirt` + `virsh`
- `virt-install` (`virtinst`)
- `talosctl` (must match `TALOS_VERSION`)
- `kubectl`, `helm`, `curl`

You must be able to talk to `qemu:///system` — add your user to the `libvirt` group,
or run the scripts with `sudo` (the scripts pass `--connect` explicitly).

Roughly 20 GiB of disk and 4 vCPU / 16 GiB RAM of headroom for the default topology
(3 control planes + 2 workers).

---

## Usage

```bash
cd examples/bare-metal/lab

# 1. Build the network, VMs, and Talos cluster (~10-15 min first run: ISO + images)
./create-lab.sh

# 2. Install Cilium, Local Path Provisioner, StorageClasses, the operator, and the
#    example StellarNode
export KUBECONFIG="${HOME}/.cache/stellar-lab/kubeconfig"
./bootstrap-cluster.sh

# 3. Run the acceptance checklist
./verify.sh

# 4. Tear down
./destroy-lab.sh          # add --force to skip the confirmation prompt
```

`create-lab.sh` prints the exact `KUBECONFIG` and state directory to use.

### Expected result

```
Result: 12 passed, 0 failed
─────────────────────────────────────────────
Bare-metal validation PASSED
```

### Capturing evidence for the issue

```bash
./create-lab.sh > lab-create.log 2>&1
./bootstrap-cluster.sh >> lab-create.log 2>&1
./verify.sh | tee lab-verify.log
kubectl -n stellar get pvc,pods -o wide | tee -a lab-verify.log
```

Attach `lab-verify.log` to the PR/issue when closing #249.

---

## Configuration

Every value in [`cluster.env`](cluster.env) can be overridden by exporting it first:

```bash
WORKERS=3 VM_MEMORY_MB=6144 ./create-lab.sh
```

| Variable | Default | Purpose |
|---|---|---|
| `CONTROLPLANES` / `WORKERS` | `3` / `2` | Topology |
| `VM_CPUS` / `VM_MEMORY_MB` | `2` / `4096` | Per-node resources |
| `VM_DATA_DISK_GB` | `40` | Size of each local data disk |
| `TALOS_VERSION` | `v1.8.2` | Pinned Talos release |
| `LOCAL_PATH_VERSION` | `v0.0.30` | Pinned provisioner release |
| `LAB_STATE_DIR` | `~/.cache/stellar-lab` | All generated state (outside the repo) |
| `LAB_SKIP_NETWORK_CHECKS` | `0` | Set to `1` if the host bridge blocks 802.1Q |

---

## Layout

| File | Purpose |
|---|---|
| `cluster.env` | Shared, non-secret configuration |
| `create-lab.sh` | Network + VMs + Talos config/bootstrap |
| `bootstrap-cluster.sh` | CNI, local storage, StorageClasses, operator, example node |
| `verify.sh` | Acceptance checklist (§8.3/§8.4) |
| `destroy-lab.sh` | Teardown |
| `local-volume-probe.yaml` | Deterministic local-disk probe used by `verify.sh` |

Generated state (Talos config, per-node patches, disk images, `nodes.tsv`) lives under
`LAB_STATE_DIR`, never in the repository.

---

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `cannot talk to libvirt` | `libvirtd` not running, or your user is not in the `libvirt` group |
| VM never reaches maintenance mode | `LAB_STATE_DIR/${LAB_NAME}-*.log` has the serial console; check `/dev/kvm` is available and VT-x/AMD-V is enabled |
| `bootstrap-cluster.sh` cannot reach the API | Re-run `create-lab.sh`; the kubeconfig is written to `LAB_STATE_DIR/kubeconfig` |
| `local-volume-probe` PVC stays `Pending` | The local-path `nodePathMap` did not match a node name — check `LAB_STATE_DIR/nodes.tsv` against `kubectl get nodes` |
| VLAN check fails | The host bridge may filter 802.1Q; re-run with `LAB_SKIP_NETWORK_CHECKS=1` and validate §6.3 on hardware |
| `verify.sh`: "no StellarNode pod is scheduled yet" | The operator needs a minute; re-run `verify.sh` |
