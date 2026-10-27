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

//! Pure endpoint-port resolution for shorthand connections.
//!
//! A shorthand names a component without naming a port. The port is chosen
//! only from actual declarations: an explicit port, the factory's validated
//! primary port for that direction, or the single port of that direction.
//! Several candidates with no primary designation are ambiguous and never
//! resolved by position or name.

use super::{ContractError, PortDescriptor, PortDirection, PortId, Result};

/// A factory's optional designation of one primary input and one primary
/// output. Designations name ports by id, so they survive format overrides, and
/// are checked against the actual port declarations by [`Self::validate`].
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortPrimaries {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input: Option<PortId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<PortId>,
}

impl PortPrimaries {
    pub fn new(input: Option<PortId>, output: Option<PortId>) -> Self {
        Self { input, output }
    }

    pub fn is_empty(&self) -> bool {
        self.input.is_none() && self.output.is_none()
    }

    /// The designated primary port for a direction, if any.
    pub fn primary(&self, direction: PortDirection) -> Option<&PortId> {
        match direction {
            PortDirection::Input => self.input.as_ref(),
            PortDirection::Output => self.output.as_ref(),
        }
    }

    /// Require each designation to name an existing port of its own direction.
    pub fn validate(&self, ports: &[PortDescriptor]) -> Result<()> {
        for direction in [PortDirection::Input, PortDirection::Output] {
            if let Some(primary) = self.primary(direction) {
                require_port(ports, direction, primary)?;
            }
        }
        Ok(())
    }

    /// Resolve the port a shorthand endpoint refers to.
    ///
    /// An explicit port must exist with the requested direction. Without one,
    /// the validated primary for that direction is used, otherwise the only port
    /// of that direction. No port, or several without a primary, is an error.
    pub fn resolve(
        &self,
        ports: &[PortDescriptor],
        direction: PortDirection,
        explicit: Option<&PortId>,
    ) -> Result<PortId> {
        self.validate(ports)?;
        if let Some(port) = explicit {
            return require_port(ports, direction, port);
        }
        if let Some(primary) = self.primary(direction) {
            return Ok(primary.clone());
        }
        let mut candidates = ports_of(ports, direction);
        match (candidates.next(), candidates.next()) {
            (None, _) => Err(ContractError::NoEndpointPort { direction }),
            (Some(only), None) => Ok(only.clone()),
            (Some(first), Some(second)) => Err(ContractError::AmbiguousEndpointPort {
                direction,
                candidates: [first, second]
                    .into_iter()
                    .chain(candidates)
                    .cloned()
                    .collect(),
            }),
        }
    }
}

fn ports_of(ports: &[PortDescriptor], direction: PortDirection) -> impl Iterator<Item = &PortId> {
    ports
        .iter()
        .filter(move |port| port.direction() == direction)
        .map(PortDescriptor::id)
}

