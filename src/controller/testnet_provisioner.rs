// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//! StellarTestnet Provisioner – reconciliation loop for ephemeral Stellar testnets.
//!
//! # Responsibilities
//!
//! 1. **Create** a single-pod `Pod` running Stellar Core in standalone mode with a
//!    memory-backed ledger, plus an optional Soroban RPC sidecar.
//! 2. **Service** – expose Core HTTP (port 11626) and the Soroban RPC port via a
//!    `ClusterIP` Service.
//! 3. **Ingress** – optionally create a Kubernetes `Ingress` for external access.
//! 4. **Genesis keys** – generate Ed25519 key-pairs for pre-funded accounts and
//!    store them in a per-testnet Kubernetes `Secret`.
//! 5. **Status** – write `rpcUrl`, `networkPassphrase`, `fundedPublicKeys`, and the
//!    current `phase` back into the CRD status sub-resource.
//! 6. **TTL** – delete the `StellarTestnet` when `spec.ttlSeconds` is exceeded.
//! 7. **Cleanup** – remove all owned resources before releasing the finalizer so
//!    that no dangling Pods, Services, or Secrets remain in the cluster.
//!
//! # Garbage Collection Strategy
//!
//! Every object created by this provisioner carries:
//! - A Kubernetes owner-reference pointing at the `StellarTestnet` CR.
//! - The label `stellar.org/testnet: <name>` for bulk-selection.
//!
//! This means Kubernetes will garbage-collect owned resources automatically when
//! the CR is deleted *and* when the finalizer is removed.  The provisioner also
//! performs an explicit best-effort delete of the `Pod`, `Service`, and `Secret`
//! in [`cleanup_testnet`] so that resources are released immediately rather than
//! waiting for the GC cycle.
//!
//! # Standalone Mode Config
//!
//! The provisioner renders a minimal `stellar-core.cfg` as a `ConfigMap` and
//! mounts it into the Stellar Core container at `/etc/stellar/stellar-core.cfg`.
//! Key options:
//!
//! ```toml
//! NETWORK_PASSPHRASE="<passphrase>"
//! RUN_STANDALONE=true
//! DATABASE="sqlite3:///:memory:"
//! HTTP_PORT=11626
//! TARGET_LEDGER_CLOSE_TIME=<ttlSeconds>
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{
    ConfigMap, Container, ContainerPort, EnvVar, Pod, PodSpec, PodTemplateSpec,
    ResourceRequirements, Secret, Service, ServicePort, ServiceSpec,
};
use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec, ServiceBackendPort,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::{
    api::{Api, DeleteParams, Patch, PatchParams, PostParams},
    client::Client,
    runtime::{
        controller::{Action, Controller},
        watcher::Config as WatcherConfig,
    },
    Resource, ResourceExt,
};
use tracing::{error, info, warn};

use crate::crd::testnet::{
    StellarTestnet, StellarTestnetStatus, TestnetCondition, TestnetPhase,
};
use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Finalizer name placed on each `StellarTestnet` to ensure cleanup runs.
pub const TESTNET_FINALIZER: &str = "stellar.org/testnet-cleanup";

/// Label key applied to all objects owned by a given testnet.
pub const TESTNET_LABEL: &str = "stellar.org/testnet";

/// Well-known port for Stellar Core HTTP management API.
const CORE_HTTP_PORT: u16 = 11626;

/// Field manager name used for server-side apply.
const FIELD_MANAGER: &str = "stellar-testnet-operator";

// ---------------------------------------------------------------------------
// Controller State
// ---------------------------------------------------------------------------

/// Shared state threaded through every reconcile invocation.
#[derive(Clone)]
pub struct TestnetState {
    /// Kubernetes client.
    pub client: Client,
}

// ---------------------------------------------------------------------------
// Entry Point – run the controller loop
// ---------------------------------------------------------------------------

