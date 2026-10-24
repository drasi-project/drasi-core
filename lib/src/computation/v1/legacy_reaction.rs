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

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;

use super::{
    ComponentCreationError, ComponentDescriptor, ComponentFactory, ComponentId, ComponentRole,
    ComponentSpecification, ComputationComponent, ConfigurationSchema, ConstructedComponent,
    ConstructionContext, EnvelopeSink, FactoryDescriptor, ImplementationIdentity, InputEnvelope,
    PipeRequirements, PortDescriptor, PortDirection, PortId, QueryChangeCodec, ResourceHandle,
    ResourceId, ResourceOwnership, ResourceRequirement, ResourceRole, ResourceSpecification,
    SinkCompletion,
};

/// An explicit reference to an already managed legacy Reaction. This resource
/// never owns its initialization, processing loop, stop or deprovision lifecycle.
pub struct LegacyReactionResource(pub Arc<dyn crate::Reaction>);

pub struct LegacyReactionSink {
    descriptor: ComponentDescriptor,
    resource: Arc<LegacyReactionResource>,
}

fn descriptor(id: ComponentId) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        id,
        vec![PortDescriptor::new(
            PortId::try_new("in").expect("port"),
            PortDirection::Input,
            QueryChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )
    .expect("reaction input descriptor")
}

impl LegacyReactionSink {
    pub fn borrowed(id: ComponentId, reaction: Arc<dyn crate::Reaction>) -> Self {
        Self {
            descriptor: descriptor(id),
            resource: Arc::new(LegacyReactionResource(reaction)),
        }
    }
}

#[async_trait]
impl ComputationComponent for LegacyReactionSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for LegacyReactionSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Accepted
    }
    /// Hands one query-row envelope to the borrowed reaction's queue.
    ///
    /// The envelope is accepted once `enqueue_query_result` returns `Ok`, and
    /// nothing may fail after that: an error would make the graph redeliver a
    /// row the reaction already holds.
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        if QueryChangeCodec::is_snapshot(&input.envelope) {
            anyhow::bail!("legacy enqueue cannot claim snapshot replacement; use an explicit bootstrap-capable boundary");
        }
        let result = QueryChangeCodec::to_legacy_result(&input.envelope)?;
        self.resource.0.enqueue_query_result(result).await
    }
}

pub struct LegacyReactionFactory {
    descriptor: FactoryDescriptor,
}

impl Default for LegacyReactionFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new(
                    "drasi/legacy-reaction-adapter",
                    "1",
                )
                .expect("implementation"),
                role: ComponentRole::Sink,
                configuration_version: 1,
                configuration: ConfigurationSchema::default(),
                dependencies: BTreeMap::from([(
                    Arc::from("reaction"),
                    ResourceRequirement::exactly_one::<LegacyReactionResource>(
                        ResourceRole::LegacyReaction,
                    ),
                )]),
            },
        }
    }
}

#[async_trait]
impl ComponentFactory for LegacyReactionFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        if spec.descriptor != descriptor(spec.descriptor.id().clone())
            || spec.completion != Some(SinkCompletion::Accepted)
        {
            anyhow::bail!("legacy reaction adapter accepts typed query rows and declares Accepted, never Handled");
        }
        Ok(())
    }
    fn validate_resources(
        &self,
        spec: &ComponentSpecification,
        declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        _resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate(spec)?;
        for id in spec.dependencies.get("reaction").into_iter().flatten() {
            if declarations[id].ownership != ResourceOwnership::Borrowed {
                anyhow::bail!("legacy enqueue adapter borrows an externally managed reaction");
            }
        }
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError> {
        let resource = context
            .resources::<LegacyReactionResource>("reaction")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| ComponentCreationError::terminal(anyhow::anyhow!("missing reaction")))?;
        Ok(ConstructedComponent::sink(Box::new(LegacyReactionSink {
            descriptor: descriptor(context.component_id),
            resource,
        })))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex,
        },
    };

    use drasi_core::evaluation::{
        context::QueryPartEvaluationContext, variable_value::VariableValue,
    };

    use super::*;
    use crate::{
        channels::{ComponentStatus, QueryResult, ResultDiff},
        computation::v1::{QueryOutputMetadata, StreamId, SystemMetadata},
        context::ReactionRuntimeContext,
    };

    #[derive(Default)]
    struct RecordingReaction {
        reject: AtomicBool,
        attempts: Mutex<Vec<QueryResult>>,
    }

    #[async_trait]
    impl crate::Reaction for RecordingReaction {
        fn id(&self) -> &str {
            "reaction"
        }
        fn type_name(&self) -> &str {
            "recording"
        }
        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }
        fn query_ids(&self) -> Vec<String> {
            vec!["query".into()]
        }
        async fn initialize(&self, _context: ReactionRuntimeContext) {}
        async fn start(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn status(&self) -> ComponentStatus {
            ComponentStatus::Running
        }
        async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
            self.attempts.lock().expect("attempts").push(result);
            if self.reject.load(Ordering::Acquire) {
                anyhow::bail!("reaction queue closed");
            }
            Ok(())
        }
    }

    fn sink(reaction: &Arc<RecordingReaction>) -> LegacyReactionSink {
        LegacyReactionSink::borrowed(ComponentId::try_new("sink").expect("id"), reaction.clone())
    }

    fn added_row(sequence: u64) -> InputEnvelope {
        let envelope = QueryChangeCodec::encode_evaluation(
            None,
            &ComponentId::try_new("query").expect("id"),
            SystemMetadata::new(StreamId::try_new("query/out").expect("stream"), sequence),
            &[QueryPartEvaluationContext::Adding {
                after: BTreeMap::from([(Box::<str>::from("value"), VariableValue::from(1))]),
                row_signature: 1,
            }],
            QueryOutputMetadata {
                query_id: "query".into(),
                source_id: None,
                timestamp: chrono::Utc::now(),
                metadata: HashMap::new(),
                profiling: None,
            },
        )
        .expect("encode")
        .expect("one row");
        InputEnvelope {
            port: PortId::try_new("in").expect("port"),
            envelope,
        }
    }

    #[tokio::test]
    async fn an_accepted_row_is_enqueued_exactly_once() {
        let reaction = Arc::new(RecordingReaction::default());

        sink(&reaction)
            .handle(added_row(7))
            .await
            .expect("accepted");

        let attempts = reaction.attempts.lock().expect("attempts");
        let [result] = attempts.as_slice() else {
            panic!("expected one enqueue, got {}", attempts.len());
        };
        assert_eq!(result.query_id, "query");
        assert_eq!(result.sequence, 7);
        assert!(matches!(
            result.results.as_slice(),
            [ResultDiff::Add { .. }]
        ));
    }

    #[tokio::test]
    async fn a_rejected_row_is_reported_unaccepted_so_it_is_redelivered() {
        let reaction = Arc::new(RecordingReaction::default());
        reaction.reject.store(true, Ordering::Release);

        let error = sink(&reaction)
            .handle(added_row(7))
            .await
            .expect_err("the reaction refused the row");

        assert_eq!(error.to_string(), "reaction queue closed");
        assert_eq!(reaction.attempts.lock().expect("attempts").len(), 1);
    }
}
