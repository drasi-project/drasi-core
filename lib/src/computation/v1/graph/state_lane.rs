// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The point-to-point snapshot lane belongs to its data binding. It is not
//! another desired relationship, subscriber, or durable delivery obligation.

use super::{
    ComputationGraph, EdgeDefinition, EdgeSnapshot, Endpoint, GraphResult, GraphSnapshot,
    NodeSnapshot,
};
use crate::computation::v1::{
    BootstrapInputEdge, BootstrapOutputEdge, BootstrapPorts, BoundedPipeConfig, ComponentId,
    ComponentRole, CurrentStateOptions, OutputEndpoint, PipeControl, PipeError,
    PipeMetricsSnapshot, PipeProvider, PortDirection, PortKind, Schema, StateTransfer,
};
use std::{num::NonZeroU64, sync::Arc};

pub(super) struct StateLane {
    pub output: Endpoint,
    pub input: Endpoint,
    pub reader: Option<CurrentStateOptions>,
    file: Option<(NonZeroU64, Arc<Schema>)>,
}

pub(super) fn has_cut_reader(graph: &ComputationGraph, source: &Endpoint) -> bool {
    graph
        .ids
        .get(&source.component)
        .and_then(|index| graph.components[*index].output_readers())
        .is_some_and(|readers| {
            readers.port(&source.port).is_some_and(|r| {
                r.snapshot_is_provided()
                    || r.projection()
                        .and_then(|(_, op)| op.upgrade())
                        .is_some_and(|op| op.cut_namespace().is_some())
            })
        })
}

impl StateLane {
    pub(super) fn has_seed(graph: &GraphSnapshot, consumer: &ComponentId) -> bool {
        let Some(node) = graph.nodes.iter().find(|n| n.descriptor.id() == consumer) else {
            return false;
        };
        let is_seed = |edge: &EdgeDefinition| {
            &edge.to.component == consumer
                && node
                    .descriptor
                    .ports()
                    .iter()
                    .any(|p| p.id() == &edge.to.port && p.kind() == PortKind::Bootstrap)
                && graph
                    .nodes
                    .iter()
                    .find(|n| n.descriptor.id() == &edge.from.component)
                    .is_none_or(|n| n.role == ComponentRole::BootstrapProvider)
        };
        graph.edges.iter().any(|e| is_seed(&e.definition))
            || graph
                .unbound_relationships
                .iter()
                .any(|e| e.policy.required_for_binding && is_seed(&e.definition))
    }

    /// Match an endpoint, not merely its component. Never substitute a
    /// snapshot of some other data output on a multi-output node.
    pub(super) fn for_edge(graph: &GraphSnapshot, edge: &EdgeSnapshot) -> Option<Self> {
        if edge.policy.initial_state == Some(false)
            || Self::has_seed(graph, &edge.definition.to.component)
        {
            return None;
        }
        let node = |id: &ComponentId| graph.nodes.iter().find(|n| n.descriptor.id() == id);
        let def = &edge.definition;
        let from = node(&def.from.component)?;
        let to = node(&def.to.component)?;
        let output = Self::lane_port(from, &def.from, PortDirection::Output)?;
        let input = Self::lane_port(to, &def.to, PortDirection::Input)?;
        Some(Self {
            output,
            input,
            reader: None,
            file: None,
        })
    }

    fn lane_port(
        node: &NodeSnapshot,
        endpoint: &Endpoint,
        direction: PortDirection,
    ) -> Option<Endpoint> {
        let state = node.descriptor.state_port_for(&endpoint.port)?;
        if state.direction() != direction {
            return None;
        }
        Some(Endpoint::new(
            endpoint.component.clone(),
            state.id().clone(),
        ))
    }

