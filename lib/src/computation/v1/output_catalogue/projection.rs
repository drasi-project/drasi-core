// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::collections::BTreeSet;

struct Stage {
    output: OutputHandle,
    input: EdgeDefinition,
    binding_generation: u64,
    projector: std::sync::Weak<dyn StateProjector>,
}

pub(super) struct ProjectionRoute {
    source: OutputHandle,
    stages: Vec<Stage>,
    revision: GraphRevision,
}

impl ProjectionRoute {
    pub(super) fn resolve(
        catalogue: &OutputCatalogue,
        graph: &GraphRegistrySnapshot,
        mut output: Resolved,
    ) -> ReadResult<(Provision, Option<Arc<Self>>)> {
        let mut stages = Vec::new();
        let mut seen = BTreeSet::new();
        loop {
            let endpoint = output.description.endpoint.clone();
            if !seen.insert(endpoint.clone()) {
                return Err(OutputReadError::invalid(format!(
                    "cyclic state projection at {endpoint}"
                )));
            }
            let Some((input, projector)) = output
                .readers
                .as_ref()
                .and_then(WeakOutputPortReaders::projection)
            else {
                let provision = output.description.current_state;
                if provision != Provision::Provided {
                    return Ok((provision, None));
                }
                stages.reverse();
                return Ok((
                    provision,
                    Some(Arc::new(Self {
                        source: catalogue.handle(endpoint, output)?,
                        stages,
                        revision: graph.desired.revision,
                    })),
                ));
            };
            let target = Endpoint::new(endpoint.component.clone(), input.clone());
            if graph
                .desired
                .unbound_relationships
                .iter()
                .any(|e| e.definition.to == target)
            {
                return Ok((Provision::NotConstructed, None));
            }
            let mut edges = graph
                .observed
                .relationships
                .iter()
                .filter(|(def, _)| def.to == target);
            let Some((edge, binding)) = edges.next() else {
                return Ok((Provision::Absent, None));
            };
            // A single-input projector cannot invent one cut for several producers.
            if edges.next().is_some() {
                return Ok((Provision::Absent, None));
            }
            if let Some(failure) = &binding.failure {
                return Err(OutputReadError::Failed(anyhow::Error::new(
                    failure.cause.clone(),
                )));
            }
            if binding.binding != BindingState::Bound {
                return Ok((Provision::NotConstructed, None));
            }
            let projector = projector.clone();
            stages.push(Stage {
                output: catalogue.handle(endpoint, output)?,
                input: edge.clone(),
                binding_generation: binding.generation,
                projector,
            });
            output = OutputCatalogue::resolve_direct(
                graph,
                &OutputEndpoint::new(edge.from.component.clone(), edge.from.port.clone()),
            )?;
        }
    }

    pub(super) fn ensure_current(&self, graph: &GraphRegistrySnapshot) -> ReadResult<()> {
        self.source.ensure_local_current(graph)?;
        for stage in &self.stages {
            stage.output.ensure_local_current(graph)?;
            if !graph
                .observed
                .relationships
                .get(&stage.input)
                .is_some_and(|binding| {
                    binding.binding == BindingState::Bound
                        && binding.generation == stage.binding_generation
                })
                || (graph.desired.revision != self.revision
                    && (graph
                        .desired
                        .unbound_relationships
                        .iter()
                        .any(|e| e.definition.to == stage.input.to)
                        || graph
                            .observed
                            .relationships
                            .keys()
                            .any(|edge| edge.to == stage.input.to && edge != &stage.input)))
            {
                return Err(OutputReadError::Retired);
            }
        }
        Ok(())
    }

    pub(super) async fn capture(&self, limits: SnapshotBatchLimits) -> ReadResult<CapturedView> {
        // An empty upstream has no page on which a native projector could
        // check activation. Establish readiness before touching the source.
        {
            let graph = self.source.catalogue.registry_snapshot();
            for stage in &self.stages {
                let observed = graph
                    .observed
                    .components
                    .get(&stage.output.endpoint.component)
                    .ok_or(OutputReadError::Retired)?;
                if let Some(failure) = &observed.failure {
                    return Err(OutputReadError::Failed(anyhow::Error::new(
                        failure.cause.clone(),
                    )));
                }
                if !matches!(
                    observed.lifecycle,
                    ComponentLifecycle::Running | ComponentLifecycle::Quiesced
                ) {
                    return Err(OutputReadError::NotReady(format!(
                        "state projector {} is {:?}",
                        stage.output.endpoint, observed.lifecycle
                    )));
                }
            }
        }
        let source = self
            .source
            .capture_direct(SnapshotBatchLimits::default())
            .await?;
        let mut pages = source.batches;
        let mut cursor = source.cursor;
        let mut stages = self.stages.iter().peekable();
        while let Some(stage) = stages.next() {
            let op = stage.projector.upgrade().ok_or(OutputReadError::Retired)?;
            cursor = cursor
                .as_ref()
                .zip(op.cut_namespace())
                .map(|(input, owner)| input.projected_by(owner))
                .transpose()?;
            if let Some(cut) = &cursor {
                stage.output.check_cursor(cut)?;
            }
            let projected = pages
                .then(move |batch| {
                    let op = op.clone();
                    async move { op.project(batch?).await }
                })
                .boxed();
            let output = &stage.output;
            let final_stage = stages.peek().is_none();
            let view = CapturedView::new(output.endpoint.to_string(), cursor.clone(), |stream| {
                state_transfer::scan_batches(
                    projected,
                    stream,
                    output.schema.clone(),
                    if final_stage {
                        limits
                    } else {
                        SnapshotBatchLimits::default()
                    },
                )
            })?;
            if final_stage {
                return Ok(view);
            }
            pages = view.batches;
        }
        Err(OutputReadError::invalid("empty state projection route"))
    }
}
