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

//! Pure per-port format selection.
//!
//! A factory declares each port's default schema plus any alternative schemas
//! it genuinely implements. Configuration may select one alternative per port:
//!
//! ```json
//! { "ports": { "out": { "format": "vendor.columnar-batch-v1" } } }
//! ```
//!
//! Resolution only chooses among declared schemas. It never invents a codec,
//! converts between formats, or falls back to the default for an unknown name.
//! It performs no I/O and is safe to call before construction.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::{
    ComponentDescriptor, ComponentId, ContractError, PortDescriptor, PortId, Result,
    SchemaDescriptor,
};

/// Reserved configuration field holding the per-port format overrides.
pub const PORT_FORMATS_KEY: &str = "ports";

/// Canonical configuration name of a schema: `<schema id>-v<version>`.
pub fn port_format_name(schema: &SchemaDescriptor) -> String {
    format!("{}-v{}", schema.id(), schema.version().value())
}

/// A non-default schema a factory implements for one of its declared ports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortFormatAlternative {
    pub port: PortId,
    pub schema: SchemaDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CataloguePort {
    default: PortDescriptor,
    /// The default schema first, then alternatives in declaration order.
    formats: Vec<SchemaDescriptor>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PortOverride {
    format: String,
}

/// The ports a factory can describe, with the formats each actually supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortFormatCatalogue {
    ports: Vec<CataloguePort>,
}

impl PortFormatCatalogue {
    /// Declare default ports and the alternative schemas the factory implements.
    /// Alternatives must name a declared port, and every format name on a port
    /// (including the default's) must be unique.
    pub fn try_new(
        defaults: impl IntoIterator<Item = PortDescriptor>,
        alternatives: impl IntoIterator<Item = PortFormatAlternative>,
    ) -> Result<Self> {
        let mut ports: Vec<CataloguePort> = Vec::new();
        for default in defaults {
            if ports.iter().any(|port| port.default.id() == default.id()) {
                return Err(ContractError::DuplicatePort {
                    port: default.id().clone(),
                });
            }
            ports.push(CataloguePort {
                formats: vec![default.schema().clone()],
                default,
            });
        }
        for alternative in alternatives {
            let port = ports
                .iter_mut()
                .find(|port| port.default.id() == &alternative.port)
                .ok_or_else(|| ContractError::InvalidPortFormats {
                    reason: format!(
                        "alternative format {} is declared for undeclared port {}",
                        port_format_name(&alternative.schema),
                        alternative.port
                    ),
                })?;
            port.formats.push(alternative.schema);
        }
        for port in &ports {
            let mut names = BTreeSet::new();
            for schema in &port.formats {
                let name = port_format_name(schema);
                if !names.insert(name.clone()) {
                    return Err(ContractError::InvalidPortFormats {
                        reason: format!(
                            "port {} declares format {name} more than once",
                            port.default.id()
                        ),
                    });
                }
            }
        }
        Ok(Self { ports })
    }

    /// A catalogue with no alternatives: only explicit selection of each
    /// port's own format is accepted.
    pub fn fixed(defaults: impl IntoIterator<Item = PortDescriptor>) -> Result<Self> {
        Self::try_new(defaults, [])
    }

    /// Ports with their default formats, in declaration order.
    pub fn defaults(&self) -> Vec<PortDescriptor> {
        self.ports.iter().map(|port| port.default.clone()).collect()
    }

    /// All supported schemas for a port, default first.
    pub fn formats(&self, port: &PortId) -> Option<&[SchemaDescriptor]> {
        self.ports
            .iter()
            .find(|candidate| candidate.default.id() == port)
            .map(|candidate| candidate.formats.as_slice())
    }

    /// Every supported non-default schema, suitable for factory metadata.
    pub fn alternatives(&self) -> Vec<PortFormatAlternative> {
        self.ports
            .iter()
            .flat_map(|port| {
                port.formats[1..]
                    .iter()
                    .map(move |schema| PortFormatAlternative {
                        port: port.default.id().clone(),
                        schema: schema.clone(),
                    })
            })
            .collect()
    }

