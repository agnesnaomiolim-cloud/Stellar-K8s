# Production Profiling Runbook

Capture CPU and heap profiles from the Stellar-K8s operator REST API in production, then analyze them with standard pprof tooling.

This runbook matches the implementation in issue #1330. Do not enable profiling permanently; use it for short diagnostic windows.

## Enabling profiling

Profiling is **off by default** and uses two independent gates:

1. **Build-time:** compile the operator with the optional Cargo feature `profiling` (not in `default`):

   ```bash
   cargo build --release --features profiling
   ```

   Container images used for production profiling must include this feature. Enabling Helm alone does not add the endpoints if the image was built without `profiling`.

2. **Runtime:** set `REST_API_PROFILING_ENABLED=true` so routes are registered.

### Helm

```yaml
operator:
  restApiEnabled: true
  profiling:
    enabled: true
```

When `operator.profiling.enabled` is true (and the REST API is enabled), the chart sets:

| Env / mount | Value |
|-------------|--------|
| `REST_API_PROFILING_ENABLED` | `true` |
| `MALLOC_CONF` | `prof:true,prof_active:true,lg_prof_sample:19` |
| Volume `profiling-tmp` | ephemeral `emptyDir` mounted at `/tmp` |

`MALLOC_CONF` turns on jemalloc allocation sampling for heap profiles. CPU profiles use the `pprof` crate and do not require jemalloc, but the same image feature enables both.

**Writable temporary directory:** Heap dumps write a short-lived jemalloc profile into the process temp directory (`/tmp`) before converting it to pprof. The chart keeps `readOnlyRootFilesystem: true` and only mounts an ephemeral `emptyDir` at `/tmp` when profiling is enabled. That volume is not a hostPath and is discarded when the pod stops. Profiling-disabled deployments do not get this mount.

### Security requirements

- Endpoints are on the **protected** REST router (`api_reader` Bearer/OIDC/K8s token).
- Each profiling route also requires **Admin** (`api_admin`), same as `POST /config/log-level`.
- Paths are versioned: `/api/v1/debug/pprof/...` (see [API versioning](../api/versioning.md)).
- Unauthenticated requests receive `401` (missing token) or `403` (insufficient role).
- Classic unauthenticated `/debug/pprof/*` paths are **not** registered (security scanners flag those).

Disable when finished:

```yaml
operator:
  profiling:
    enabled: false
```

Redeploy so the env vars and `/tmp` emptyDir mount are removed. Prefer a rebuild without `--features profiling` for long-lived images that should not expose the capability at all.

## Capturing a CPU profile

Default duration is **30 seconds** (allowed range **1-60**). Concurrent CPU captures return `429`.

Protobuf (for `go tool pprof` / `pprof`):

```bash
TOKEN="$(kubectl create token stellar-operator -n stellar-system)"

curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  "https://stellar-operator.stellar-system.svc:8080/api/v1/debug/pprof/profile?seconds=30" \
  -o cpu-profile.pb
```

| Parameter | Allowed | Notes |
|-----------|---------|--------|
| `seconds` | 1-60 | Integer; omit for 30 |
| `format` | `proto` | Default; other values return `400` |

Response: `application/octet-stream`, filename `cpu-profile.pb`.

Flamegraphs are **not** rendered by the operator (avoids a CDDL-licensed transitive crate blocked by `cargo-deny`). Generate them locally from the protobuf (see Analyzing profiles).

Choose duration based on load: 10-30s is usually enough to see hotspots; avoid 60s unless needed (CPU and latency impact).

Adjust host/port to match your Service (`operator.restApiPort`, Ingress, or port-forward).

## Capturing a memory/heap profile

Heap dumps use **jemalloc** + `jemalloc_pprof` and return a pprof protobuf of sampled allocations.

```bash
curl -fsS \
  -H "Authorization: Bearer ${TOKEN}" \
  "https://stellar-operator.stellar-system.svc:8080/api/v1/debug/pprof/heap" \
  -o heap-profile.pb
```

### Platform notes

