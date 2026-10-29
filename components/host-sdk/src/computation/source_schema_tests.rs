// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_lib::schema::{NodeSchema, SourceSchema};
use futures_util::FutureExt;
use std::sync::atomic::AtomicBool;
use tokio::sync::Notify;

#[derive(Default)]
struct Reader {
    calls: AtomicUsize,
    blocked: AtomicBool,
    fail: AtomicBool,
    oversize: AtomicBool,
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl SourceSchemaReader for Reader {
    async fn describe_schema(&self) -> anyhow::Result<Option<SourceSchema>> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.entered.notify_one();
        if self.blocked.load(Ordering::Acquire) {
            self.release.notified().await;
        }
        anyhow::ensure!(
            !self.fail.load(Ordering::Acquire),
            "actual schema read failed"
        );
        let label = if self.oversize.load(Ordering::Acquire) {
            "x".repeat(abi::source_schema::MAX_SCHEMA_BYTES)
        } else {
            "ActualTable".to_owned()
        };
        Ok(Some(SourceSchema {
            nodes: vec![NodeSchema::new(label)],
            relations: Vec::new(),
        }))
    }
}

struct SchemaFactory(Arc<Reader>);

impl sdk::Factory for SchemaFactory {
    fn metadata(&self) -> FactoryMetadata {
        sdk::Factory::metadata(&TestFactory {
            role: ComponentRole::Source,
            name: "test/source-schema",
            controlled: false,
            transactional: false,
        })
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        Ok(sdk::Component::Source(Box::new(SchemaSource {
            descriptor: ComponentDescriptor::try_new(request.id.clone(), self.metadata().ports)?,
            reader: self.0.clone(),
        }))
        .into())
    }
}

struct SchemaSource {
    descriptor: ComponentDescriptor,
    reader: Arc<Reader>,
}

#[async_trait]
impl sdk::NativeComponent for SchemaSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<Value> {
        Ok(json!({}))
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl sdk::NativeSource for SchemaSource {
    fn source_schema_reader(&self) -> Option<Arc<dyn SourceSchemaReader>> {
        Some(self.reader.clone())
    }
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}

fn definition(reader: Arc<Reader>) -> anyhow::Result<sdk::PluginDefinition> {
    sdk::PluginDefinition::new(
        "test/schema-library",
        "1",
        vec![Arc::new(SchemaFactory(reader))],
        vec![GraphChangeCodec::schema()],
    )
}

async fn source_schema_matrix() -> anyhow::Result<()> {
    let state = Arc::new(Reader::default());
    let plugin = NativePlugin::from_definition(definition(state.clone())?)?;
    let mut source =
        plugin.factories()[0].create_component(ComponentId::try_new("source")?, json!({}))?;
    let reader = source
        .source_schema_reader()
        .expect("negotiated capability");
    assert_eq!(state.calls.load(Ordering::Acquire), 0);
    assert!(reader.describe_schema().await.is_err());
    source.start().await?;
    {
        let mut next = Box::pin(source.next());
        tokio::select! {
            biased;
            result = &mut next => panic!("source should remain pending: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        let schema = tokio::time::timeout(Duration::from_secs(1), reader.describe_schema())
            .await??
            .expect("actual data schema");
        assert_eq!(schema.nodes[0].label, "ActualTable");
    }

    state.fail.store(true, Ordering::Release);
    let error = reader.describe_schema().await.unwrap_err();
    assert!(format!("{error:#}").contains("actual schema read failed"));
    state.fail.store(false, Ordering::Release);
    state.oversize.store(true, Ordering::Release);
    assert!(reader.describe_schema().await.is_err());
    state.oversize.store(false, Ordering::Release);

    state.blocked.store(true, Ordering::Release);
    // Consume prior completion notifications before synchronizing the next read.
    let _ = state.entered.notified().now_or_never();
    let mut pending = Box::pin(reader.describe_schema());
    tokio::select! {
        biased;
        result = &mut pending => panic!("schema read should remain pending: {result:?}"),
        _ = state.entered.notified() => {}
    }
    let busy = reader.describe_schema().await.unwrap_err();
    assert_eq!(
        busy.downcast_ref::<sdk::transport::Failure>().unwrap().code,
        abi::status::BUSY
    );
    drop(pending);
    state.blocked.store(false, Ordering::Release);
    assert!(reader.describe_schema().await?.is_some());

    state.blocked.store(true, Ordering::Release);
    let _ = state.entered.notified().now_or_never();
    let mut old = Box::pin(reader.describe_schema());
    tokio::select! {
        biased;
        result = &mut old => panic!("schema read should remain pending: {result:?}"),
        _ = state.entered.notified() => {}
    }
    source.stop().await?;
    assert!(reader.describe_schema().await.is_err());
    source.start().await?;
    state.blocked.store(false, Ordering::Release);
    assert!(reader.describe_schema().await?.is_some());
    let retired = tokio::time::timeout(Duration::from_secs(1), old)
        .await?
        .unwrap_err();
    assert_eq!(
        retired
            .downcast_ref::<sdk::transport::Failure>()
            .unwrap()
            .code,
        abi::status::CLOSED
    );
    assert!(reader.describe_schema().await?.is_some());
    source.stop().await?;
    source.start().await?;
    state.blocked.store(true, Ordering::Release);
    let _ = state.entered.notified().now_or_never();
    let mut removed = Box::pin(reader.describe_schema());
    tokio::select! {
        biased;
        result = &mut removed => panic!("schema read should remain pending: {result:?}"),
        _ = state.entered.notified() => {}
    }
    drop(source);
    assert!(tokio::time::timeout(Duration::from_secs(1), removed)
        .await?
        .is_err());
    assert!(reader.describe_schema().await.is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn source_schema_current_thread() -> anyhow::Result<()> {
    source_schema_matrix().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_multi_thread() -> anyhow::Result<()> {
    source_schema_matrix().await
}

fn exported() -> &'static sdk::ExportedPlugin {
    static EXPORT: std::sync::OnceLock<sdk::ExportedPlugin> = std::sync::OnceLock::new();
    EXPORT.get_or_init(|| {
        sdk::ExportedPlugin::new(definition(Arc::new(Reader::default())).unwrap()).unwrap()
    })
}

unsafe extern "C" fn metadata() -> *const abi::Metadata {
    exported().metadata()
}
unsafe extern "C" fn entry(out: *mut abi::PluginHandle) -> abi::Status {
    sdk::transport::status_boundary(|| unsafe { exported().entry(out) })
}
unsafe extern "C" fn invalid_extension() -> *const abi::source_schema::PluginSourceSchemaV1 {
    static INVALID: abi::source_schema::PluginSourceSchemaV1 =
        abi::source_schema::PluginSourceSchemaV1 {
            header: abi::Header::new::<abi::source_schema::PluginSourceSchemaV1>(),
            version: 999,
            reserved: 0,
            supported: None,
            begin: None,
        };
    &INVALID
}

#[test]
fn source_schema_extension_is_optional_and_validated() -> anyhow::Result<()> {
    let old = unsafe { NativePlugin::from_entry_points(metadata, entry)? };
    let source = old.factories()[0].create_component(ComponentId::try_new("source")?, json!({}))?;
    assert!(source.source_schema_reader().is_none());
    assert!(unsafe {
        NativePlugin::from_entry_points_with_source_schema(metadata, entry, Some(invalid_extension))
    }
    .is_err());
    Ok(())
}
