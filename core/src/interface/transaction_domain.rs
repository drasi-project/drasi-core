// Copyright 2026 The Drasi Authors.
//
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

use std::{fmt, sync::Arc};

/// Opaque identity for resources that participate in one transaction domain.
///
/// Backend providers create one domain and share clones with their session
/// control and every writer whose staged mutations join that session. Equality
/// compares identity, not backend type or configuration.
#[derive(Clone)]
pub struct TransactionDomain(Arc<()>);

impl TransactionDomain {
    /// Create a new identity distinct from every existing transaction domain.
    pub fn new() -> Self {
        Self(Arc::new(()))
    }
}

impl Default for TransactionDomain {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for TransactionDomain {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for TransactionDomain {}

impl fmt::Debug for TransactionDomain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TransactionDomain(<opaque>)")
    }
}

/// Validated capability for staging complete persistent query output atomically.
///
/// Values are created only after a [`CreatedIndexes`](super::CreatedIndexes)
/// bundle proves that its session, checkpoint/result-sequence store, outbox, and
/// live-results writer share one [`TransactionDomain`].
#[derive(Clone, Debug)]
pub struct AtomicResultTransaction {
    transaction_domain: TransactionDomain,
}

impl AtomicResultTransaction {
    pub(crate) fn new(transaction_domain: TransactionDomain) -> Self {
        Self { transaction_domain }
    }

    pub(crate) fn matches(&self, transaction_domain: &TransactionDomain) -> bool {
        self.transaction_domain == *transaction_domain
    }
}

#[cfg(test)]
mod tests {
    use super::{AtomicResultTransaction, TransactionDomain};

    #[test]
    fn equality_tracks_shared_identity() {
        let first = TransactionDomain::new();
        let shared = first.clone();
        let second = TransactionDomain::new();

        assert_eq!(first, shared);
        assert_ne!(first, second);

        let transaction = AtomicResultTransaction::new(first.clone());
        assert!(transaction.matches(&first));
        assert!(!transaction.matches(&second));
    }
}