- Requires the `profiling` feature (jemalloc global allocator) and `prof:true` in jemalloc config.
- Supported on typical Linux operator images. If jemalloc profiling control is missing, the API returns **503** with `heap_unavailable` / `heap_inactive`.
- Sampling (`lg_prof_sample:19`) is statistical; short-lived processes may show sparse stacks until enough allocations occur.

## Analyzing profiles

### CPU protobuf

```bash
# Interactive CLI (install https://github.com/google/pprof)
pprof -http=:8081 ./target/release/stellar-operator cpu-profile.pb

# Or with go tool pprof if you have a Go toolchain
go tool pprof -http=:8081 cpu-profile.pb
```

Open the flame graph view. Look for:

- Wide frames (self time) under reconciliation, HTTP handlers, or serde
- Unexpected blocking in I/O or locks
- Allocation-heavy paths that also show up in heap profiles

### Heap protobuf

Heap responses are **gzip-compressed** pprof protobuf (jemalloc_pprof encoding). `go tool pprof` and Google `pprof` accept this format directly:

```bash
pprof -http=:8081 ./target/release/stellar-operator heap-profile.pb
```

Use **inuse_space** / **alloc_space** (tool-dependent) to find allocation hotspots. Compare with a second dump after load to see growth.

### Comparing profiles

```bash
pprof -base=cpu-before.pb cpu-after.pb
```

Compare only profiles taken under similar load and similar duration. Short samples (1-5s) are noisy; prefer >=10s for CPU.

### Avoiding misleading conclusions

- A quiet operator yields flat or empty-looking profiles - reproduce the issue while capturing.
- Release builds strip symbols (`strip = true` in release profile); keep a matching unstripped binary or debuginfo for readable stacks when possible.
- Heap samples are not a full heap dump; they reflect jemalloc's sampling rate.
- Do not treat a single 1-second profile as proof of a production bottleneck.

## Production safety

| Topic | Guidance |
|-------|----------|
| Auth | Bearer/OIDC + **Admin** required; never expose the REST API without auth |
| Overhead | CPU profiling samples at 100Hz for the requested duration; heap sampling adds allocator cost while `prof_active` is true |
| Duration | Prefer 10-30s; max 60s enforced by the API |
| Concurrency | One CPU profile at a time (`429` if busy) |
| Sensitive data | Profiles expose function names and stacks, not request bodies; still treat as operationally sensitive |
| Cleanup | Set `operator.profiling.enabled: false` after the investigation |

## Troubleshooting

| Symptom | Likely cause | Action |
|---------|--------------|--------|
| `404` on `/api/v1/debug/pprof/*` | Runtime flag off or feature not compiled | Enable Helm `profiling.enabled` **and** use a `profiling`-featured image |
| `401` | Missing/invalid Bearer token | Use a ServiceAccount token or OIDC JWT accepted by the operator |
| `403` | Authenticated but not Admin | Grant Admin/RBAC admin verb (same as log-level POST) |
| `400` `invalid_parameter` | `seconds` out of range or unsupported `format` | Use 1-60; omit format or set `proto` |
| `429` `profiler_busy` | Overlapping CPU capture | Wait for the in-flight profile to finish |
| `503` heap errors | jemalloc profiling inactive/unsupported, or temp write failed | Confirm `MALLOC_CONF`, Linux image with `--features profiling`, and that Helm mounted `/tmp` emptyDir when profiling is enabled |
| Empty / unreadable stacks | Stripped binary or short sample | Longer capture; analyze with matching symbols |
| Warning that feature is missing | Env set on a non-profiling build | Rebuild with `--features profiling` |

## Related documentation

- [API versioning](../api/versioning.md)
- [Operator REST API](../api/index.md)
- [OpenAPI](../api/openapi.yaml)
- [Operations index](index.md)

---

## Issue #305 — Memory Leak Detector & Heaptrack Profiling

This section covers the enhancements introduced by issue #305: the standalone
debug server, jemalloc heap profiling, the in-process memory-leak detector, and
the automated Prometheus alerts.

### Architecture overview