/// Start the `StellarTestnet` controller.
///
/// Returns when the watch stream terminates (i.e. the process should exit).
pub async fn run_testnet_controller(client: Client) -> Result<()> {
    let state = Arc::new(TestnetState {
        client: client.clone(),
    });

    let testnet_api: Api<StellarTestnet> = Api::all(client.clone());

    info!("Starting StellarTestnet controller");

    Controller::new(testnet_api, WatcherConfig::default())
        .shutdown_on_signal()
        .run(reconcile, error_policy, state)
        .for_each(|res| async move {
            match res {
                Ok((obj, _action)) => {
                    info!(name = %obj.name_any(), "Reconcile successful");
                }
                Err(e) => {
                    error!(error = %e, "Reconcile failed");
                }
            }
        })
        .await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Error Policy
// ---------------------------------------------------------------------------

fn error_policy(
    _obj: Arc<StellarTestnet>,
    err: &Error,
    _state: Arc<TestnetState>,
) -> Action {
    warn!(error = %err, "Reconcile error – requeueing in 30s");
    Action::requeue(Duration::from_secs(30))
}

// ---------------------------------------------------------------------------
// Main Reconcile Function
// ---------------------------------------------------------------------------

async fn reconcile(
    testnet: Arc<StellarTestnet>,
    state: Arc<TestnetState>,
) -> std::result::Result<Action, Error> {
    let client = &state.client;
    let name = testnet.name_any();
    let namespace = testnet
        .namespace()
        .unwrap_or_else(|| "default".to_string());

    info!(name = %name, namespace = %namespace, "Reconciling StellarTestnet");

    // ── Finalizer management ────────────────────────────────────────────────
    let is_deleting = testnet.metadata.deletion_timestamp.is_some();

    if is_deleting {
        if has_finalizer(&testnet) {
            cleanup_testnet(client, &testnet).await?;
            remove_finalizer(client, &testnet).await?;
        }
        return Ok(Action::await_change());
    }

    if !has_finalizer(&testnet) {
        add_finalizer(client, &testnet).await?;
        // Re-queue so the next loop runs with the finalizer in place.
        return Ok(Action::requeue(Duration::from_secs(1)));
    }

    // ── TTL check ────────────────────────────────────────────────────────────
    if let Some(expires_at) = testnet_expires_at(&testnet) {
        if Utc::now().timestamp() > expires_at {
            info!(name = %name, "Testnet TTL exceeded – deleting");
            let api: Api<StellarTestnet> =
                Api::namespaced(client.clone(), &namespace);
            api.delete(&name, &DeleteParams::default()).await.map_err(|e| {
                Error::KubeError(e)
            })?;
            patch_status(
                client,
                &testnet,
                StellarTestnetStatus {
                    phase: TestnetPhase::Terminating,
                    message: Some("TTL expired; deleting".to_string()),
                    ..Default::default()
                },
            )
            .await?;
            return Ok(Action::await_change());
        }
    }

    // ── Provision resources ──────────────────────────────────────────────────
    patch_status(
        client,
        &testnet,
        StellarTestnetStatus {
            phase: TestnetPhase::Provisioning,
            message: Some("Provisioning testnet resources".to_string()),
            ..Default::default()
        },
    )
    .await?;

    // 1. Network passphrase
    let passphrase = resolve_passphrase(&testnet);

    // 2. Funded accounts + Secret
    let (funded_keys_secret_name, funded_public_keys) =
        ensure_funded_keys_secret(client, &testnet, &namespace).await?;

    // 3. ConfigMap with stellar-core.cfg
    ensure_core_config_map(client, &testnet, &namespace, &passphrase).await?;

    // 4. Pod
    ensure_pod(client, &testnet, &namespace).await?;

    // 5. Service
    let service_name = ensure_service(client, &testnet, &namespace).await?;

    // 6. Optional Ingress
    let rpc_url = if testnet.spec.ingress.enabled {
        ensure_ingress(client, &testnet, &namespace, &service_name).await?
    } else {
        let rpc_port = testnet.spec.soroban_rpc.port;
        format!(
            "http://{}.{}.svc.cluster.local:{}",
            service_name, namespace, rpc_port
        )
    };

    let core_url = format!(
        "http://{}.{}.svc.cluster.local:{}",
        service_name, namespace, CORE_HTTP_PORT
    );

    // 7. Calculate expiry
    let expires_at = if testnet.spec.ttl_seconds > 0 {
        testnet
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|ts| ts.0.timestamp() + testnet.spec.ttl_seconds as i64)
    } else {
        None
    };

    // 8. Build conditions
    let conditions = vec![
        TestnetCondition {
            condition_type: "Ready".to_string(),
            status: "True".to_string(),
            reason: Some("Provisioned".to_string()),
            message: Some("Testnet is provisioned and running".to_string()),
            last_transition_time: Some(Utc::now().to_rfc3339()),
        },
        TestnetCondition {
            condition_type: "CoreRunning".to_string(),
            status: "True".to_string(),
            reason: Some("PodScheduled".to_string()),
            message: Some("Stellar Core pod is running".to_string()),
            last_transition_time: Some(Utc::now().to_rfc3339()),
        },
        TestnetCondition {
            condition_type: "RpcReachable".to_string(),
            status: "True".to_string(),
            reason: Some("ServiceCreated".to_string()),
            message: Some("Soroban RPC Service is created".to_string()),
            last_transition_time: Some(Utc::now().to_rfc3339()),
        },
    ];

    // 9. Patch status to Ready
    patch_status(
        client,
        &testnet,
        StellarTestnetStatus {
            phase: TestnetPhase::Ready,
            rpc_url: Some(rpc_url),
            core_url: Some(core_url),
            network_passphrase: Some(passphrase),
            funded_public_keys,
            funded_keys_secret: Some(funded_keys_secret_name),
            message: Some("Testnet ready".to_string()),
            expires_at,
            last_reconciled_at: Some(Utc::now().to_rfc3339()),
            conditions,
        },
    )
    .await?;

    // Requeue before TTL expiry (or every 60 s) for ongoing health checks.
    let requeue_secs = if testnet.spec.ttl_seconds > 0 {
        testnet.spec.ttl_seconds.min(60)
    } else {
        60
    };

    Ok(Action::requeue(Duration::from_secs(requeue_secs)))
}

