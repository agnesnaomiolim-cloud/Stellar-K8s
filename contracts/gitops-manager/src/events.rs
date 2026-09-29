// src/events.rs
//! Event identifiers for the GitOpsManager contract.

/// Event emitted when a proposal is submitted.
pub const EVENT_PROPOSAL_SUBMITTED: &str = "proposal_submitted";

/// Event emitted when a proposal reaches the required multi‑sig threshold.
pub const EVENT_PROPOSAL_APPROVED: &str = "proposal_approved";
