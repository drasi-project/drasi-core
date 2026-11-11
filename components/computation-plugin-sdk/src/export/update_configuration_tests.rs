// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::{
    ApplyUpdateRequest, ComponentUpdateData, ComponentUpdateMode, ComponentUpdatePlan,
    ConfigDefinition, ConfigOption, ConfigType, UpdateRequest,
};
use abi::operation::*;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, ComponentRole, GraphChangeCodec, ImplementationIdentity,
    OutputEnvelope, PortDescriptor, PortId,
};
use serde_json::{json, Value};

fn definition() -> ConfigDefinition {
    ConfigDefinition::try_new([
        ConfigOption::new("stream", ConfigType::String, true, "Output stream."),
        ConfigOption::new("count", ConfigType::Integer, true, "Number of values."),
        ConfigOption::new("paused", ConfigType::Boolean, true, "Pause publication."),
        ConfigOption::new(
            "interval_ms",
            ConfigType::Integer,
            true,
            "Publication interval.",
        )
        .minimum(1),
        ConfigOption::new("start", ConfigType::Integer, false, "Initial value.").default(0),
        ConfigOption::new("step", ConfigType::Integer, false, "Increment.").default(1),
        ConfigOption::new("label", ConfigType::String, false, "Optional label.")
            .default_description("Chosen by the component."),
    ])
    .unwrap()
}

fn parsed(configuration: &Value) -> Value {
    let mut result = configuration.clone();
    let object = result.as_object_mut().unwrap();
    object.entry("start").or_insert(json!(0));
    object.entry("step").or_insert(json!(1));
    result
}

#[derive(Default)]
struct Shared {
    received: Mutex<Vec<ApplyUpdateRequest>>,
    reported: Mutex<Option<Value>>,
    fail_next: AtomicBool,
    pend_next: AtomicBool,
}

struct TestFactory {
    shared: Arc<Shared>,
    publish_defaults: bool,
    allow_additional: bool,
}

impl Factory for TestFactory {
    fn metadata(&self) -> FactoryMetadata {
        let mut configuration = definition().schema();
        configuration.allow_additional = self.allow_additional;
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/parsed-counter", "1").unwrap(),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration,
            ports: vec![PortDescriptor::new(
                PortId::try_new("out").unwrap(),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                Default::default(),
            )],
            completion: None,
            capabilities: crate::Capabilities {
                update_modes: vec![ComponentUpdateMode::InPlace],
                ..Default::default()
            },
        }
    }
    fn configuration_details(&self) -> Option<crate::ConfigDetails> {
        self.publish_defaults.then(|| definition().details())
    }
    fn supports_update_planning(&self) -> bool {
        true
    }
    fn plan_update(&self, _: &UpdateRequest) -> anyhow::Result<ComponentUpdatePlan> {
        Ok(ComponentUpdatePlan {
            mode: ComponentUpdateMode::InPlace,
            data: ComponentUpdateData::Preserve,
        })
    }
    fn create(
        &self,
        request: &CreateRequest,
        _: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        Ok(Component::Source(Box::new(ParsedCounter {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            configuration: parsed(&request.configuration),
            shared: self.shared.clone(),
        }))
        .into())
    }
}

struct ParsedCounter {
    descriptor: ComponentDescriptor,
    configuration: Value,
    shared: Arc<Shared>,
}

#[async_trait]
impl ComputationComponent for ParsedCounter {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<Value> {
        Ok(self.configuration.clone())
    }
    fn supports_apply_update(&self) -> bool {
        true
    }
    async fn apply_update(&mut self, update: &ApplyUpdateRequest) -> anyhow::Result<()> {
        self.shared.received.lock().unwrap().push(update.clone());
        self.configuration = self
            .shared
            .reported
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| parsed(&update.request.proposed.configuration));
        if self.shared.fail_next.swap(false, Ordering::AcqRel) {
            anyhow::bail!("injected update failure after configuration mutation");
        }
        if self.shared.pend_next.swap(false, Ordering::AcqRel) {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for ParsedCounter {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(None)
    }
}

struct Fixture {
    factory: abi::FactoryHandle,
    component: abi::ComponentHandle,
    request: CreateRequest,
    shared: Arc<Shared>,
}

impl Fixture {
    fn new(publish_defaults: bool, allow_additional: bool) -> anyhow::Result<Self> {
        let shared = Arc::new(Shared::default());
        let exported = ExportedPlugin::new(PluginDefinition::new(
            "fixture/parsed-defaults",
            "1",
            vec![Arc::new(TestFactory {
                shared: shared.clone(),
                publish_defaults,
                allow_additional,
            })],
            vec![GraphChangeCodec::schema()],
        )?)?;
        let request = CreateRequest {
            id: ComponentId::try_new("ad06fe65-63a2-43f9-ae52-7a14af5b294d")?,
            implementation: exported.state.definition.metadata.factories[0]
                .implementation
                .clone(),
            configuration_version: 1,
            configuration: json!({"stream": "counter/out", "count": 2, "paused": false, "interval_ms": 10}),
            scope: Some(crate::Scope {
                instance_id: "instance".into(),
                graph_id: "graph".into(),
                generation: 1,
            }),
            inputs: Vec::new(),
            activation_credentials_bound: false,
            update_directive: None,
        };
        let mut plugin = abi::PluginHandle::null();
        let mut factory = abi::FactoryHandle::null();
        let mut component = abi::ComponentHandle::null();
        let bytes = wire::encode_create_request(&request)?;
        unsafe {
            exported.entry(&mut plugin)?;
            transport::take_status((*plugin.vtable).factory.unwrap()(
                plugin.state,
                0,
                &mut factory,
            ))?;
            (*plugin.vtable).release.unwrap()(plugin.state);
            transport::take_status((*factory.vtable).create.unwrap()(
                factory.state,
                BorrowedBytes::new(&bytes),
                &mut component,
            ))?;
        }
        Ok(Self {
            factory,
            component,
            request,
            shared,
        })
    }

