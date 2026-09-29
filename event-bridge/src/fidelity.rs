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

//! Horizon API 100% data fidelity validator.
//!
//! Validates that events streamed through the Kafka bridge match the data
//! exposed by Horizon REST API endpoints with zero discrepancies.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;

use crate::models::{HorizonOperationRecord, TokenTransferEvent};

#[derive(Debug, Error)]
pub enum FidelityError {
    #[error("Field mismatch in {field}: expected {expected}, actual {actual}")]
    FieldMismatch {
        field: String,
        expected: String,
        actual: String,
    },
    #[error("Missing record for transaction {0}")]
    MissingRecord(String),
}

/// Verification report summarizing data fidelity metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct FidelityReport {
    pub total_compared: u64,
    pub exact_matches: u64,
    pub discrepancies: u64,
    pub fidelity_percentage: f64,
}

/// Validator verifying 100% data fidelity between bridge events and Horizon API responses.
#[derive(Debug, Default)]
pub struct FidelityValidator {
    total_compared: Arc<AtomicU64>,
    exact_matches: Arc<AtomicU64>,
    discrepancies: Arc<AtomicU64>,
}

impl FidelityValidator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compares a streamed TokenTransferEvent with a HorizonOperationRecord.
    pub fn verify_token_transfer(
        &self,
        event: &TokenTransferEvent,
        horizon_op: &HorizonOperationRecord,
    ) -> Result<(), FidelityError> {
        self.total_compared.fetch_add(1, Ordering::Relaxed);

        // 1. Transaction Hash
        if event.tx_hash != horizon_op.transaction_hash {
            self.discrepancies.fetch_add(1, Ordering::Relaxed);
            return Err(FidelityError::FieldMismatch {
                field: "transaction_hash".to_string(),
                expected: horizon_op.transaction_hash.clone(),
                actual: event.tx_hash.clone(),
            });
        }

        // 2. Ledger Sequence
        if event.ledger_sequence != horizon_op.ledger_sequence {
            self.discrepancies.fetch_add(1, Ordering::Relaxed);
            return Err(FidelityError::FieldMismatch {
                field: "ledger_sequence".to_string(),
                expected: horizon_op.ledger_sequence.to_string(),
                actual: event.ledger_sequence.to_string(),
            });
        }

        // 3. Sender / From Account
        if let Some(ref expected_from) = horizon_op.from {
            if &event.from != expected_from {
                self.discrepancies.fetch_add(1, Ordering::Relaxed);
                return Err(FidelityError::FieldMismatch {
                    field: "from".to_string(),
                    expected: expected_from.clone(),
                    actual: event.from.clone(),
                });
            }
        }

        // 4. Receiver / To Account
        if let Some(ref expected_to) = horizon_op.to {
            if &event.to != expected_to {
                self.discrepancies.fetch_add(1, Ordering::Relaxed);
                return Err(FidelityError::FieldMismatch {
                    field: "to".to_string(),
                    expected: expected_to.clone(),
                    actual: event.to.clone(),
                });
            }
        }

        // 5. Amount
        if let Some(ref expected_amt) = horizon_op.amount {
            if &event.amount != expected_amt {
                self.discrepancies.fetch_add(1, Ordering::Relaxed);
                return Err(FidelityError::FieldMismatch {
                    field: "amount".to_string(),
                    expected: expected_amt.clone(),
                    actual: event.amount.clone(),
                });
            }
        }

        // 6. Asset Code
        if let Some(ref expected_code) = horizon_op.asset_code {
            if &event.asset_code != expected_code {
                self.discrepancies.fetch_add(1, Ordering::Relaxed);
                return Err(FidelityError::FieldMismatch {
                    field: "asset_code".to_string(),
                    expected: expected_code.clone(),
                    actual: event.asset_code.clone(),
                });
            }
        }

        // 7. Success Status
        if event.successful != horizon_op.successful {
            self.discrepancies.fetch_add(1, Ordering::Relaxed);
            return Err(FidelityError::FieldMismatch {
                field: "successful".to_string(),
                expected: horizon_op.successful.to_string(),
                actual: event.successful.to_string(),
            });
        }

        self.exact_matches.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Computes summary report.
    pub fn report(&self) -> FidelityReport {
        let total = self.total_compared.load(Ordering::Relaxed);
        let matches = self.exact_matches.load(Ordering::Relaxed);
        let disc = self.discrepancies.load(Ordering::Relaxed);
        let pct = if total == 0 {
            100.0
        } else {
            (matches as f64 / total as f64) * 100.0
        };

        FidelityReport {
            total_compared: total,
            exact_matches: matches,
            discrepancies: disc,
            fidelity_percentage: pct,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TransferType;

    #[test]
    fn test_fidelity_verification_pass() {
        let validator = FidelityValidator::new();

        let event = TokenTransferEvent {
            event_id: "evt-1".to_string(),
            ledger_sequence: 1234,
            tx_hash: "tx_abc".to_string(),
            operation_index: 0,
            timestamp: "2026-09-29T12:00:00Z".to_string(),
            source_account: "GA5Z...".to_string(),
            from: "GA5Z...".to_string(),
            to: "GCKF...".to_string(),
            asset_type: "credit_alphanum4".to_string(),
            asset_code: "USDC".to_string(),
            asset_issuer: Some("GBBD...".to_string()),
            amount: "500.0000000".to_string(),
            amount_stroops: 5000000000,
            successful: true,
            transfer_type: TransferType::Payment,
        };

        let horizon_op = HorizonOperationRecord {
            id: "1234-0".to_string(),
            transaction_hash: "tx_abc".to_string(),
            ledger_sequence: 1234,
            created_at: "2026-09-29T12:00:00Z".to_string(),
            source_account: "GA5Z...".to_string(),
            r#type: "payment".to_string(),
            successful: true,
            from: Some("GA5Z...".to_string()),
            to: Some("GCKF...".to_string()),
            amount: Some("500.0000000".to_string()),
            asset_type: Some("credit_alphanum4".to_string()),
            asset_code: Some("USDC".to_string()),
            asset_issuer: Some("GBBD...".to_string()),
        };

        let result = validator.verify_token_transfer(&event, &horizon_op);
        assert!(result.is_ok());

        let report = validator.report();
        assert_eq!(report.total_compared, 1);
        assert_eq!(report.exact_matches, 1);
        assert_eq!(report.discrepancies, 0);
        assert_eq!(report.fidelity_percentage, 100.0);
    }
}
