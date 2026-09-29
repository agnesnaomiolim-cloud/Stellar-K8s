# Networking: Ports, Egress, and Firewall Requirements

Stellar validators are only useful if they can reach their peers. This page documents every port a
`StellarNode` needs, with a focus on **outbound (egress) TCP access to peer ports** — the single most
common reason a freshly deployed validator sits stuck in `Joining SCP` with **zero peers**.

> **TL;DR:** If your validator cannot open outbound TCP connections on ports **11625** (standard peer
> port) and **3510** (used by SDF testnet cores), it will never connect to the SDF testnet. Open
> egress to both ports before debugging anything else. See the
> [symptom checklist](#symptoms-of-missing-egress) below.

---

## Port Requirement Table

| Port  | Protocol | Direction            | Purpose                                              | Required?                      |
|-------|----------|----------------------|------------------------------------------------------|--------------------------------|
| 3510  | TCP      | **Outbound (egress)** | Peer connections to SDF testnet cores                | Yes — testnet validators       |
| 11625 | TCP      | **Outbound (egress)** | Stellar Core P2P (peer-to-peer SCP traffic)          | Yes — all validators           |
| 11625 | TCP      | Inbound              | Accept P2P connections from other validators         | Yes — validators serving peers |
| 11626 | TCP      | Internal             | Stellar Core HTTP admin endpoint / Horizon ingest    | Yes                            |
| 8000  | TCP      | Inbound              | Horizon REST API                                     | Horizon only                   |
| 9090  | TCP      | Internal             | Prometheus metrics scrape (if enabled)               | Optional                       |
| 53    | UDP/TCP  | Outbound             | DNS resolution (kube-dns/CoreDNS)                    | Yes                            |
| 443   | TCP      | Outbound             | History archive access (HTTPS)                       | Yes                            |

**Outbound access to peer ports 3510 and 11625 is mandatory.** Peer ports are how your validator
discovers, handshakes with, and stays connected to the rest of the quorum set. A node with correct
configuration but blocked egress will start, pass its readiness checks against its local admin
endpoint, and then sit idle forever.

---

## Required Outbound (Egress) Rules

Validators need outbound TCP access to peer ports on the network they join. The destinations differ
 between Testnet and Mainnet — see [Mainnet vs Testnet](#mainnet-vs-testnet-destinations) below.

| Destination                                    | Port | Protocol | Purpose                          |
|------------------------------------------------|------|----------|----------------------------------|
| SDF testnet cores (`core-testnet*.stellar.org`)| 3510 | TCP      | SCP consensus with SDF testnet   |
| Other validators (cluster-internal)            | 11625| TCP      | SCP consensus (in-cluster peers) |
| SDF testnet cores                              | 11625| TCP      | SCP consensus (testnet peer port)|
| Other validators (external)                    | 11625| TCP      | SCP consensus with external peers|
| History archive servers                        | 443  | TCP      | Ledger history catch-up          |
| Kubernetes DNS                                 | 53   | UDP/TCP  | Service and peer name resolution |

> **Note:** SDF occasionally rotates testnet core hostnames and may serve peers on either port.
> Allow egress to **both 3510 and 11625** for the `core-testnet*.stellar.org` hosts so a published
> port change cannot silently disconnect your validator. Always confirm the current host list in the
> SDF testnet quorum set configuration before locking down your firewall.

### Example NetworkPolicy egress rules

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: allow-validator-egress
  namespace: stellar-testnet
spec:
  podSelector:
    matchLabels:
      app.kubernetes.io/component: stellar-validator
  policyTypes:
    - Egress
  egress:
    # Peer traffic to SDF testnet cores (both peer ports).
    # Scope the ipBlock to SDF address ranges in production if you can.
    - to:
        - ipBlock:
            cidr: 0.0.0.0/0
      ports:
        - port: 3510
          protocol: TCP
        - port: 11625
          protocol: TCP
    # DNS
    - to:
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: kube-system
      ports:
        - port: 53
          protocol: UDP
        - port: 53
          protocol: TCP
    # History archives
    - to:
        - ipBlock:
            cidr: 0.0.0.0/0
      ports:
        - port: 443
          protocol: TCP
```

> The reconciler-generated per-node policies (`spec.networkPolicy.enabled: true`) already include
> same-network peer traffic; if your nodes only peer inside the cluster you normally do not need the
> policy above. The moment you join a **public** network (SDF testnet or mainnet), the external
> peer-destinations must be allowed explicitly — see
> [Network Isolation](network-isolation.md) for how the namespace-level policies interact.

---

## Mainnet vs Testnet Destinations

The peer ports you must open **depend on the network the node joins**:

| Network  | Namespace label          | Peer destinations                                  | Peer ports     |
|----------|--------------------------|----------------------------------------------------|----------------|
| Testnet  | `stellar.org/network=testnet` | SDF testnet cores (`core-testnet*.stellar.org`)| **3510**, 11625 |
| Mainnet  | `stellar.org/network=mainnet` | Mainnet validators (`core-live*.stellar.org`, external validators) | **11625** |
| Futurenet| `stellar.org/network=futurenet`| SDF futurenet cores                             | 11625          |

- **Testnet validators must allow both 3510 and 11625.** SDF testnet cores are reachable on either
  peer port; blocking either one can leave you with a partial peer set that fails to reach quorum.
- **Mainnet validators only need 11625.** Opening 3510 on a mainnet node is unnecessary — keep the
  egress list minimal.
- The per-network destinations are enforced by the `stellar.org/network` namespace label (see
  [Network Isolation](network-isolation.md)). A node in a `testnet`-labelled namespace cannot peer
  with mainnet pods, and vice versa — the isolation policies intentionally block cross-network
  traffic, including on peer ports.

---

## Symptoms of Missing Egress

If outbound access to the peer ports is blocked, the node looks "healthy" but never joins
consensus. Map what you see in `kubectl` to the likely network cause:

| Symptom (kubectl-visible)                                    | Likely cause                                                    |
|--------------------------------------------------------------|-----------------------------------------------------------------|
| Pod `Running`, readiness passing, but `Joining SCP` for >15 min | **Missing egress to peer ports 3510/11625**                    |
| `status.syncState: Unknown` stuck indefinitely               | Node cannot reach peers to learn the network state              |
| Zero peers in `curl http://<pod>:11626/peers`                | Outbound peer connections blocked (firewall/NetworkPolicy/NAT)  |
| `DROP` counters increasing in CNI / security group logs      | Egress explicitly denied                                        |
| Repeated `Failed to connect to peer` in core logs            | Destination port filtered or security group missing             |
| Peers connect briefly then drop mid-handshake                | Asymmetric firewall — outbound allowed, return path blocked     |

### Quick diagnosis sequence

```bash
# 1. Check the node's observed sync state ("Joining SCP"/"Booting" surface as Unknown)
kubectl get stellarnode <name> -n <namespace> -o jsonpath='{.status.syncState}'

# 2. Count live peers from inside the validator
kubectl exec -n <namespace> <pod> -- curl -s http://localhost:11626/peers | jq '.authenticated_peers'

# 3. Test outbound reachability to the SDF testnet peer ports
kubectl run -it --rm netdebug --image=nicolaka/netshoot --restart=Never -- \
  nc -zv core-testnet1.stellar.org 3510
kubectl run -it --rm netdebug --image=nicolaka/netshoot --restart=Never -- \
  nc -zv core-testnet1.stellar.org 11625

# 4. If either connection times out, fix egress first — nothing else matters yet
```

If step 2 returns an empty peer list and step 3 times out, you have confirmed the missing-egress
failure mode. Work through the [firewall and NAT guidance](#firewall-nat-and-security-group-guidance)
below, then re-run step 3 before restarting the node.

A validator that is stuck with zero peers will log lines similar to:

```text
INFO  Ledgers: (0) Joining SCP (last): [last_check_ledger=2]
WARN  Flood: too few peers connected (0/7)
```

The node may also enter and leave `CatchingUp` spuriously once a single peer connection succeeds,
because it keeps losing its only peer.

---

## Firewall, NAT, and Security Group Guidance

### Cloud security groups

Security groups sit **outside** Kubernetes — NetworkPolicies cannot open them.

- **AWS:** attach an outbound rule allowing TCP 3510 and 11625 to the worker-node security group
  (or to the SDF testnet address ranges if you scope destinations).
- **GCP:** add a VPC firewall *egress* rule (`--direction=EGRESS`) for TCP 3510,11625.
- **Azure:** add an outbound NSG rule for the node subnet allowing TCP 3510 and 11625.

```bash
# AWS CLI example — allow egress to the testnet peer ports
aws ec2 authorize-security-group-egress \
  --group-id sg-0123456789abcdef0 \
  --protocol tcp --port 3510 \
  --cidr 0.0.0.0/0
aws ec2 authorize-security-group-egress \
  --group-id sg-0123456789abcdef0 \
  --protocol tcp --port 11625 \
  --cidr 0.0.0.0/0
```

### NAT gateways and egress IP stability

- Stellar peers do not require a stable source IP, but **SNAT must be enabled** for the node subnet.
  Instances routed through a NAT gateway work fine; instances with no NAT path (private subnets with
  no egress route) will show the zero-peer symptom above.
- If you restrict the NAT gateway to an allow-list of destination ports, include **3510 and 11625
  TCP** alongside 443 (history) and 53 (DNS).
- Hairpin NAT is **not** required for public-network peering; only clusters peering with their own
  LoadBalancer IPs need it.

### In-cluster NetworkPolicies

- Default-deny egress policies silently block peer traffic. Every egress policy applied to validator
  pods must explicitly allow **TCP 3510 and 11625 outbound** — see the example policy above and the
  [troubleshooting guide](troubleshooting/networking.md#6-stellar-p2p-firewalling).
- Remember that namespace-level isolation policies (Helm `networkIsolation.*` values) apply to *all*
  pods in the namespace; if they lack external peer destinations, adding a per-node policy is not
  enough. See [Network Isolation](network-isolation.md).
- Calico `GlobalNetworkPolicy` and Cilium `CiliumNetworkPolicy` objects are easy to miss when
  auditing — check cluster-wide policies too.

### Verification checklist

- [ ] Outbound TCP 3510 to SDF testnet cores succeeds from a pod (`nc -zv core-testnet1.stellar.org 3510`)
- [ ] Outbound TCP 11625 to SDF testnet cores succeeds from a pod
- [ ] Security group / NSG / VPC firewall egress rules cover both ports
- [ ] No NetworkPolicy (namespace or cluster-scoped) drops 3510/11625 egress
- [ ] NAT path exists from the node subnet to the internet
- [ ] Peer count is non-zero after restarting Stellar Core

---

## Required Inbound Rules (for reference)

| Source                        | Port  | Purpose                                  |
|-------------------------------|-------|------------------------------------------|
| Other validators              | 11625 | Inbound P2P connections                  |
| Horizon pods (same namespace) | 11626 | Ledger ingest from Stellar Core          |
| Operator pod                  | 11626 | Health checks and config reload          |
| Ingress / LoadBalancer        | 8000  | Horizon REST API (Horizon nodes only)    |

---

## Related Documentation

- [Networking Troubleshooting Guide](troubleshooting/networking.md) — symptom-first diagnosis,
  including the zero-peer / `Joining SCP` failure mode
- [Network Isolation](network-isolation.md) — mainnet/testnet separation and namespace labels
- [Peer Discovery](peer-discovery.md) — how the operator maintains the peer list
- [Service Mesh](service-mesh.md) — routing P2P traffic through Istio/Linkerd
- [Diagnostic script](../scripts/debug-network.sh) — automated connectivity checks