```
┌───────────────────────────────────────────────────────────────┐
│  stellar-operator (built with --features profiling)           │
│                                                               │
│  ┌─────────────────────────────┐  ┌────────────────────────┐ │
│  │  jemalloc global allocator  │  │  MemoryLeakDetector     │ │
│  │  (tikv-jemallocator)        │  │  polls RSS every 5 min  │ │
│  │  MALLOC_CONF:               │  │  fires warn! if >20%    │ │
│  │    prof:true                │  │  growth over 24 h       │ │
│  │    prof_active:false        │  └───────────┬────────────┘ │
│  │    lg_prof_sample:19        │              │               │
│  └──────────────┬──────────────┘              │ tracing::warn │
│                 │ dump_pprof()                 ▼               │
│  ┌──────────────▼──────────────┐  ┌────────────────────────┐ │
│  │  /debug/pprof/heap          │  │  Prometheus             │ │
│  │  (127.0.0.1:6060)           │  │  stellar_profiling_     │ │
│  │  X-Profiling-Token required │  │  rss_bytes gauge        │ │
│  └─────────────────────────────┘  └────────────────────────┘ │
└───────────────────────────────────────────────────────────────┘
```

### Enabling the debug server (issue #305)

The debug server runs on `127.0.0.1:6060` **in addition to** the REST API
server on port 9090.  It is completely independent and never requires mTLS.

```bash
# Build with profiling support
cargo build --release --features profiling

# Set the token SHA-256 (store the raw token in a K8s Secret)
TOKEN=$(openssl rand -hex 32)
TOKEN_SHA256=$(echo -n "$TOKEN" | sha256sum | awk '{print $1}')

kubectl create secret generic stellar-profiling-token \
  --from-literal=token="$TOKEN" \
  -n stellar-system

# Pass the hash to the operator via env var
export STELLAR_PROFILING_TOKEN_SHA256="$TOKEN_SHA256"
```

### Pulling a heap flamegraph (issue #305 workflow)

```bash
# 1. Port-forward the debug server
kubectl port-forward pod/<operator-pod> 6060:6060 -n stellar-system

# 2. Retrieve the token
TOKEN=$(kubectl get secret stellar-profiling-token \
        -n stellar-system \
        -o jsonpath='{.data.token}' | base64 -d)

# 3. Pull a binary pprof heap dump
curl -sSf \
  -H "X-Profiling-Token: $TOKEN" \
  "http://localhost:6060/debug/pprof/heap?format=proto" \
  -o heap.pb.gz

# 4. Render an interactive flamegraph
go tool pprof -http=:8080 heap.pb.gz
# Open http://localhost:8080/ui/flamegraph in your browser

# 5. Or get a quick text summary
go tool pprof -top heap.pb.gz
```

### In-process memory-leak detector

The `MemoryLeakDetector` (in `telemetry/src/profiling/allocator.rs`) runs as a
background Tokio task.  It samples RSS every 5 minutes and emits a
`tracing::warn!` with `event=stellar_operator_memory_growth_alert` when RSS
grows more than **20 %** over a **24-hour** rolling window.

This alert is also captured by the Prometheus rule
`StellarOperatorMemoryLeakDetected` in
`monitoring/memory-leak-alerts.yaml`.

### Prometheus alerts (issue #305)

| Alert | Condition | Severity |
|-------|-----------|----------|
| `StellarOperatorMemoryLeakDetected` | RSS > 20 % growth over 24 h (jemalloc) | warning |
| `StellarOperatorMemoryLeakCritical` | RSS > 50 % growth over 24 h | critical |
| `StellarOperatorRSSExceedsLimit` | RSS > 150 MiB absolute | warning |
| `StellarOperatorMemoryLeakDetectedProc` | RSS > 20 % growth over 24 h (/proc) | warning |

Deploy the alert rules:

```bash
kubectl apply -f monitoring/memory-leak-alerts.yaml -n stellar-system
```

### Zero-overhead guarantee

When the operator is **not** built with `--features profiling`:
- The system allocator is used unchanged.
- The debug server on port 6060 is still started but returns `501 Not Implemented` for `?format=proto`.
- The `MemoryLeakDetector` falls back to `/proc/self/status` for RSS polling.
- All jemalloc code paths are compiled out by `#[cfg(feature = "profiling")]`.
