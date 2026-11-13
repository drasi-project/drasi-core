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

use drasi_core::{
    interface::{OutputPartition, StorageScope},
    ComponentId,
};

pub(crate) struct OutputScope {
    storage_key: String,
    component_id: ComponentId,
}

impl OutputScope {
    pub(crate) fn new(scope: &StorageScope, component_id: &ComponentId) -> Self {
        Self {
            storage_key: scope.component_key(component_id),
            component_id: component_id.clone(),
        }
    }

    pub(crate) fn storage_key(&self) -> &str {
        &self.storage_key
    }

    pub(crate) fn key(&self, prefix: &str, output: &OutputPartition) -> String {
        let primary = format!("{prefix}:{{{}}}", self.storage_key);
        if matches!(output, OutputPartition::Component(id) if id == &self.component_id) {
            primary
        } else {
            let encoded: String = output
                .storage_key()
                .bytes()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            format!("{primary}:query:{encoded}")
        }
    }

    pub(crate) fn component_key(&self, prefix: &str, component: &ComponentId) -> String {
        self.key(prefix, &OutputPartition::Component(component.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::OutputScope;
    use drasi_core::{
        interface::{OutputPartition, StorageScope},
        ComponentId,
    };

    #[test]
    fn primary_output_keys_preserve_the_storage_scope() {
        let owner = ComponentId::generate();
        let storage = StorageScope::new("storage-scope");
        let scope = OutputScope::new(&storage, &owner);
        let primary = OutputPartition::Component(owner.clone());
        for prefix in ["outbox", "outbox_data", "live"] {
            assert_eq!(
                scope.key(prefix, &primary),
                format!("{prefix}:{{{}}}", storage.component_key(&owner)),
            );
        }
    }

    #[test]
    fn auxiliary_ids_are_distinct_and_preserve_the_primary_hash_tag() {
        let owner = ComponentId::generate();
        let storage = StorageScope::new("storage-scope");
        let scope = OutputScope::new(&storage, &owner);
        let primary = scope.key("outbox", &OutputPartition::Component(owner));
        let mut keys = std::collections::HashSet::new();
        keys.insert(primary.clone());
        for id in [
            "logical-query",
            "storage-scope",
            "",
            "a",
            "ab",
            "a:b",
            "{other}",
            "☃",
        ] {
            let key = scope.key("outbox", &OutputPartition::Journal(id.into()));
            assert!(key.starts_with(&primary));
            assert_eq!(key.matches('{').count(), 1);
            assert_eq!(key.matches('}').count(), 1);
            assert!(keys.insert(key));
        }
    }

    #[test]
    fn identical_logical_ids_in_different_storage_scopes_are_isolated() {
        let owner = ComponentId::generate();
        let first = OutputScope::new(&StorageScope::new("first"), &owner);
        let second = OutputScope::new(&StorageScope::new("second"), &owner);
        for output in [
            OutputPartition::Component(owner),
            OutputPartition::Journal("auxiliary".into()),
        ] {
            assert_ne!(first.key("outbox", &output), second.key("outbox", &output));
        }
    }
}
