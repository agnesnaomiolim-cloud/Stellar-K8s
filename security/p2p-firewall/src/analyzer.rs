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
//! XDR Packet Analyzer — parses and classifies incoming SCP gossip packets.
//!
//! # Design
//!
//! The Stellar P2P overlay uses an XDR-encoded framing protocol.  Every
//! message on port 11625 is preceded by a 4-byte big-endian length prefix
//! (the "record-mark"), followed by the XDR-encoded [`StellarMessage`].
//!
//! The analyzer does **not** perform a full protocol decode.  Instead it
//! applies lightweight heuristics in strict latency order to achieve the
//! sub-millisecond requirement:
//!
//! 1. **Minimum-size check** — a valid SCP message is at least 8 bytes
//!    (4-byte length + 4-byte discriminant).  Anything smaller is dropped.
//! 2. **Length-field sanity** — the claimed body length must be ≤ 64 KiB
//!    (Stellar's current max message size).  Larger payloads are flagged as
//!    malformed.
//! 3. **XDR discriminant check** — the first 4 bytes of the body encode the
//!    `StellarMessageType` enum.  Values outside the known range are flagged.
//! 4. **Flood detection** — per-IP packet-per-second counter; exceeding
//!    `flood_pps_threshold` triggers a ban.
//! 5. **Rapid handshake-failure detection** — repeated TCP RST events during
//!    the first 3 seconds of a connection indicate a failed cryptographic
//!    handshake.

use crate::interceptor::RawPacket;
use bytes::Buf;
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};
use tracing::{debug, warn};

// ─── Known XDR StellarMessageType discriminants ──────────────────────────────
//
// From Stellar's stellar-xdr / src/protocol.x:
//   ERROR_MSG        = 0
//   AUTH             = 2
//   DONT_HAVE        = 3
//   GET_PEERS        = 4
//   PEERS            = 5
//   GET_TX_SET       = 6
//   TX_SET           = 7
//   TRANSACTION      = 8
//   GET_SCP_QUORUMSET= 9
//   SCP_QUORUMSET    = 10
//   SCP_MESSAGE      = 11
//   GET_SCP_STATE    = 12
//   HELLO            = 13
//   SURVEY_REQUEST   = 14
//   SURVEY_RESPONSE  = 15
//   SEND_MORE        = 16
//   FLOOD_ADVERT     = 18
//   FLOOD_DEMAND     = 19
const MAX_KNOWN_MSG_TYPE: u32 = 19;
/// Maximum valid XDR body size (64 KiB).
const MAX_XDR_BODY_BYTES: u32 = 65_536;
/// XDR record-mark header is 4 bytes.
const XDR_RECORD_MARK_LEN: usize = 4;
/// Minimum valid SCP message: 4-byte record-mark + 4-byte discriminant.
const MIN_VALID_MSG_LEN: usize = 8;

// ─── Public types ─────────────────────────────────────────────────────────────

/// Classification of a detected threat.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ThreatKind {
    /// Payload too small to be a valid XDR message.
    MalformedTooSmall,
    /// XDR body length field exceeds the protocol maximum.
    MalformedOversizeLength,
    /// Unknown/invalid XDR message-type discriminant.
    MalformedBadDiscriminant { discriminant: u32 },
    /// Peer is sending packets faster than `flood_pps_threshold`.
    FloodAttack { pps: u64 },
    /// Peer failed the cryptographic handshake repeatedly.
    HandshakeFlood { failures: u32 },
}

impl std::fmt::Display for ThreatKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedTooSmall => write!(f, "malformed_too_small"),
            Self::MalformedOversizeLength => write!(f, "malformed_oversize_length"),
            Self::MalformedBadDiscriminant { discriminant } => {
                write!(f, "malformed_bad_discriminant({})", discriminant)
            }
            Self::FloodAttack { pps } => write!(f, "flood_attack({}pps)", pps),
            Self::HandshakeFlood { failures } => {
                write!(f, "handshake_flood({} failures)", failures)
            }
        }
    }
}

