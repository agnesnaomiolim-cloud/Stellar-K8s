# Bare-Metal Stellar-K8s Examples

Reference manifests for running Stellar-K8s on physical hardware with **no
cloud dependencies**. These files back the guide at
[`docs/infrastructure/bare-metal.md`](../../docs/infrastructure/bare-metal.md).

Every manifest here is deliberately cloud-free:

- No cloud CSI provisioner (`ebs.csi.aws.com`, `pd.csi.storage.gke.io`,
  `disk.csi.azure.com`, and so on).
- No cloud load balancer annotations. External access is MetalLB.
- No cloud DNS, managed database, or object storage requirement.

## Contents

| File | Purpose |
|---|---|
| [`storage-class.yaml`](storage-class.yaml) | Three StorageClasses: static local NVMe (validators), Local Path Provisioner (archives), OpenEBS Local PV (larger fleets) |
| [`validator-baremetal.yaml`](validator-baremetal.yaml) | A `StellarNode` validator pinned to a physical NVMe through a static local PV |
| [`network-attachment.yaml`](network-attachment.yaml) | SCP isolation NetworkPolicy, MetalLB address pools, and the host bond/VLAN reference |
| [`scripts/validate-vm-cluster.sh`](scripts/validate-vm-cluster.sh) | VM validation harness that proves local-disk volume provisioning end to end |

## Quick start

```bash
# 1. Validate the local-storage path on this machine (needs docker, kind, kubectl).
bash examples/bare-metal/scripts/validate-vm-cluster.sh

# 2. On a real cluster, apply the storage classes.
kubectl apply -f examples/bare-metal/storage-class.yaml

# 3. Label hosts that have passed the fio benchmark.
kubectl label node stellar-cp-1 stellar.org/storage=nvme-tuned

# 4. Deploy a validator pinned to that host.
kubectl apply -f examples/bare-metal/validator-baremetal.yaml
```

## Validation harness

`scripts/validate-vm-cluster.sh` creates a disposable kind cluster and proves
the storage contract the guide depends on:

1. Installs a local, cloud-free provisioner and StorageClass.
2. Provisions a PVC and asserts the bound PersistentVolume is node-local
   (`spec.local` / `spec.hostPath`) and carries node affinity — a network disk
   fails the check.
3. Writes data from one pod, deletes it, and reads the data back from a second
   pod — proving durability across a pod restart.
4. Asserts the pod and the volume are co-located on the same node.
5. Asserts `reclaimPolicy: Retain`, so a `kubectl delete` cannot take the
   ledger database with it.

It exits non-zero on the first failed assertion, so it can gate CI. The cluster
is deleted in a `trap` on exit; pass `--keep` to retain it for inspection.

```bash
bash examples/bare-metal/scripts/validate-vm-cluster.sh --keep
kind delete cluster --name stellar-baremetal-vm   # when you are done
```

### What the harness does not cover

Full Talos Linux or kubeadm bootstrap on VMs needs privileged VMs, a second
boot target, and (for the network half) switch-side LACP and VLAN
configuration. Those stay documented manual steps in
[the guide](../../docs/infrastructure/bare-metal.md), which is why the harness
focuses on the part that breaks silently: attaching a physical disk to a
`StellarNode` StatefulSet.

## Prerequisites

- `docker`, `kind`, `kubectl` for the harness.
- For a real cluster: the storage tuning in
  [`docs/performance/bare-metal-iops.md`](../../docs/performance/bare-metal-iops.md)
  completed **before** the cluster is built. This guide assumes the data drive
  is already formatted, mounted, and benchmarked.

## Related documentation

- [Bare-Metal Kubernetes Bootstrap Guide](../../docs/infrastructure/bare-metal.md)
- [Bare-Metal NVMe IOPS Tuning](../../docs/performance/bare-metal-iops.md)
- [MetalLB / BGP Anycast](../../docs/metallb-bgp-anycast.md)
- [Networking Overview](../../docs/networking/index.md)
- [Volume Snapshots](../../docs/volume-snapshots.md)
- [Capacity Planning](../../docs/operations/capacity-planning.md)