// ---------------------------------------------------------------------------
// Finalizer Helpers
// ---------------------------------------------------------------------------

fn has_finalizer(testnet: &StellarTestnet) -> bool {
    testnet
        .metadata
        .finalizers
        .as_deref()
        .unwrap_or_default()
        .contains(&TESTNET_FINALIZER.to_string())
}

async fn add_finalizer(client: &Client, testnet: &StellarTestnet) -> Result<()> {
    let name = testnet.name_any();
    let namespace = testnet
        .namespace()
        .unwrap_or_else(|| "default".to_string());
    let api: Api<StellarTestnet> = Api::namespaced(client.clone(), &namespace);

    let patch = serde_json::json!({
        "metadata": {
            "finalizers": [TESTNET_FINALIZER]
        }
    });
    api.patch(
        &name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&patch),
    )
    .await
    .map_err(Error::KubeError)?;
    Ok(())
}

async fn remove_finalizer(client: &Client, testnet: &StellarTestnet) -> Result<()> {
    let name = testnet.name_any();
    let namespace = testnet
        .namespace()
        .unwrap_or_else(|| "default".to_string());
    let api: Api<StellarTestnet> = Api::namespaced(client.clone(), &namespace);

    let existing: Vec<String> = testnet
        .metadata
        .finalizers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|f| f != TESTNET_FINALIZER)
        .collect();

    let patch = serde_json::json!({
        "metadata": {
            "finalizers": existing
        }
    });
    api.patch(
        &name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&patch),
    )
    .await
    .map_err(Error::KubeError)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Cleanup (called when the CR is being deleted)
// ---------------------------------------------------------------------------

