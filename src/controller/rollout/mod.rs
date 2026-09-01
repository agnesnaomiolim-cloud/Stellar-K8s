//! Rollout gating and health-based update management
//!
//! This module provides health-gated rolling update management for Kubernetes StatefulSets,
//! particularly for Horizon nodes that require ingestion synchronization before proceeding
//! with the next pod update.
//!
//! # Overview
//!
//! The rollout gate ensures safe, ordered updates by:
//! - Monitoring pod health after updates
//! - Pausing updates until health thresholds are met
//! - Emitting Kubernetes events for operator visibility
//! - Using non-blocking async requeue loops

pub mod health;
pub mod horizon_gate;

pub use horizon_gate::HorizonRolloutGate;
pub use health::{RolloutHealthChecker, RolloutHealthConfig};

/// Annotation key for tracking rollout gate state
pub const ROLLOUT_GATE_ANNOTATION: &str = "stellar.org/rollout-gate-state";

/// Annotation key for tracking last checked pod
pub const LAST_CHECKED_POD_ANNOTATION: &str = "stellar.org/last-checked-pod";

/// Annotation key for tracking check start time
pub const CHECK_START_TIME_ANNOTATION: &str = "stellar.org/check-start-time";