    fn begin(&self, code: u32, input: &[u8]) -> Result<transport::OperationFuture, Failure> {
        let mut operation = abi::OperationHandle::null();
        unsafe {
            transport::take_status((*self.component.vtable).begin.unwrap()(
                self.component.state,
                code,
                BorrowedBytes::new(input),
                std::ptr::null(),
                &mut operation,
            ))?;
            transport::OperationFuture::new(operation)
        }
    }

    async fn run(&self, code: u32) -> anyhow::Result<()> {
        assert!(self.begin(code, &[])?.await?.is_empty());
        Ok(())
    }

    async fn activate_paused(&self) -> anyhow::Result<()> {
        self.run(INITIALIZE).await?;
        self.run(START).await?;
        self.run(PAUSE_PROCESSING).await
    }

    fn approved(&self) -> anyhow::Result<ApplyUpdateRequest> {
        let mut proposed = self.request.clone();
        proposed.configuration["interval_ms"] = json!(20);
        self.plan(self.request.clone(), proposed, "interval-update")
    }

    fn plan(
        &self,
        current: CreateRequest,
        proposed: CreateRequest,
        operation_id: &str,
    ) -> anyhow::Result<ApplyUpdateRequest> {
        let request = UpdateRequest { current, proposed };
        let bytes = serde_json::to_vec(&request)?;
        let plan = unsafe {
            serde_json::from_slice(&transport::take_reply((*self.factory.vtable)
                .plan_update
                .unwrap()(
                self.factory.state,
                BorrowedBytes::new(&bytes),
            ))?)?
        };
        Ok(ApplyUpdateRequest {
            operation_id: operation_id.into(),
            request,
            plan,
        })
    }

    async fn apply(&self, update: &ApplyUpdateRequest) -> anyhow::Result<()> {
        assert!(self
            .begin(APPLY_UPDATE, &wire::encode(update)?)?
            .await?
            .is_empty());
        Ok(())
    }

    fn configuration(&self) -> anyhow::Result<Value> {
        unsafe {
            Ok(serde_json::from_slice(&transport::take_reply((*self
                .component
                .vtable)
                .configuration
                .unwrap()(
                self.component.state,
            ))?)?)
        }
    }

    async fn dispose(&self) -> anyhow::Result<()> {
        self.run(STOP).await?;
        self.run(DISPOSE).await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            (*self.component.vtable).release.unwrap()(self.component.state);
            (*self.factory.vtable).release.unwrap()(self.factory.state);
        }
    }
}

async fn omitted_defaults() -> anyhow::Result<()> {
    let fixture = Fixture::new(true, false)?;
    fixture.activate_paused().await?;
    let update = fixture.approved()?;
    assert!(update.request.proposed.configuration.get("start").is_none());
    fixture.apply(&update).await?;
    assert_eq!(
        fixture.configuration()?,
        parsed(&update.request.proposed.configuration)
    );
    assert_eq!(
        *fixture.shared.received.lock().unwrap(),
        vec![update.clone()]
    );
    fixture.run(RESUME_PROCESSING).await?;
    fixture.apply(&update).await?;
    assert_eq!(fixture.shared.received.lock().unwrap().len(), 1);

    let mut rewritten = update.clone();
    rewritten.request.proposed.configuration = parsed(&rewritten.request.proposed.configuration);
    assert!(
        fixture.apply(&rewritten).await.is_err(),
        "default equivalence must not rewrite operation receipts"
    );
    assert_eq!(fixture.shared.received.lock().unwrap().len(), 1);
    let current = update.request.proposed.clone();
    let mut proposed = current.clone();
    proposed.configuration["interval_ms"] = json!(30);
    let next = fixture.plan(current, proposed, "next-interval-update")?;
    fixture.run(PAUSE_PROCESSING).await?;
    fixture.apply(&next).await?;
    assert_eq!(
        *fixture.shared.received.lock().unwrap(),
        vec![update, next.clone()]
    );
    assert_eq!(
        fixture.configuration()?,
        parsed(&next.request.proposed.configuration)
    );
    fixture.dispose().await
}