fn require_port(
    ports: &[PortDescriptor],
    direction: PortDirection,
    port: &PortId,
) -> Result<PortId> {
    if ports_of(ports, direction).any(|candidate| candidate == port) {
        return Ok(port.clone());
    }
    Err(ContractError::EndpointPortNotFound {
        port: port.clone(),
        direction,
        known: ports_of(ports, direction).cloned().collect(),
    })
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::{PipeRequirements, SchemaDescriptor, SchemaId, SchemaVersion};

    fn schema(id: &str) -> SchemaDescriptor {
        SchemaDescriptor::try_new(
            SchemaId::try_new(id).unwrap(),
            SchemaVersion::try_new(1).unwrap(),
            "test-encoding",
            Bytes::from_static(b"definition"),
        )
        .unwrap()
    }

    fn id(value: &str) -> PortId {
        PortId::try_new(value).unwrap()
    }

    fn port(name: &str, direction: PortDirection, schema_id: &str) -> PortDescriptor {
        PortDescriptor::new(
            id(name),
            direction,
            schema(schema_id),
            PipeRequirements::default(),
        )
    }

    fn pair() -> Vec<PortDescriptor> {
        vec![
            port("in", PortDirection::Input, "graph"),
            port("out", PortDirection::Output, "graph"),
        ]
    }

    fn multiple() -> Vec<PortDescriptor> {
        vec![
            port("left", PortDirection::Input, "graph"),
            port("right", PortDirection::Input, "graph"),
            port("main", PortDirection::Output, "graph"),
            port("audit", PortDirection::Output, "graph"),
        ]
    }

    #[test]
    fn a_single_port_of_a_direction_resolves_implicitly() {
        let none = PortPrimaries::default();
        assert_eq!(
            none.resolve(&pair(), PortDirection::Input, None).unwrap(),
            id("in")
        );
        assert_eq!(
            none.resolve(&pair(), PortDirection::Output, None).unwrap(),
            id("out")
        );
        // The names are not special: the only port of a direction is chosen.
        let renamed = vec![port("result", PortDirection::Output, "graph")];
        assert_eq!(
            none.resolve(&renamed, PortDirection::Output, None).unwrap(),
            id("result")
        );
    }

    #[test]
    fn several_ports_without_a_primary_are_ambiguous_not_guessed() {
        let none = PortPrimaries::default();
        for (direction, expected) in [
            (PortDirection::Input, vec![id("left"), id("right")]),
            (PortDirection::Output, vec![id("main"), id("audit")]),
        ] {
            assert!(matches!(
                none.resolve(&multiple(), direction, None),
                Err(ContractError::AmbiguousEndpointPort { candidates, .. })
                    if candidates == expected
            ));
        }
        // A port literally named `out` is still not chosen by heuristic.
        let ports = vec![
            port("audit", PortDirection::Output, "graph"),
            port("out", PortDirection::Output, "graph"),
        ];
        assert!(matches!(
            none.resolve(&ports, PortDirection::Output, None),
            Err(ContractError::AmbiguousEndpointPort { .. })
        ));
    }

    #[test]
    fn a_missing_direction_is_an_error() {
        let source = vec![port("out", PortDirection::Output, "graph")];
        assert!(matches!(
            PortPrimaries::default().resolve(&source, PortDirection::Input, None),
            Err(ContractError::NoEndpointPort {
                direction: PortDirection::Input
            })
        ));
    }

    #[test]
    fn a_designated_primary_resolves_among_several_ports() {
        let primaries = PortPrimaries::new(Some(id("right")), Some(id("main")));
        assert_eq!(
            primaries
                .resolve(&multiple(), PortDirection::Input, None)
                .unwrap(),
            id("right")
        );
        assert_eq!(
            primaries
                .resolve(&multiple(), PortDirection::Output, None)
                .unwrap(),
            id("main")
        );
    }

    #[test]
    fn an_explicit_port_overrides_the_primary_but_must_exist_with_its_direction() {
        let primaries = PortPrimaries::new(None, Some(id("main")));
        assert_eq!(
            primaries
                .resolve(&multiple(), PortDirection::Output, Some(&id("audit")))
                .unwrap(),
            id("audit")
        );
        for (direction, port) in [
            (PortDirection::Output, "missing"),
            (PortDirection::Output, "left"),
            (PortDirection::Input, "main"),
        ] {
            assert!(
                matches!(
                    primaries.resolve(&multiple(), direction, Some(&id(port))),
                    Err(ContractError::EndpointPortNotFound { .. })
                ),
                "{direction:?} {port}"
            );
        }
    }

    #[test]
    fn invalid_designations_are_rejected_even_for_a_single_port() {
        for primaries in [
            PortPrimaries::new(Some(id("missing")), None),
            PortPrimaries::new(Some(id("out")), None),
            PortPrimaries::new(None, Some(id("in"))),
            PortPrimaries::new(None, Some(id("missing"))),
        ] {
            assert!(
                matches!(
                    primaries.validate(&pair()),
                    Err(ContractError::EndpointPortNotFound { .. })
                ),
                "{primaries:?}"
            );
            assert!(primaries
                .resolve(&pair(), PortDirection::Output, None)
                .is_err());
        }
        PortPrimaries::new(Some(id("in")), Some(id("out")))
            .validate(&pair())
            .unwrap();
        PortPrimaries::default().validate(&pair()).unwrap();
    }

    #[test]
    fn the_primary_survives_a_format_override() {
        let primaries = PortPrimaries::new(None, Some(id("main")));
        let mut overridden = multiple();
        overridden[2] = port("main", PortDirection::Output, "vendor.batch");
        assert_eq!(
            primaries
                .resolve(&overridden, PortDirection::Output, None)
                .unwrap(),
            id("main")
        );
    }

    #[test]
    fn serialization_omits_absent_designations_and_rejects_unknown_fields() {
        assert_eq!(
            serde_json::to_value(PortPrimaries::default()).unwrap(),
            serde_json::json!({})
        );
        let value = serde_json::json!({"output": "main"});
        let parsed: PortPrimaries = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(parsed, PortPrimaries::new(None, Some(id("main"))));
        assert_eq!(serde_json::to_value(&parsed).unwrap(), value);
        assert!(
            serde_json::from_value::<PortPrimaries>(serde_json::json!({"first": "in"})).is_err()
        );
    }
}