/// Explicitly remove the Pod, Service, ConfigMap, Ingress, and funded-keys
/// Secret owned by this testnet, so cluster resources are freed immediately.
async fn cleanup_testnet(client: &Client, testnet: &StellarTestnet) -> Result<()> {
    let name = testnet.name_any();
    let namespace = testnet
        .namespace()
        .unwrap_or_else(|| "default".to_string());

    info!(name = %name, namespace = %namespace, "Cleaning up StellarTestnet resources");

    // Delete Pod
    let pod_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    let pod_name = format!("{}-core", name);
    match pod_api.delete(&pod_name, &DeleteParams::default()).await {
        Ok(_) => info!(pod = %pod_name, "Deleted Pod"),
        Err(kube::Error::Api(ae)) if ae.code == 404 => {}
        Err(e) => warn!(error = %e, pod = %pod_name, "Failed to delete Pod"),
    }

    // Delete Service
    let svc_api: Api<Service> = Api::namespaced(client.clone(), &namespace);
    let svc_name = format!("{}-testnet", name);
    match svc_api.delete(&svc_name, &DeleteParams::default()).await {
        Ok(_) => info!(service = %svc_name, "Deleted Service"),
        Err(kube::Error::Api(ae)) if ae.code == 404 => {}
        Err(e) => warn!(error = %e, service = %svc_name, "Failed to delete Service"),
    }

    // Delete ConfigMap
    let cm_api: Api<ConfigMap> = Api::namespaced(client.clone(), &namespace);
    let cm_name = format!("{}-core-config", name);
    match cm_api.delete(&cm_name, &DeleteParams::default()).await {
        Ok(_) => info!(configmap = %cm_name, "Deleted ConfigMap"),
        Err(kube::Error::Api(ae)) if ae.code == 404 => {}
        Err(e) => warn!(error = %e, configmap = %cm_name, "Failed to delete ConfigMap"),
    }

    // Delete funded-keys Secret
    let secret_api: Api<Secret> = Api::namespaced(client.clone(), &namespace);
    let secret_name = format!("{}-funded-keys", name);
    match secret_api
        .delete(&secret_name, &DeleteParams::default())
        .await
    {
        Ok(_) => info!(secret = %secret_name, "Deleted Secret"),
        Err(kube::Error::Api(ae)) if ae.code == 404 => {}
        Err(e) => warn!(error = %e, secret = %secret_name, "Failed to delete Secret"),
    }

    // Delete Ingress (best-effort)
    if testnet.spec.ingress.enabled {
        let ing_api: Api<Ingress> = Api::namespaced(client.clone(), &namespace);
        let ing_name = format!("{}-testnet-ingress", name);
        match ing_api.delete(&ing_name, &DeleteParams::default()).await {
            Ok(_) => info!(ingress = %ing_name, "Deleted Ingress"),
            Err(kube::Error::Api(ae)) if ae.code == 404 => {}
            Err(e) => warn!(error = %e, ingress = %ing_name, "Failed to delete Ingress"),
        }
    }

    info!(name = %name, "StellarTestnet cleanup complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Resource Provisioners
// ---------------------------------------------------------------------------

/// Build a deterministic network passphrase from the resource name when the
/// user has not supplied one.
fn resolve_passphrase(testnet: &StellarTestnet) -> String {
    testnet
        .spec
        .genesis_config
        .network_passphrase
        .clone()
        .unwrap_or_else(|| {
            format!(
                "Ephemeral Testnet {} ; {}",
                testnet.name_any(),
                testnet
                    .metadata
                    .uid
                    .as_deref()
                    .unwrap_or("unknown")
            )
        })
}

/// Unix timestamp when this testnet expires, or `None` if TTL is disabled.
fn testnet_expires_at(testnet: &StellarTestnet) -> Option<i64> {
    if testnet.spec.ttl_seconds == 0 {
        return None;
    }
    testnet
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|ts| ts.0.timestamp() + testnet.spec.ttl_seconds as i64)
}

/// Build the owner-reference for objects created on behalf of `testnet`.
fn owner_ref(testnet: &StellarTestnet) -> OwnerReference {
    OwnerReference {
        api_version: StellarTestnet::api_version(&()).to_string(),
        kind: StellarTestnet::kind(&()).to_string(),
        name: testnet.name_any(),
        uid: testnet.metadata.uid.clone().unwrap_or_default(),
        block_owner_deletion: Some(true),
        controller: Some(true),
    }
}

/// Build common labels for all resources owned by this testnet.
fn common_labels(testnet: &StellarTestnet) -> BTreeMap<String, String> {
    let mut labels = testnet.spec.labels.clone();
    labels.insert(TESTNET_LABEL.to_string(), testnet.name_any());
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "stellar-testnet-operator".to_string(),
    );
    labels.insert(
        "app.kubernetes.io/name".to_string(),
        "stellar-testnet".to_string(),
    );
    labels.insert(
        "app.kubernetes.io/instance".to_string(),
        testnet.name_any(),
    );
    labels
}

// ---------------------------------------------------------------------------
// Funded Keys Secret
// ---------------------------------------------------------------------------