    /// Resolve the ports selected by the optional `ports` configuration value.
    /// Unmentioned ports keep their defaults and all declared ports, in
    /// declaration order, are returned with their direction and requirements.
    pub fn resolve(&self, overrides: Option<&Value>) -> Result<Vec<PortDescriptor>> {
        let overrides = parse_overrides(overrides)?;
        for port in overrides.keys() {
            if !self.ports.iter().any(|known| known.default.id() == port) {
                return Err(ContractError::UnknownFormatPort {
                    port: port.clone(),
                    known: self
                        .ports
                        .iter()
                        .map(|known| known.default.id().clone())
                        .collect(),
                });
            }
        }
        self.ports
            .iter()
            .map(|port| {
                let Some(requested) = overrides.get(port.default.id()) else {
                    return Ok(port.default.clone());
                };
                let schema = port
                    .formats
                    .iter()
                    .find(|schema| port_format_name(schema) == *requested)
                    .ok_or_else(|| ContractError::UnsupportedPortFormat {
                        port: port.default.id().clone(),
                        requested: requested.clone(),
                        supported: port.formats.iter().map(port_format_name).collect(),
                    })?;
                Ok(PortDescriptor::new(
                    port.default.id().clone(),
                    port.default.direction(),
                    schema.clone(),
                    port.default.requirements().clone(),
                ))
            })
            .collect()
    }

    /// Describe a component whose ports follow the optional `ports` value.
    pub fn describe(
        &self,
        id: ComponentId,
        overrides: Option<&Value>,
    ) -> Result<ComponentDescriptor> {
        ComponentDescriptor::try_new(id, self.resolve(overrides)?)
    }

    /// Require declared ports to be exactly those the configuration resolves
    /// to, independent of order. Rejects unsupported selections, duplicate,
    /// missing or extra ports, and any difference in format, direction or
    /// requirements.
    pub fn validate_ports(
        &self,
        actual: &[PortDescriptor],
        overrides: Option<&Value>,
    ) -> Result<()> {
        let expected = self.resolve(overrides)?;
        for (index, port) in actual.iter().enumerate() {
            if actual[..index]
                .iter()
                .any(|earlier| earlier.id() == port.id())
            {
                return Err(ContractError::DuplicatePort {
                    port: port.id().clone(),
                });
            }
        }
        for port in actual {
            if !expected.iter().any(|candidate| candidate.id() == port.id()) {
                return Err(ContractError::UnknownFormatPort {
                    port: port.id().clone(),
                    known: expected.iter().map(|known| known.id().clone()).collect(),
                });
            }
        }
        for port in &expected {
            let found = actual.iter().find(|candidate| candidate.id() == port.id());
            if found != Some(port) {
                return Err(ContractError::PortFormatMismatch {
                    port: port.id().clone(),
                    expected: summarize(Some(port)),
                    actual: summarize(found),
                });
            }
        }
        Ok(())
    }
}

fn summarize(port: Option<&PortDescriptor>) -> String {
    match port {
        None => "no declaration".to_owned(),
        Some(port) => format!(
            "{:?} {} (fingerprint {}, requirements {:?})",
            port.direction(),
            port_format_name(port.schema()),
            port.schema().fingerprint().value(),
            port.requirements().required()
        ),
    }
}

