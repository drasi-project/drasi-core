// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::collections::{BTreeMap, BTreeSet};

use drasi_computation_contracts::{GraphChangeCodec, InputBinding, OutputCursor, PortId, StreamId};
use drasi_computation_plugin_sdk::{
    output_snapshots::SnapshotOpen, CreateRequest, NativeOutputSnapshots,
};
use drasi_core::{
    computation::QueryInputBatch,
    models::{ChangeOrdering, SourceChange},
    query::QueryEvaluator,
};
use futures_util::StreamExt;

use super::output::{QueryOutput, Rows};

pub(super) type InputCursors = BTreeMap<(PortId, StreamId), OutputCursor>;

pub(super) struct SnapshotBootstrap {
    inputs: Vec<InputBinding>,
    snapshots: NativeOutputSnapshots,
}

impl SnapshotBootstrap {
    pub(super) fn from_request(
        request: &CreateRequest,
        snapshots: Option<NativeOutputSnapshots>,
    ) -> anyhow::Result<Option<Self>> {
        let Some(configuration) = request.configuration.get("bootstrap") else {
            return Ok(None);
        };
        let configured = serde_json::from_value::<Vec<SnapshotOpen>>(configuration.clone())?;
        anyhow::ensure!(
            !configured.is_empty() && configured.len() == request.inputs.len(),
            "bootstrap must cover every declared input exactly once"
        );
        let mut endpoints = BTreeSet::new();
        let mut routes = BTreeSet::new();
        let mut inputs = Vec::with_capacity(configured.len());
        for configured in configured {
            let endpoint = configured.endpoint()?;
            anyhow::ensure!(
                endpoints.insert(endpoint.clone()),
                "duplicate bootstrap output {endpoint}"
            );
            let mut matching = request
                .inputs
                .iter()
                .filter(|input| input.output == endpoint);
            let input = matching.next().ok_or_else(|| {
                anyhow::anyhow!("bootstrap output {endpoint} has no declared input")
            })?;
            anyhow::ensure!(
                matching.next().is_none() && input.input_port.as_str() == "in",
                "bootstrap output {endpoint} must match exactly one query input"
            );
            anyhow::ensure!(
                routes.insert((input.input_port.clone(), input.stream.clone())),
                "bootstrap inputs cannot share the same input port and stream"
            );
            inputs.push(input.clone());
        }
        Ok(Some(Self {
            inputs,
            snapshots: snapshots.ok_or_else(|| {
                anyhow::anyhow!("snapshot bootstrap requires the host output_snapshots resource")
            })?,
        }))
    }

    pub(super) async fn load(
        &self,
        evaluator: &QueryEvaluator,
        output: &QueryOutput,
        scopes: Option<&drasi_query_common::QueryInputScopes>,
        telemetry: &super::observations::QueryObservations,
    ) -> anyhow::Result<(InputCursors, Rows)> {
        let mut cursors = InputCursors::new();
        let mut rows = Rows::new();
        for input in &self.inputs {
            let scope = scopes.and_then(|scopes| {
                scopes
                    .get(&(input.input_port.clone(), input.stream.clone()))
                    .cloned()
            });
            let mut snapshot = self
                .snapshots
                .snapshot(&input.output, &GraphChangeCodec::schema())
                .await?;
            anyhow::ensure!(
                snapshot.cursor.producer().stream() == &input.stream,
                "bootstrap snapshot stream does not match the query's declared input"
            );
            while let Some(record) = snapshot.records.next().await {
                let element = GraphChangeCodec::decode_element(&record?)?.try_into()?;
                let results = telemetry
                    .evaluate::<false>(
                        evaluator,
                        QueryInputBatch::new(
                            vec![SourceChange::Insert { element }],
                            ChangeOrdering::EventTime,
                        )
                        .with_optional_input_scope(scope.clone()),
                        drasi_computation_contracts::telemetry::QueryObservationPhase::Bootstrap,
                    )
                    .await?;
                output.apply_initial(&mut rows, &results)?;
                telemetry.input(
                    drasi_computation_contracts::telemetry::QueryObservationPhase::Bootstrap,
                )?;
            }
            cursors.insert(
                (input.input_port.clone(), input.stream.clone()),
                snapshot.cursor,
            );
        }
        Ok((cursors, rows))
    }
}
