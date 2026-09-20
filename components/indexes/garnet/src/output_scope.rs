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

pub(crate) struct OutputScope {
    storage_scope: String,
    query_id: String,
}

impl OutputScope {
    pub(crate) fn new(storage_scope: &str) -> Self {
        Self {
            storage_scope: storage_scope.to_owned(),
            query_id: storage_scope.to_owned(),
        }
    }

    pub(crate) fn bind_query_id(&mut self, query_id: &str) {
        self.query_id = query_id.to_owned();
    }

    pub(crate) fn key(&self, prefix: &str, query_id: &str) -> String {
        let primary = format!("{prefix}:{{{}}}", self.storage_scope);
        if query_id == self.query_id {
            primary
        } else {
            let encoded: String = query_id.bytes().map(|byte| format!("{byte:02x}")).collect();
            format!("{primary}:query:{encoded}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OutputScope;

    #[test]
    fn primary_output_keys_preserve_the_storage_scope() {
        let mut scope = OutputScope::new("storage-scope");
        assert_eq!(
            scope.key("outbox", "storage-scope"),
            "outbox:{storage-scope}"
        );
        scope.bind_query_id("logical-query");
        assert_eq!(
            scope.key("outbox", "logical-query"),
            "outbox:{storage-scope}"
        );
        assert_eq!(
            scope.key("outbox_data", "logical-query"),
            "outbox_data:{storage-scope}"
        );
        assert_eq!(scope.key("live", "logical-query"), "live:{storage-scope}");
    }

    #[test]
    fn auxiliary_ids_are_distinct_and_preserve_the_primary_hash_tag() {
        let mut scope = OutputScope::new("storage-scope");
        scope.bind_query_id("logical-query");
        let mut keys = std::collections::HashSet::new();
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
            let key = scope.key("outbox", id);
            assert!(key.starts_with("outbox:{storage-scope}"));
            assert_eq!(key.matches('{').count(), 1);
            assert_eq!(key.matches('}').count(), 1);
            assert!(keys.insert(key));
        }
    }

    #[test]
    fn identical_logical_ids_in_different_storage_scopes_are_isolated() {
        let mut first = OutputScope::new("first");
        let mut second = OutputScope::new("second");
        first.bind_query_id("query");
        second.bind_query_id("query");
        for id in ["query", "auxiliary"] {
            assert_ne!(first.key("outbox", id), second.key("outbox", id));
        }
    }
}
