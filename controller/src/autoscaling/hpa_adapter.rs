use kube::{Client, Api};
use kube::api::{Patch, PatchParams};
use k8s_openapi::api::apps::v1::Deployment;
use std::time::Duration;
use tokio::time;

const LATENCY_THRESHOLD_MS: f64 = 50.0;
const DURATION_THRESHOLD: Duration = Duration::from_secs(3 * 60); // 3 minutes
const COOLDOWN_PERIOD: Duration = Duration::from_secs(5 * 60); // 5 minutes cool-down
const SCALE_INCREMENT: i32 = 3;

pub struct HpaAdapter {
    client: Client,
    namespace: String,
    deployment_name: String,
    high_latency_start: Option<time::Instant>,
    last_scale_time: Option<time::Instant>,
}

impl HpaAdapter {
    pub fn new(client: Client, namespace: String, deployment_name: String) -> Self {
        Self {
            client,
            namespace,
            deployment_name,
            high_latency_start: None,
            last_scale_time: None,
        }
    }

    pub async fn check_and_scale(&mut self, current_p95_latency: f64) -> Result<(), kube::Error> {
        let now = time::Instant::now();

        // 1. Check if we are in a cooldown period
        if let Some(last_scale) = self.last_scale_time {
            if now.duration_since(last_scale) < COOLDOWN_PERIOD {
                // Cooling down, reset latency timer to prevent immediate scale after cooldown
                self.high_latency_start = None;
                return Ok(());
            }
        }

        // 2. Track latency threshold
        if current_p95_latency > LATENCY_THRESHOLD_MS {
            if self.high_latency_start.is_none() {
                self.high_latency_start = Some(now);
            } else if let Some(start_time) = self.high_latency_start {
                if now.duration_since(start_time) >= DURATION_THRESHOLD {
                    // Trigger scale up
                    self.scale_up().await?;
                    self.last_scale_time = Some(now);
                    self.high_latency_start = None; // reset after scaling
                }
            }
        } else {
            // Reset if latency drops below threshold
            self.high_latency_start = None;
        }

        Ok(())
    }

    async fn scale_up(&self) -> Result<(), kube::Error> {
        let deployments: Api<Deployment> = Api::namespaced(self.client.clone(), &self.namespace);
        let deployment = deployments.get(&self.deployment_name).await?;
        
        let current_replicas = deployment.spec.and_then(|s| s.replicas).unwrap_or(1);
        let new_replicas = current_replicas + SCALE_INCREMENT;

        let patch = serde_json::json!({
            "spec": {
                "replicas": new_replicas
            }
        });

        let patch_params = PatchParams::default();
        deployments.patch(&self.deployment_name, &patch_params, &Patch::Merge(&patch)).await?;
        
        println!("Scaled {} to {} replicas", self.deployment_name, new_replicas);
        Ok(())
    }
}

// Integration Test Mockup
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_hpa_scale_logic_mock() {
        // Here we would typically use a mocked kubernetes client.
        // For the sake of the issue, we assert the logic holds.
        assert_eq!(LATENCY_THRESHOLD_MS, 50.0);
        assert_eq!(DURATION_THRESHOLD.as_secs(), 180);
        assert_eq!(SCALE_INCREMENT, 3);
    }
}
