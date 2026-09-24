// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::*;

/// Configuration captured while the component is exclusively owned at a
/// construction/reconfiguration boundary, never by interrupting data processing.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "camelCase", deny_unknown_fields)]
pub enum CapturedComponentConfiguration {
    Declared {
        values: BTreeMap<Arc<str>, ConfigurationValue>,
    },
    Available {
        values: serde_json::Value,
    },
    Unavailable {
        reason: String,
    },
}

/// Privileged configuration export. Unlike public topology-as-data inspection,
/// this may contain sensitive properties returned by component implementations.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphConfigurationSnapshot {
    pub topology: DesiredTopology,
    pub configurations: BTreeMap<ComponentId, CapturedComponentConfiguration>,
}

impl Component {
    pub(super) fn capture_configuration(&self) -> CapturedComponentConfiguration {
        let result = match self {
            Self::Source(component) => component.configuration(),
            Self::Transformer(component) | Self::Query(component) => component.configuration(),
            Self::Sink(component) => component.configuration(),
            Self::Service(component) => component.configuration(),
            Self::Deferred { specification, .. } => {
                return CapturedComponentConfiguration::Declared {
                    values: specification.configuration.clone(),
                };
            }
            Self::Unresolved(definition) => match &definition.construction {
                ComponentConstruction::Factory(specification) => {
                    return CapturedComponentConfiguration::Declared {
                        values: specification.configuration.clone(),
                    };
                }
                ComponentConstruction::External { binding } => {
                    return CapturedComponentConfiguration::Unavailable {
                        reason: format!("external component binding {binding} is not supplied"),
                    };
                }
            },
        };
        match result {
            Ok(values) => CapturedComponentConfiguration::Available { values },
            Err(error) => CapturedComponentConfiguration::Unavailable {
                reason: format!("{error:#}"),
            },
        }
    }
}