/// Result of analyzing a single packet.
#[derive(Debug, Clone)]
pub struct AnalysisResult {
    /// The source IP of the offending peer.
    pub peer_ip: IpAddr,
    /// What kind of threat was detected.
    pub kind: ThreatKind,
    /// Whether the ban manager should immediately ban this peer.
    pub should_ban: bool,
}

// ─── Per-peer state ───────────────────────────────────────────────────────────

#[derive(Debug)]
struct PeerState {
    /// Packet count within the current rate window.
    pkt_count: u64,
    /// Start of the current rate window.
    window_start: Instant,
    /// Number of handshake failures within the current rate window.
    handshake_fails: u32,
    /// Peak PPS observed in the most recently closed window.
    peak_pps: u64,
}

impl PeerState {
    fn new() -> Self {
        Self {
            pkt_count: 0,
            window_start: Instant::now(),
            handshake_fails: 0,
            peak_pps: 0,
        }
    }
}

// ─── Analyzer ─────────────────────────────────────────────────────────────────

/// Stateful XDR packet analyzer with per-peer heuristics.
///
/// Thread-safe: all mutable state is protected by an internal `Mutex`.
pub struct PacketAnalyzer {
    flood_pps_threshold: u64,
    handshake_fail_threshold: u32,
    rate_window: Duration,
    peer_state: Mutex<HashMap<IpAddr, PeerState>>,
}

impl PacketAnalyzer {
    /// Create a new analyzer.
    ///
    /// # Arguments
    ///
    /// * `flood_pps_threshold` — packets-per-second over the window that
    ///   triggers a flood ban.
    /// * `handshake_fail_threshold` — repeated RST-during-handshake events
    ///   within the window that trigger a ban.
    /// * `rate_window_secs` — width of the sliding rate window in seconds.
    pub fn new(
        flood_pps_threshold: u64,
        handshake_fail_threshold: u32,
        rate_window_secs: u64,
    ) -> Self {
        Self {
            flood_pps_threshold,
            handshake_fail_threshold,
            rate_window: Duration::from_secs(rate_window_secs),
            peer_state: Mutex::new(HashMap::new()),
        }
    }

    /// Analyze a single raw packet.
    ///
    /// Returns `Some(AnalysisResult)` if a threat was detected, `None` if the
    /// packet is benign.  This call is designed to complete in **O(1)** time
    /// with no heap allocations on the happy path.
    pub fn analyze(&self, pkt: &RawPacket) -> Option<AnalysisResult> {
        // ── 1. Structural / size heuristics (allocation-free) ──────────────
        if let Some(kind) = self.check_structure(pkt) {
            warn!(peer = %pkt.src_ip, kind = %kind, "malformed XDR packet detected");
            return Some(AnalysisResult {
                peer_ip: pkt.src_ip,
                kind,
                should_ban: true,
            });
        }

        // ── 2. Handshake failure check ─────────────────────────────────────
        if pkt.is_handshake_fail {
            if let Some(result) = self.record_handshake_fail(pkt.src_ip) {
                return Some(result);
            }
        }

        // ── 3. Flood / rate check ──────────────────────────────────────────
        self.check_flood(pkt)
    }

    // ── Structural check ────────────────────────────────────────────────────

    fn check_structure(&self, pkt: &RawPacket) -> Option<ThreatKind> {
        let payload = &pkt.payload;

        // Too small to contain the XDR record-mark + discriminant.
        if payload.len() < MIN_VALID_MSG_LEN {
            debug!(
                peer = %pkt.src_ip,
                len = payload.len(),
                "packet too small"
            );
            return Some(ThreatKind::MalformedTooSmall);
        }

        // Parse the 4-byte big-endian record-mark (body length).
        let mut cursor = &payload[..];
        let body_len = cursor.get_u32();
        // Strip the record-mark "last-fragment" high bit (RFC 1831 §10).
        let body_len = body_len & 0x7FFF_FFFF;

        if body_len > MAX_XDR_BODY_BYTES {
            debug!(
                peer = %pkt.src_ip,
                body_len,
                "XDR body length exceeds maximum"
            );
            return Some(ThreatKind::MalformedOversizeLength);
        }

        // Must have at least 4 more bytes for the message-type discriminant.
        if payload.len() < XDR_RECORD_MARK_LEN + 4 {
            return Some(ThreatKind::MalformedTooSmall);
        }

        // Read the XDR message-type discriminant.
        let discriminant = cursor.get_u32();
        if discriminant > MAX_KNOWN_MSG_TYPE {
            debug!(
                peer = %pkt.src_ip,
                discriminant,
                "unknown XDR message-type discriminant"
            );
            return Some(ThreatKind::MalformedBadDiscriminant { discriminant });
        }

        None
    }

