# Finalizer Storage Recovery

The StellarNode finalizer is only removed after node cleanup is complete and storage verification is positive. The normal reconcile and periodic recovery scan share this gate, so a controller restart does not change the decision.

Before deleting a PVC with `Delete` retention, the controller records the backing cloud volume identity in the StellarNode annotation `stellar.org/finalizer-volume-identity`. If the PVC was never bound, it records an explicit `NeverBound` marker. This write happens before the PVC delete request, so recovery can verify storage after the PVC or PV object has disappeared. If a legacy deletion has already lost both Kubernetes objects and has no recorded identity, recovery deliberately leaves the finalizer in place for operator investigation.

Verification has two independent parts:

1. Kubernetes `VolumeAttachment` objects must not report the PV as attached. A missing status or API error is not interpreted as detached.
2. The cloud API is queried for the exact volume identity. AWS uses EC2 `DescribeVolumes`; `Delete` retention requires the volume to be not found, while `Retain` accepts an existing volume only when it has no attachments. GCP uses Compute Engine `disks.get`; the `users` field must be empty for retained disks, and deleted disks must return `404`.

Unknown CSI drivers, malformed identities, missing credentials, authorization failures, cloud API errors, and ambiguous Kubernetes state all fail closed. The controller never removes CSI/PVC/PV finalizers; it removes only its own StellarNode finalizer after the checks above. GCP deployments must provide `GOOGLE_OAUTH_ACCESS_TOKEN` or configure workload identity so the pod can obtain a Compute API access token from the GCE metadata server. AWS deployments must grant `ec2:DescribeVolumes` for the relevant regions.

The recovery worker scans deleting StellarNodes every 30 seconds and runs only on the elected leader. Verification errors retain the finalizer and are retried on the next scan.