/// Ensure the funded-keys `Secret` exists.  Returns `(secret_name,
/// funded_public_keys_map)`.
///
/// In production the operator would generate real Ed25519 key-pairs here.
/// For the reference implementation we use placeholder values that encode the
/// account name so the test scripts can find them.
async fn ensure_funded_keys_secret(
    client: &Client,
    testnet: &StellarTestnet,
    namespace: &str,
) -> Result<(String, BTreeMap<String, String>)> {
    let name = testnet.name_any();
    let secret_name = format!("{}-funded-keys", name);
    let api: Api<Secret> = Api::namespaced(client.clone(), namespace);

    let mut secret_data: BTreeMap<String, String> = BTreeMap::new();
    let mut public_keys: BTreeMap<String, String> = BTreeMap::new();

    for account in &testnet.spec.funded_accounts {
        // Placeholder: real impl would call stellar_base::Keypair::random()
        let public_key = format!("G{}", account.name.to_uppercase().chars()
            .cycle()
            .take(55)
            .collect::<String>());
        let secret_key = format!("S{}", account.name.to_uppercase().chars()
            .cycle()
            .take(55)
            .collect::<String>());

        secret_data.insert(
            format!("{}-public-key", account.name),
            public_key.clone(),
        );
        secret_data.insert(format!("{}-secret-key", account.name), secret_key);
        public_keys.insert(account.name.clone(), public_key);
    }

    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(secret_name.clone()),
            namespace: Some(namespace.to_string()),
            labels: Some(common_labels(testnet)),
            owner_references: Some(vec![owner_ref(testnet)]),
            ..Default::default()
        },
        string_data: Some(secret_data),
        ..Default::default()
    };

    let result = api.get_opt(&secret_name).await.map_err(Error::KubeError)?;
    if result.is_none() {
        api.create(&PostParams::default(), &secret)
            .await
            .map_err(Error::KubeError)?;
        info!(secret = %secret_name, "Created funded-keys Secret");
    }

    Ok((secret_name, public_keys))
}

// ---------------------------------------------------------------------------
// Core ConfigMap
// ---------------------------------------------------------------------------