    // ── Handshake failure ───────────────────────────────────────────────────

    fn record_handshake_fail(&self, ip: IpAddr) -> Option<AnalysisResult> {
        let mut states = self.peer_state.lock().unwrap();
        let now = Instant::now();
        let state = states.entry(ip).or_insert_with(PeerState::new);

        // Reset window if expired.
        if now.duration_since(state.window_start) > self.rate_window {
            state.handshake_fails = 0;
            state.window_start = now;
        }

        state.handshake_fails += 1;

        if state.handshake_fails >= self.handshake_fail_threshold {
            let failures = state.handshake_fails;
            warn!(peer = %ip, failures, "handshake flood detected");
            Some(AnalysisResult {
                peer_ip: ip,
                kind: ThreatKind::HandshakeFlood { failures },
                should_ban: true,
            })
        } else {
            None
        }
    }

    // ── Flood detection ─────────────────────────────────────────────────────

    fn check_flood(&self, pkt: &RawPacket) -> Option<AnalysisResult> {
        let mut states = self.peer_state.lock().unwrap();
        let now = Instant::now();
        let state = states.entry(pkt.src_ip).or_insert_with(PeerState::new);

        let elapsed = now.duration_since(state.window_start);
        if elapsed > self.rate_window {
            // Compute PPS for the window that just closed.
            let window_secs = elapsed.as_secs_f64().max(0.001);
            state.peak_pps = (state.pkt_count as f64 / window_secs) as u64;
            // Start new window.
            state.pkt_count = 0;
            state.window_start = now;
        }

        state.pkt_count += 1;

        // Early detection: check instantaneous rate within window.
        let elapsed_secs = elapsed.as_secs_f64().max(0.001);
        let instant_pps = (state.pkt_count as f64 / elapsed_secs) as u64;

        if instant_pps > self.flood_pps_threshold {
            warn!(
                peer = %pkt.src_ip,
                pps = instant_pps,
                threshold = self.flood_pps_threshold,
                "flood attack detected"
            );
            Some(AnalysisResult {
                peer_ip: pkt.src_ip,
                kind: ThreatKind::FloodAttack { pps: instant_pps },
                should_ban: true,
            })
        } else {
            None
        }
    }

    /// Evict state entries for peers that have been silent longer than twice
    /// the rate window (called by the ban sweeper).
    pub fn evict_stale(&self) {
        let cutoff = Instant::now() - self.rate_window * 2;
        let mut states = self.peer_state.lock().unwrap();
        states.retain(|_, v| v.window_start > cutoff);
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn make_pkt(ip: IpAddr, payload: Vec<u8>) -> RawPacket {
        RawPacket {
            src_ip: ip,
            payload,
            is_handshake_fail: false,
            timestamp_ns: 0,
        }
    }

    fn make_pkt_hs_fail(ip: IpAddr) -> RawPacket {
        RawPacket {
            src_ip: ip,
            payload: valid_payload(),
            is_handshake_fail: true,
            timestamp_ns: 0,
        }
    }

    /// Build a minimal valid SCP message: 4-byte length + type 11 (SCP_MESSAGE).
    fn valid_payload() -> Vec<u8> {
        let mut v = Vec::new();
        // Record-mark: body length = 4 bytes, last-fragment bit set.
        v.extend_from_slice(&(0x8000_0004u32).to_be_bytes());
        // Discriminant: SCP_MESSAGE = 11.
        v.extend_from_slice(&11u32.to_be_bytes());
        v
    }

    #[test]
    fn test_valid_packet_passes() {
        let analyzer = PacketAnalyzer::new(10_000, 5, 10);
        let pkt = make_pkt(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), valid_payload());
        assert!(analyzer.analyze(&pkt).is_none());
    }

