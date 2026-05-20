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

use serde::{Deserialize, Serialize};

/// Configuration for the Kubernetes bootstrap plugin.
///
/// This struct is intentionally empty. The bootstrap provider derives all its
/// operational configuration from the associated `KubernetesSourceConfig`
/// (passed via `source_config_json`). This type exists to satisfy the plugin
/// descriptor contract and to serve as a placeholder for future bootstrap-specific
/// settings (e.g., concurrency limits, timeout overrides).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct KubernetesBootstrapConfig {}