    pub(super) fn resolve(
        runtime: &ComputationGraph,
        edge: &EdgeSnapshot,
        producer: &drasi_computation_contracts::ComponentDescription,
        consumer: &drasi_computation_contracts::ComponentDescription,
    ) -> GraphResult<Option<Self>> {
        let graph = &runtime.snapshot;
        let requested = edge
            .policy
            .initial_state
            .or(consumer.upstream_initial_state);
        if consumer.requires_shared_input_cut && requested != Some(false) {
            let target = &runtime.nodes[runtime.ids[&edge.definition.to.component]];
            let participates =
                |def: &EdgeDefinition, policy: &crate::computation::v1::RelationshipPolicy| {
                    def.to.component == edge.definition.to.component
                        && policy.initial_state.or(consumer.upstream_initial_state) != Some(false)
                        && target.descriptor.state_port_for(&def.to.port).is_some()
                };
            if participates(&edge.definition, &edge.policy)
                && graph
                    .edges
                    .iter()
                    .map(|e| (&e.definition, &e.policy))
                    .chain(
                        graph
                            .unbound_relationships
                            .iter()
                            .map(|e| (&e.definition, &e.policy)),
                    )
                    .any(|(def, policy)| def != &edge.definition && participates(def, policy))
            {
                return Err(super::GraphError::Incompatible {
                    component: edge.definition.to.component.clone(),
                    phase: crate::computation::v1::FailurePhase::Binding,
                    context: format!(
                        "initial state for {}:{}",
                        edge.definition.to.component, edge.definition.to.port
                    ),
                    source: drasi_computation_contracts::Incompatibility::SharedInputCutUnavailable,
                });
            }
        }
        if requested == Some(false)
            || consumer.local_seed
            || graph.is_bootstrap_relationship(&edge.definition)
            || Self::has_seed(graph, &edge.definition.to.component)
        {
            return Ok(None);
        }
        let mut lane = Self::for_edge(graph, edge);
        let source = &edge.definition.from;
        let target = &edge.definition.to;
        let relayed_seed = lane.as_ref().is_some_and(|lane| {
            producer.bootstrap_output(&lane.output.port).completion
                == drasi_computation_contracts::BootstrapCompletionKind::Relayed
                && Self::has_seed(graph, &source.component)
        });
        if !relayed_seed && has_cut_reader(runtime, source) {
            if let Some(input) = Self::lane_port(
                &runtime.nodes[runtime.ids[&target.component]],
                target,
                PortDirection::Input,
            ) {
                lane = Some(Self {
                    output: source.clone(),
                    input,
                    reader: Some(CurrentStateOptions::default()),
                    file: None,
                });
            }
        }
        let missing_provider = lane.as_ref().is_some_and(|lane| {
            producer.bootstrap_output(&lane.output.port).completion
                == drasi_computation_contracts::BootstrapCompletionKind::Relayed
                && !Self::has_seed(graph, &lane.output.component)
        });
        if missing_provider || (requested == Some(true) && lane.is_none()) {
            return Err(super::GraphError::Incompatible {
                component: edge.definition.to.component.clone(),
                phase: crate::computation::v1::FailurePhase::Binding,
                context: format!(
                    "initial state for {}:{} from {}:{}",
                    edge.definition.to.component,
                    edge.definition.to.port,
                    edge.definition.from.component,
                    edge.definition.from.port
                ),
                source: drasi_computation_contracts::Incompatibility::InitialStateUnavailable,
            });
        }
        if let (Some(lane), StateTransfer::TemporaryFile { max_file_bytes }) =
            (&mut lane, edge.policy.state_transfer)
        {
            let (_, port) = super::resolve(&runtime.ids, &runtime.nodes, source)?;
            let factory = graph
                .specifications
                .get(&source.component)
                .and_then(|spec| runtime.factories.get(&spec.implementation))
                .map(Arc::as_ref);
            let schema =
                crate::computation::v1::output_catalogue::transfer_schema(port.schema(), factory)
                    .ok_or_else(|| super::GraphError::Incompatible {
                    component: target.component.clone(),
                    phase: crate::computation::v1::FailurePhase::Binding,
                    context: format!(
                        "no executable schema for file transfer from {}:{}",
                        source.component, source.port
                    ),
                    source: drasi_computation_contracts::Incompatibility::StateTransferUnavailable,
                })?;
            lane.file = Some((max_file_bytes, schema));
        }
        Ok(lane)
    }

    pub(super) fn bind(
        self,
        index: usize,
        from: &BootstrapPorts,
        to: &BootstrapPorts,
        live: Arc<dyn PipeControl>,
    ) -> GraphResult<Arc<dyn PipeControl>> {
        let error = |source| super::GraphError::Pipe {
            edge: index,
            source,
        };
        let mut lane = match self.file {
            Some((quota, schema)) => {
                crate::computation::v1::state_transfer::file_lane(64, quota, schema)
            }
            None => BoundedPipeConfig { capacity: 64 }.create(),
        }
        .map_err(error)?;
        let receiver = lane.pipe.take_receiver().map_err(error)?;
        from.bind_session_pipe(
            to,
            index,
            BootstrapOutputEdge {
                output_port: self.output.port.clone(),
                consumer: self.input.component,
                input_port: self.input.port.clone(),
            },
            BootstrapInputEdge {
                producer: OutputEndpoint::new(self.output.component, self.output.port),
                input_port: self.input.port,
                local: false,
            },
            (lane.pipe.sender(), receiver),
            self.reader,
        );
        Ok(Arc::new(ConnectionControl {
            live,
            lane: lane.control,
        }))
    }
}

struct ConnectionControl {
    live: Arc<dyn PipeControl>,
    lane: Arc<dyn PipeControl>,
}

#[async_trait::async_trait]
impl PipeControl for ConnectionControl {
    fn close(&self) {
        self.live.close();
        self.lane.close();
    }
    fn cancel(&self) {
        self.live.cancel();
        self.lane.cancel();
    }
    async fn is_idle(&self) -> Result<bool, PipeError> {
        Ok(self.live.is_idle().await? && self.lane.is_idle().await?)
    }
    fn metrics(&self) -> Option<PipeMetricsSnapshot> {
        self.live.metrics()
    }
    fn observations(&self) -> Option<Arc<crate::observability::QueueObservations>> {
        self.live.observations()
    }
}