    #[test]
    fn test_too_small_flagged() {
        let analyzer = PacketAnalyzer::new(10_000, 5, 10);
        let pkt = make_pkt(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), vec![0x00, 0x01]);
        let result = analyzer.analyze(&pkt).unwrap();
        assert_eq!(result.kind, ThreatKind::MalformedTooSmall);
        assert!(result.should_ban);
    }

    #[test]
    fn test_oversize_length_flagged() {
        let analyzer = PacketAnalyzer::new(10_000, 5, 10);
        let mut payload = Vec::new();
        // Body length = 100_000 (exceeds 64 KiB).
        payload.extend_from_slice(&100_000u32.to_be_bytes());
        payload.extend_from_slice(&11u32.to_be_bytes());
        let pkt = make_pkt(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), payload);
        let result = analyzer.analyze(&pkt).unwrap();
        assert_eq!(result.kind, ThreatKind::MalformedOversizeLength);
    }

    #[test]
    fn test_bad_discriminant_flagged() {
        let analyzer = PacketAnalyzer::new(10_000, 5, 10);
        let mut payload = Vec::new();
        payload.extend_from_slice(&(0x8000_0004u32).to_be_bytes());
        // Discriminant 9999 is unknown.
        payload.extend_from_slice(&9999u32.to_be_bytes());
        let pkt = make_pkt(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), payload);
        let result = analyzer.analyze(&pkt).unwrap();
        assert!(matches!(
            result.kind,
            ThreatKind::MalformedBadDiscriminant { discriminant: 9999 }
        ));
    }

    #[test]
    fn test_flood_detected_after_threshold() {
        let analyzer = PacketAnalyzer::new(10, 100, 10);
        let ip = IpAddr::V4(Ipv4Addr::new(2, 3, 4, 5));
        let mut ban_triggered = false;
        // Send 100 packets rapidly; flood detector should trip well before that.
        for _ in 0..100 {
            let pkt = make_pkt(ip, valid_payload());
            if let Some(r) = analyzer.analyze(&pkt) {
                if matches!(r.kind, ThreatKind::FloodAttack { .. }) {
                    ban_triggered = true;
                    break;
                }
            }
        }
        assert!(ban_triggered, "flood should have been detected");
    }

    #[test]
    fn test_handshake_fail_threshold() {
        let threshold = 3;
        let analyzer = PacketAnalyzer::new(10_000, threshold, 10);
        let ip = IpAddr::V4(Ipv4Addr::new(3, 4, 5, 6));
        let mut ban_triggered = false;
        for _ in 0..(threshold + 1) {
            let pkt = make_pkt_hs_fail(ip);
            if let Some(r) = analyzer.analyze(&pkt) {
                if matches!(r.kind, ThreatKind::HandshakeFlood { .. }) {
                    ban_triggered = true;
                    break;
                }
            }
        }
        assert!(ban_triggered, "handshake flood should have been detected");
    }

    #[test]
    fn test_known_message_types_are_accepted() {
        let analyzer = PacketAnalyzer::new(10_000, 100, 10);
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        // Test all known discriminants 0–19.
        for disc in 0u32..=19 {
            let mut payload = Vec::new();
            payload.extend_from_slice(&(0x8000_0004u32).to_be_bytes());
            payload.extend_from_slice(&disc.to_be_bytes());
            let pkt = make_pkt(ip, payload);
            let result = analyzer.analyze(&pkt);
            // Only structural checks should pass here (flood won't trip for 20 pkts).
            if let Some(r) = &result {
                assert!(
                    !matches!(r.kind, ThreatKind::MalformedBadDiscriminant { .. }),
                    "discriminant {} should be known",
                    disc
                );
            }
        }
    }

    #[test]
    fn test_evict_stale_does_not_panic() {
        let analyzer = PacketAnalyzer::new(10_000, 5, 1);
        let ip = IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8));
        let pkt = make_pkt(ip, valid_payload());
        let _ = analyzer.analyze(&pkt);
        analyzer.evict_stale();
    }
}