#[tokio::test(flavor = "current_thread")]
async fn omitted_published_defaults_match_without_rewriting_requests_current_thread(
) -> anyhow::Result<()> {
    omitted_defaults().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn omitted_published_defaults_match_without_rewriting_requests_multi_thread(
) -> anyhow::Result<()> {
    omitted_defaults().await
}

#[tokio::test]
async fn defaults_are_compared_on_both_sides_without_replacing_explicit_nondefaults(
) -> anyhow::Result<()> {
    let fixture = Fixture::new(true, false)?;
    fixture.activate_paused().await?;
    let mut update = fixture.approved()?;
    let raw = update.request.proposed.configuration.clone();
    update.request.proposed.configuration = parsed(&raw);
    *fixture.shared.reported.lock().unwrap() = Some(raw);
    fixture.apply(&update).await?;
    assert!(fixture.configuration()?.get("step").is_none());
    fixture.dispose().await?;

    for reported_step in [None, Some(json!(1)), Some(json!(8))] {
        let fixture = Fixture::new(true, false)?;
        fixture.activate_paused().await?;
        let mut update = fixture.approved()?;
        update.request.proposed.configuration["step"] = json!(7);
        let mut wrong = update.request.proposed.configuration.clone();
        wrong.as_object_mut().unwrap().remove("step");
        if let Some(step) = reported_step {
            wrong["step"] = step;
        }
        *fixture.shared.reported.lock().unwrap() = Some(wrong);
        assert!(fixture
            .apply(&update)
            .await
            .unwrap_err()
            .to_string()
            .contains("proposed configuration"));
        assert!(fixture.run(RESUME_PROCESSING).await.is_err());
        *fixture.shared.reported.lock().unwrap() = None;
        fixture.apply(&update).await?;
        assert_eq!(fixture.configuration()?["step"], 7);
        assert_eq!(
            *fixture.shared.received.lock().unwrap(),
            vec![update.clone(), update]
        );
        fixture.dispose().await?;
    }
    Ok(())
}

#[tokio::test]
async fn comparison_rejects_unpublished_defaults_extra_fields_and_changed_values(
) -> anyhow::Result<()> {
    let no_details = Fixture::new(false, false)?;
    no_details.activate_paused().await?;
    assert!(no_details
        .apply(&no_details.approved()?)
        .await
        .unwrap_err()
        .to_string()
        .contains("proposed configuration"));
    no_details.dispose().await?;

    for (field, value) in [
        ("count", json!(99)),
        ("step", json!(2)),
        ("label", json!("generated")),
        ("extra", json!(true)),
    ] {
        let fixture = Fixture::new(true, true)?;
        fixture.activate_paused().await?;
        let update = fixture.approved()?;
        let mut wrong = parsed(&update.request.proposed.configuration);
        wrong[field] = value;
        *fixture.shared.reported.lock().unwrap() = Some(wrong);
        assert!(fixture
            .apply(&update)
            .await
            .unwrap_err()
            .to_string()
            .contains("proposed configuration"));
        assert!(fixture.run(RESUME_PROCESSING).await.is_err());
        fixture.dispose().await?;
    }
    Ok(())
}

#[tokio::test]
async fn failed_and_cancelled_updates_retry_the_exact_raw_intent_with_defaults(
) -> anyhow::Result<()> {
    for cancelled in [false, true] {
        let fixture = Fixture::new(true, false)?;
        fixture.activate_paused().await?;
        let update = fixture.approved()?;
        if cancelled {
            fixture.shared.pend_next.store(true, Ordering::Release);
            let mut operation = fixture.begin(APPLY_UPDATE, &wire::encode(&update)?)?;
            assert!(futures_util::poll!(&mut operation).is_pending());
            operation.cancel()?;
            assert_eq!(operation.await.unwrap_err().code, abi::status::CANCELLED);
        } else {
            fixture.shared.fail_next.store(true, Ordering::Release);
            assert!(fixture
                .apply(&update)
                .await
                .unwrap_err()
                .to_string()
                .contains("injected update failure"));
        }
        assert_eq!(
            fixture.configuration()?,
            parsed(&update.request.proposed.configuration)
        );
        assert!(fixture.run(RESUME_PROCESSING).await.is_err());
        let mut rewritten = update.clone();
        rewritten.request.proposed.configuration =
            parsed(&rewritten.request.proposed.configuration);
        assert!(fixture.apply(&rewritten).await.is_err());
        assert_eq!(fixture.shared.received.lock().unwrap().len(), 1);
        fixture.apply(&update).await?;
        fixture.run(RESUME_PROCESSING).await?;
        fixture.apply(&update).await?;
        assert_eq!(
            *fixture.shared.received.lock().unwrap(),
            vec![update.clone(), update]
        );
        fixture.dispose().await?;
    }
    Ok(())
}
