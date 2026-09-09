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

use crate::interface::SessionControl;

/// A provider's assertion that participating resources join this actual session.
///
/// Providers create one identity for their constructed session, then share it
/// only with writers whose mutations really participate. Labels, backend type,
/// or identical configuration do not establish participation.
#[derive(Clone)]
pub struct TransactionDomain {
    identity: Arc<()>,
    session: Arc<dyn SessionControl>,
}

impl TransactionDomain {
    /// The provider must guarantee rollback of every participating staged write.
    /// Do not construct this for a no-op or otherwise non-atomic session.
    pub fn new(session: Arc<dyn SessionControl>) -> Self {
        Self {
            identity: Arc::new(()),
            session,
        }
    }

    pub(crate) fn belongs_to(&self, session: &Arc<dyn SessionControl>) -> bool {
        Arc::ptr_eq(&self.session, session)
    }
}

impl PartialEq for TransactionDomain {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.identity, &other.identity) && Arc::ptr_eq(&self.session, &other.session)
    }
}

impl Eq for TransactionDomain {}

impl fmt::Debug for TransactionDomain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TransactionDomain(<scoped session>)")
    }
}

/// Proof of complete participation for one constructed computation index bundle.
/// It is not a delivery acknowledgement or an external-effect transaction.
#[derive(Debug, Clone)]
pub struct AtomicResultTransaction {
    pub(super) domain: TransactionDomain,
    pub(super) bundle: Arc<()>,
}

impl AtomicResultTransaction {
    pub(crate) fn matches(
        &self,
        expected: &Self,
        actual_session: &Arc<dyn SessionControl>,
    ) -> bool {
        Arc::ptr_eq(&self.bundle, &expected.bundle)
            && self.domain == expected.domain
            && self.domain.belongs_to(actual_session)
    }
}
