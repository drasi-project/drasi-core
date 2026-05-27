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

//! Shared utility functions for Azure Storage service clients.

/// Normalize a custom endpoint URI for Azure Storage emulators.
///
/// Ensures the URI ends with `/{account_name}` as required by the SDK's
/// `CloudLocation::Custom` variant.
pub(crate) fn normalize_custom_uri(account_name: &str, uri: &str) -> String {
    let trimmed = uri.trim_end_matches('/');
    if trimmed.ends_with(account_name) {
        trimmed.to_string()
    } else {
        format!("{trimmed}/{account_name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_already_ends_with_account_name() {
        let result = normalize_custom_uri(
            "devstoreaccount1",
            "http://127.0.0.1:10000/devstoreaccount1",
        );
        assert_eq!(result, "http://127.0.0.1:10000/devstoreaccount1");
    }

    #[test]
    fn uri_without_account_name_suffix() {
        let result = normalize_custom_uri("devstoreaccount1", "http://127.0.0.1:10000");
        assert_eq!(result, "http://127.0.0.1:10000/devstoreaccount1");
    }

    #[test]
    fn uri_with_trailing_slash_no_account_name() {
        let result = normalize_custom_uri("devstoreaccount1", "http://127.0.0.1:10000/");
        assert_eq!(result, "http://127.0.0.1:10000/devstoreaccount1");
    }

    #[test]
    fn uri_with_trailing_slash_and_account_name() {
        let result = normalize_custom_uri(
            "devstoreaccount1",
            "http://127.0.0.1:10000/devstoreaccount1/",
        );
        assert_eq!(result, "http://127.0.0.1:10000/devstoreaccount1");
    }
}