fn parse_overrides(value: Option<&Value>) -> Result<BTreeMap<PortId, String>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let invalid = |reason: String| ContractError::InvalidPortFormatOverride { reason };
    let entries: BTreeMap<String, PortOverride> = serde_json::from_value(value.clone())
        .map_err(|error| invalid(format!("expected {{<port>: {{format: <name>}}}}: {error}")))?;
    entries
        .into_iter()
        .map(|(port, selection)| {
            let port = PortId::try_new(port)?;
            Ok((port, selection.format))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use serde_json::json;

    use super::*;
    use crate::{PipeCapability, PipeRequirements, PortDirection, SchemaId, SchemaVersion};

    fn schema(id: &str, version: u32) -> SchemaDescriptor {
        SchemaDescriptor::try_new(
            SchemaId::try_new(id).unwrap(),
            SchemaVersion::try_new(version).unwrap(),
            "test-encoding",
            Bytes::from_static(b"definition"),
        )
        .unwrap()
    }

    fn port(id: &str, direction: PortDirection, schema: SchemaDescriptor) -> PortDescriptor {
        PortDescriptor::new(
            PortId::try_new(id).unwrap(),
            direction,
            schema,
            PipeRequirements::new([PipeCapability::Backpressure]),
        )
    }

    fn id(value: &str) -> PortId {
        PortId::try_new(value).unwrap()
    }

    fn catalogue() -> PortFormatCatalogue {
        PortFormatCatalogue::try_new(
            [
                port("in", PortDirection::Input, schema("graph", 1)),
                port("out", PortDirection::Output, schema("graph", 1)),
                port("audit", PortDirection::Output, schema("audit", 1)),
            ],
            [
                PortFormatAlternative {
                    port: id("out"),
                    schema: schema("vendor.batch", 1),
                },
                PortFormatAlternative {
                    port: id("in"),
                    schema: schema("vendor.batch", 1),
                },
            ],
        )
        .unwrap()
    }

    #[test]
    fn names_follow_schema_id_and_version() {
        assert_eq!(
            port_format_name(&schema("vendor.batch", 3)),
            "vendor.batch-v3"
        );
    }

    #[test]
    fn no_override_returns_defaults_in_order() {
        let catalogue = catalogue();
        assert_eq!(catalogue.resolve(None).unwrap(), catalogue.defaults());
        assert_eq!(
            catalogue.resolve(Some(&json!({}))).unwrap(),
            catalogue.defaults()
        );
    }

    #[test]
    fn selects_only_the_named_port_and_preserves_the_rest() {
        let catalogue = catalogue();
        let resolved = catalogue
            .resolve(Some(&json!({"out": {"format": "vendor.batch-v1"}})))
            .unwrap();
        let defaults = catalogue.defaults();
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0], defaults[0]);
        assert_eq!(resolved[2], defaults[2]);
        assert_eq!(resolved[1].id(), &id("out"));
        assert_eq!(resolved[1].direction(), PortDirection::Output);
        assert_eq!(resolved[1].schema(), &schema("vendor.batch", 1));
        assert_eq!(resolved[1].requirements(), defaults[1].requirements());
    }

    #[test]
    fn explicitly_selecting_the_default_is_accepted() {
        let catalogue = catalogue();
        let resolved = catalogue
            .resolve(Some(&json!({"in": {"format": "graph-v1"}})))
            .unwrap();
        assert_eq!(resolved, catalogue.defaults());
    }

    #[test]
    fn unknown_format_is_rejected_and_never_defaulted() {
        let error = catalogue()
            .resolve(Some(&json!({"out": {"format": "vendor.unknown-v1"}})))
            .unwrap_err();
        assert!(matches!(
            error,
            ContractError::UnsupportedPortFormat { ref requested, ref supported, .. }
                if requested == "vendor.unknown-v1"
                    && supported == &["graph-v1".to_owned(), "vendor.batch-v1".to_owned()]
        ));
        // A format supported by a different port is not supported here.
        assert!(matches!(
            catalogue().resolve(Some(&json!({"audit": {"format": "vendor.batch-v1"}}))),
            Err(ContractError::UnsupportedPortFormat { .. })
        ));
    }

    #[test]
    fn unknown_port_is_rejected() {
        assert!(matches!(
            catalogue().resolve(Some(&json!({"missing": {"format": "graph-v1"}}))),
            Err(ContractError::UnknownFormatPort { .. })
        ));
    }

    #[test]
    fn malformed_overrides_are_rejected() {
        for value in [
            json!(null),
            json!([]),
            json!("out"),
            json!({"out": "graph-v1"}),
            json!({"out": {}}),
            json!({"out": {"format": 1}}),
            json!({"out": {"format": "graph-v1", "other": true}}),
            json!({"": {"format": "graph-v1"}}),
        ] {
            assert!(catalogue().resolve(Some(&value)).is_err(), "{value}");
        }
    }

    #[test]
    fn catalogue_rejects_inconsistent_declarations() {
        let default = || port("out", PortDirection::Output, schema("graph", 1));
        assert!(matches!(
            PortFormatCatalogue::try_new(
                [default()],
                [PortFormatAlternative {
                    port: id("elsewhere"),
                    schema: schema("vendor.batch", 1),
                }]
            ),
            Err(ContractError::InvalidPortFormats { .. })
        ));
        assert!(matches!(
            PortFormatCatalogue::try_new(
                [default()],
                [PortFormatAlternative {
                    port: id("out"),
                    schema: schema("graph", 1),
                }]
            ),
            Err(ContractError::InvalidPortFormats { .. })
        ));
        assert!(matches!(
            PortFormatCatalogue::try_new([default(), default()], []),
            Err(ContractError::DuplicatePort { .. })
        ));
    }

    #[test]
    fn validate_ports_accepts_resolved_ports_in_any_order() {
        let catalogue = catalogue();
        let overrides = json!({"out": {"format": "vendor.batch-v1"}});
        let mut resolved = catalogue.resolve(Some(&overrides)).unwrap();
        resolved.reverse();
        catalogue
            .validate_ports(&resolved, Some(&overrides))
            .unwrap();
    }

    #[test]
    fn validate_ports_rejects_mismatch_missing_and_extra_ports() {
        let catalogue = catalogue();
        let overrides = json!({"out": {"format": "vendor.batch-v1"}});
        // Default ports do not match an override selecting another format.
        assert!(matches!(
            catalogue.validate_ports(&catalogue.defaults(), Some(&overrides)),
            Err(ContractError::PortFormatMismatch { .. })
        ));
        let mut missing = catalogue.resolve(Some(&overrides)).unwrap();
        missing.pop();
        assert!(matches!(
            catalogue.validate_ports(&missing, Some(&overrides)),
            Err(ContractError::PortFormatMismatch { .. })
        ));
        let mut extra = catalogue.resolve(Some(&overrides)).unwrap();
        extra.push(port("extra", PortDirection::Output, schema("graph", 1)));
        assert!(matches!(
            catalogue.validate_ports(&extra, Some(&overrides)),
            Err(ContractError::UnknownFormatPort { .. })
        ));
        // Same format name but a different definition is not accepted.
        let mut changed = catalogue.defaults();
        changed[0] = PortDescriptor::new(
            id("in"),
            PortDirection::Input,
            SchemaDescriptor::try_new(
                SchemaId::try_new("graph").unwrap(),
                SchemaVersion::try_new(1).unwrap(),
                "test-encoding",
                Bytes::from_static(b"other-definition"),
            )
            .unwrap(),
            changed[0].requirements().clone(),
        );
        assert!(matches!(
            catalogue.validate_ports(&changed, None),
            Err(ContractError::PortFormatMismatch { .. })
        ));
    }

    #[test]
    fn validate_ports_rejects_duplicate_actual_ports() {
        let catalogue = catalogue();
        let mut duplicated = catalogue.defaults();
        duplicated.push(duplicated[1].clone());
        assert!(matches!(
            catalogue.validate_ports(&duplicated, None),
            Err(ContractError::DuplicatePort { ref port }) if port == &id("out")
        ));
        // Even when the duplicates are identical to the expected declaration.
        let single =
            PortFormatCatalogue::fixed([port("out", PortDirection::Output, schema("graph", 1))])
                .unwrap();
        let mut twice = single.defaults();
        twice.push(twice[0].clone());
        assert!(matches!(
            single.validate_ports(&twice, None),
            Err(ContractError::DuplicatePort { .. })
        ));
    }

    #[test]
    fn alternatives_round_trip_through_metadata_form() {
        let catalogue = catalogue();
        let rebuilt =
            PortFormatCatalogue::try_new(catalogue.defaults(), catalogue.alternatives()).unwrap();
        assert_eq!(rebuilt, catalogue);
        assert_eq!(catalogue.alternatives().len(), 2);
    }
}
