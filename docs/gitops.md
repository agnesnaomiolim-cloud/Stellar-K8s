# GitOps Deployment Engine

The GitOps deployment engine (Issue #286) turns a Git repository into the
single source of truth for Stellar-K8s node configuration. It continuously
polls a GitHub repository for state changes, auto-applies Captive Core and
Horizon ConfigMaps keyed by commit SHA, and automatically rolls back commits
that break node sync health.

## Why

Manual `kubectl apply` leads to configuration drift across global
multi-region clusters. With the GitOps engine enabled, infrastructure
deployments become entirely declarative, version-controlled, and safeguarded
by automated health rollbacks.

## Repository layout

The engine expects one directory per `StellarNode`, named after the node's
`app.kubernetes.io/instance` value (i.e. the CR name):

```text
clusters/prod/
├── my-validator/
│   └── stellar-core.cfg     # → ConfigMap my-validator-config (key: stellar-core.cfg)
├── horizon-api/
│   └── horizon.env          # → ConfigMap horizon-api-config (key: horizon.env)
└── soroban-rpc/
    └── captive-core.cfg     # → ConfigMap soroban-rpc-config (key: stellar-core.cfg)
```

Each ConfigMap patch is annotated with `stellar.org/gitops-commit`, so the
live cluster state is always attributable to a specific commit.

## Enabling the engine

### Helm

```yaml
# values.yaml
featureFlags:
  enableGitops: true

gitops:
  enabled: true
  repo: "my-org/stellar-config"
  branch: main
  path: clusters/prod
  pollIntervalSecs: 30
  # Optional: raise the GitHub API rate limit (60 → 5000 req/h).
  tokenExistingSecret: stellar-gitops-token
  tokenSecretKey: token
```

### CLI / environment

```bash
stellar-operator run \
  --enable-gitops \
  --gitops-repo my-org/stellar-config \
  --gitops-branch main \
  --gitops-path clusters/prod \
  --gitops-poll-interval-secs 30 \
  --gitops-token "$GITHUB_TOKEN"
# Env equivalents: ENABLE_GITOPS, GITOPS_REPO, GITOPS_BRANCH, GITOPS_PATH,
#                  GITOPS_POLL_INTERVAL_SECS, GITOPS_TOKEN
```

## Sync flow

1. **Poll** — the leader polls the tracking branch every
   `pollIntervalSecs`. Conditional requests (`ETag`/`If-None-Match`) mean
   unchanged branches cost nothing against the GitHub rate limit.
2. **Render** — on a new commit SHA, every node directory is rendered
   *before* any cluster mutation. A commit that fails to render (broken
   file, GitHub outage mid-read) never half-applies.
3. **Apply** — rendered ConfigMaps are server-side-applied (field manager
   `stellar-gitops`).
4. **Observe** — the engine waits through the health grace period while the
   node picks up the new configuration.
5. **Promote or rollback** — healthy nodes promote the commit to
   "last-known-good"; sustained unhealthy nodes trigger a rollback.

## Automated rollback

Health is judged from the same signals the reconciler exports to
Prometheus (`stellar_node_up`, `stellar_node_sync_status`), collected
cluster-side via pod readiness.

- If nodes stay unhealthy for `unhealthyThresholdSecs` (default: 120 s)
  after the grace period, the engine re-applies the previous known-good
  commit **and bans the bad SHA** — the poller will never re-apply it
  automatically, preventing flip-flop thrash.
- With the defaults (120 s grace + 120 s threshold) the worst-case
  misconfiguration → rollback window is 4 minutes, inside the 5-minute
  requirement with margin.

Validation scenario: commit a deliberate misconfiguration to the tracking
branch → the engine applies it → the node crashes (`stellar_node_up=0`) →
the engine detects the crash and executes the automated rollback, all
observed by `tests/gitops_e2e_test.rs`.

## Rate limits and network partitions

The engine is deliberately conservative when GitHub is unhappy:

| Condition | Behaviour |
|-----------|-----------|
| Rate limit exhausted (`x-ratelimit-remaining: 0`, HTTP 403/429) | Polling pauses until quota reset; **cluster state frozen**. Counted in `stellar_gitops_rate_limit_pauses_total`. |
| Network partition / API outage | Exponential backoff (up to 8× the poll interval); last-known-good state retained; no cluster mutations. |
| Unchanged branch (HTTP 304) | Free — no cluster work, no rate-limit cost. |

## Observability

Prometheus metrics:

| Metric | Type | Description |
|--------|------|-------------|
| `stellar_gitops_syncs_total` | Counter | Commits applied |
| `stellar_gitops_rollbacks_total` | Counter | Automated health rollbacks |
| `stellar_gitops_rate_limit_pauses_total` | Counter | Polls skipped due to rate limits |
| `stellar_gitops_sync_phase` | Gauge | 0=idle, 1=applying, 2=observing, 3=healthy, 4=rolling_back, 5=rolled_back |

The engine also registers a `gitops_sync` job in the background-job
registry, visible at `GET /api/v1/jobs?kind=gitops_sync`.

## RBAC

The engine only patches ConfigMaps, which the operator's existing Role
already permits (`configmaps: get/list/watch/create/update/patch/delete`).
No extra RBAC is required when installing via the bundled Helm chart.

## Key modules

| Module | Responsibility |
|--------|----------------|
| `controller/src/gitops/mod.rs` | Engine loop, config, shared state |
| `controller/src/gitops/github.rs` | Rate-limit-aware GitHub client |
| `controller/src/gitops/sync.rs` | Manifest rendering + ConfigMap patching |
| `controller/src/gitops/health_check.rs` | Health watchdog + rollback decision |

> Note: paths above are relative to `src/` in the repository layout
> (`src/controller/gitops/…`).