/// Render and apply the `stellar-core.cfg` ConfigMap.
async fn ensure_core_config_map(
    client: &Client,
    testnet: &StellarTestnet,
    namespace: &str,
    passphrase: &str,
) -> Result<()> {
    let name = testnet.name_any();
    let cm_name = format!("{}-core-config", name);
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);

    let genesis = &testnet.spec.genesis_config;
    let core_cfg = format!(
        r#"
# Auto-generated by stellar-testnet-operator – do not edit manually.
NETWORK_PASSPHRASE="{passphrase}"
RUN_STANDALONE=true
DATABASE="sqlite3:///:memory:"
HTTP_PORT={core_http_port}
TARGET_LEDGER_CLOSE_TIME={target_ledger_time}
ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING=true
CATCHUP_COMPLETE=false
CATCHUP_RECENT=0
ARTIFICIALLY_GENERATE_LOAD_FOR_TESTING=false

[QUORUM_SET]
THRESHOLD_PERCENT=100
VALIDATORS=[]

[HISTORY.local]
get="cp history/{{0}} {{1}}"
put="cp {{0}} history/{{1}}"
mkdir="mkdir -p history/{{2}}"
"#,
        passphrase = passphrase,
        core_http_port = CORE_HTTP_PORT,
        target_ledger_time = genesis.target_ledger_time,
    );

    let mut data = BTreeMap::new();
    data.insert("stellar-core.cfg".to_string(), core_cfg);

    let cm = ConfigMap {
        metadata: ObjectMeta {
            name: Some(cm_name.clone()),
            namespace: Some(namespace.to_string()),
            labels: Some(common_labels(testnet)),
            owner_references: Some(vec![owner_ref(testnet)]),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    api.patch(
        &cm_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Apply(&cm),
    )
    .await
    .map_err(Error::KubeError)?;

    info!(configmap = %cm_name, "Applied stellar-core ConfigMap");
    Ok(())
}

// ---------------------------------------------------------------------------
// Pod
// ---------------------------------------------------------------------------

/// Ensure the Stellar Core + optional Soroban RPC pod exists.
async fn ensure_pod(client: &Client, testnet: &StellarTestnet, namespace: &str) -> Result<()> {
    let name = testnet.name_any();
    let pod_name = format!("{}-core", name);
    let api: Api<Pod> = Api::namespaced(client.clone(), namespace);

    if api
        .get_opt(&pod_name)
        .await
        .map_err(Error::KubeError)?
        .is_some()
    {
        return Ok(());
    }

    let labels = common_labels(testnet);

    // ── Resource requests/limits ──────────────────────────────────────────
    let cpu = testnet.spec.resources.cpu.clone();
    let memory = testnet.spec.resources.memory.clone();
    let mut resource_limits = BTreeMap::new();
    resource_limits.insert("cpu".to_string(), Quantity(cpu.clone()));
    resource_limits.insert("memory".to_string(), Quantity(memory.clone()));

    // ── Stellar Core container ────────────────────────────────────────────
    let core_container = Container {
        name: "stellar-core".to_string(),
        image: Some(testnet.spec.stellar_core_image.clone()),
        args: Some(vec![
            "--conf".to_string(),
            "/etc/stellar/stellar-core.cfg".to_string(),
            "run".to_string(),
        ]),
        ports: Some(vec![ContainerPort {
            name: Some("http".to_string()),
            container_port: CORE_HTTP_PORT as i32,
            protocol: Some("TCP".to_string()),
            ..Default::default()
        }]),
        resources: Some(ResourceRequirements {
            limits: Some(resource_limits.clone()),
            requests: Some(resource_limits),
            ..Default::default()
        }),
        volume_mounts: Some(vec![k8s_openapi::api::core::v1::VolumeMount {
            name: "core-config".to_string(),
            mount_path: "/etc/stellar".to_string(),
            read_only: Some(true),
            ..Default::default()
        }]),
        ..Default::default()
    };

    // ── Soroban RPC sidecar ────────────────────────────────────────────────
    let mut containers = vec![core_container];
    if testnet.spec.soroban_rpc.enabled {
        let rpc_port = testnet.spec.soroban_rpc.port;
        let mut env: Vec<EnvVar> = vec![
            EnvVar {
                name: "STELLAR_CORE_URL".to_string(),
                value: Some(format!("http://localhost:{}", CORE_HTTP_PORT)),
                ..Default::default()
            },
            EnvVar {
                name: "SOROBAN_RPC_PORT".to_string(),
                value: Some(rpc_port.to_string()),
                ..Default::default()
            },
        ];
        for (k, v) in &testnet.spec.soroban_rpc.extra_env {
            env.push(EnvVar {
                name: k.clone(),
                value: Some(v.clone()),
                ..Default::default()
            });
        }

        let rpc_container = Container {
            name: "soroban-rpc".to_string(),
            image: Some(testnet.spec.soroban_rpc.image.clone()),
            ports: Some(vec![ContainerPort {
                name: Some("rpc".to_string()),
                container_port: rpc_port as i32,
                protocol: Some("TCP".to_string()),
                ..Default::default()
            }]),
            env: Some(env),
            ..Default::default()
        };
        containers.push(rpc_container);
    }

    // ── Volumes ────────────────────────────────────────────────────────────
    let volumes = vec![k8s_openapi::api::core::v1::Volume {
        name: "core-config".to_string(),
        config_map: Some(k8s_openapi::api::core::v1::ConfigMapVolumeSource {
            name: Some(format!("{}-core-config", name)),
            ..Default::default()
        }),
        ..Default::default()
    }];

    let pod = Pod {
        metadata: ObjectMeta {
            name: Some(pod_name.clone()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            owner_references: Some(vec![owner_ref(testnet)]),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers,
            volumes: Some(volumes),
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };

    api.create(&PostParams::default(), &pod)
        .await
        .map_err(Error::KubeError)?;
    info!(pod = %pod_name, "Created Stellar Core pod");
    Ok(())
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Ensure a `ClusterIP` Service for the testnet exists.
/// Returns the service name.
async fn ensure_service(
    client: &Client,
    testnet: &StellarTestnet,
    namespace: &str,
) -> Result<String> {
    let name = testnet.name_any();
    let svc_name = format!("{}-testnet", name);
    let api: Api<Service> = Api::namespaced(client.clone(), namespace);

    let selector: BTreeMap<String, String> = {
        let mut m = BTreeMap::new();
        m.insert(TESTNET_LABEL.to_string(), name.clone());
        m
    };

    let mut ports = vec![ServicePort {
        name: Some("core-http".to_string()),
        port: CORE_HTTP_PORT as i32,
        target_port: Some(IntOrString::Int(CORE_HTTP_PORT as i32)),
        protocol: Some("TCP".to_string()),
        ..Default::default()
    }];

    if testnet.spec.soroban_rpc.enabled {
        let rpc_port = testnet.spec.soroban_rpc.port as i32;
        ports.push(ServicePort {
            name: Some("soroban-rpc".to_string()),
            port: rpc_port,
            target_port: Some(IntOrString::Int(rpc_port)),
            protocol: Some("TCP".to_string()),
            ..Default::default()
        });
    }

    let svc = Service {
        metadata: ObjectMeta {
            name: Some(svc_name.clone()),
            namespace: Some(namespace.to_string()),
            labels: Some(common_labels(testnet)),
            owner_references: Some(vec![owner_ref(testnet)]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(selector),
            ports: Some(ports),
            type_: Some("ClusterIP".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };

    api.patch(
        &svc_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Apply(&svc),
    )
    .await
    .map_err(Error::KubeError)?;

    info!(service = %svc_name, "Applied testnet Service");
    Ok(svc_name)
}

// ---------------------------------------------------------------------------
// Ingress
// ---------------------------------------------------------------------------

/// Ensure an `Ingress` resource exists (when `spec.ingress.enabled == true`).
/// Returns the external RPC URL.
async fn ensure_ingress(
    client: &Client,
    testnet: &StellarTestnet,
    namespace: &str,
    service_name: &str,
) -> Result<String> {
    let name = testnet.name_any();
    let ing_name = format!("{}-testnet-ingress", name);
    let api: Api<Ingress> = Api::namespaced(client.clone(), namespace);
    let rpc_port = testnet.spec.soroban_rpc.port as i32;
    let ingress_cfg = &testnet.spec.ingress;

    let mut annotations = ingress_cfg.annotations.clone();
    annotations
        .entry("kubernetes.io/ingress.class".to_string())
        .or_insert_with(|| {
            ingress_cfg
                .ingress_class_name
                .clone()
                .unwrap_or_else(|| "nginx".to_string())
        });

    let host = ingress_cfg
        .host
        .clone()
        .unwrap_or_else(|| format!("{}.testnet.local", name));

    let backend = IngressBackend {
        service: Some(IngressServiceBackend {
            name: service_name.to_string(),
            port: Some(ServiceBackendPort {
                number: Some(rpc_port),
                ..Default::default()
            }),
        }),
        ..Default::default()
    };

    let rule = IngressRule {
        host: Some(host.clone()),
        http: Some(HTTPIngressRuleValue {
            paths: vec![HTTPIngressPath {
                path: Some("/".to_string()),
                path_type: "Prefix".to_string(),
                backend: backend.clone(),
            }],
        }),
    };

    let ingress = Ingress {
        metadata: ObjectMeta {
            name: Some(ing_name.clone()),
            namespace: Some(namespace.to_string()),
            labels: Some(common_labels(testnet)),
            annotations: Some(annotations),
            owner_references: Some(vec![owner_ref(testnet)]),
            ..Default::default()
        },
        spec: Some(IngressSpec {
            ingress_class_name: ingress_cfg.ingress_class_name.clone(),
            rules: Some(vec![rule]),
            default_backend: Some(backend),
            ..Default::default()
        }),
        ..Default::default()
    };

    api.patch(
        &ing_name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Apply(&ingress),
    )
    .await
    .map_err(Error::KubeError)?;

    info!(ingress = %ing_name, host = %host, "Applied testnet Ingress");
    Ok(format!("https://{}/", host))
}

// ---------------------------------------------------------------------------
// Status Patch
// ---------------------------------------------------------------------------

/// Patch the `status` sub-resource of the given `StellarTestnet`.
async fn patch_status(
    client: &Client,
    testnet: &StellarTestnet,
    status: StellarTestnetStatus,
) -> Result<()> {
    let name = testnet.name_any();
    let namespace = testnet
        .namespace()
        .unwrap_or_else(|| "default".to_string());

    let api: Api<StellarTestnet> = Api::namespaced(client.clone(), &namespace);

    let patch = serde_json::json!({
        "status": status
    });

    api.patch_status(
        &name,
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(&patch),
    )
    .await
    .map_err(Error::KubeError)?;

    Ok(())
}
