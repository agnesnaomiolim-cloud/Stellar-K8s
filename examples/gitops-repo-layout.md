# GitOps tracking repository layout (Issue #286)
#
# This file documents the repository layout consumed by the GitOps
# deployment engine. The engine lives in the operator; the tracking repo
# (gitops.repo) contains one directory per StellarNode:
#
#   clusters/prod/
#   ├── my-validator/
#   │   └── stellar-core.cfg
#   ├── horizon-api/
#   │   └── horizon.env
#   └── soroban-rpc/
#       └── captive-core.cfg
#
# Directory names map to the StellarNode CR name (the
# app.kubernetes.io/instance label). Files map to ConfigMap data keys:
#
#   stellar-core.cfg  → key "stellar-core.cfg"  (Validator / Captive Core)
#   captive-core.cfg  → key "stellar-core.cfg"  (Soroban RPC captive core)
#   horizon.env       → key "horizon.env"       (Horizon ingestion config)
#
# Commit any change to the tracking branch and the operator applies it
# within pollIntervalSecs. A commit that crashes node sync is reverted
# automatically once the health watchdog's threshold is exceeded.

# ---------------------------------------------------------------------------
# clusters/prod/my-validator/stellar-core.cfg — sample validator config
# ---------------------------------------------------------------------------
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"

HISTORY.archive1.get="curl -sf https://history.stellar.org/prd/core-testnet/core_testnet_001/{0} -o {1}"

PEER_PORT=11625
HTTP_PORT=11626
LOG_LEVEL="info"

CATCHUP_COMPLETE=false
CATCHUP_RECENT=60480

# ---------------------------------------------------------------------------
# clusters/prod/horizon-api/horizon.env — sample Horizon config
# ---------------------------------------------------------------------------
# INGEST=true
# STELLAR_CORE_URL=http://stellar-core:11626
# HISTORY_ARCHIVE_URLS=https://history.stellar.org/prd/core-testnet/core_testnet_001
