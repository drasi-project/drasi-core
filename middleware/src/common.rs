// Copyright 2025 The Drasi Authors.
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

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ErrorHandling {
    #[default]
    Fail,
    Skip,
}

/// Settles a derived property against an update's removal set before deriving it.
///
/// Returns true when the update removes `target`: nothing can be derived, and the
/// derived `output` is removed as well unless the update sets it.
#[cfg(any(feature = "decoder", feature = "parse_json"))]
pub(crate) fn derive_removes_output(
    element: &drasi_core_types::models::Element,
    removed: &mut std::collections::BTreeSet<String>,
    target: &str,
    output: &str,
) -> bool {
    if !removed.contains(target) {
        return false;
    }
    if output != target && !element.get_properties().contains_key(output) {
        removed.insert(output.to_string());
    }
    true
}

/// A derived property that the update now sets can no longer also be removed.
#[cfg(any(feature = "decoder", feature = "parse_json"))]
pub(crate) fn derived_property_is_set(
    element: &drasi_core_types::models::Element,
    removed: &mut std::collections::BTreeSet<String>,
    output: &str,
) {
    if element.get_properties().contains_key(output) {
        removed.remove(output);
    }
}
