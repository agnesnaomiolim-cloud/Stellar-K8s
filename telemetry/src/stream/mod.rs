//! Streaming data-ingestion modules for the telemetry crate.
//!
//! Each sub-module owns a polling / streaming loop and publishes typed
//! snapshots over a broadcast channel so multiple consumers (WebSocket
//! handlers, Prometheus scrapers, etc.) can subscribe independently.

pub mod envoy_stats;
