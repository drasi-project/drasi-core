// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;
use drasi_computation_plugin_sdk as sdk;
use drasi_lib::{
    computation::v1::*,
    identity::{CredentialContext, Credentials, IdentityProvider},
};
use futures_util::poll;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

#[path = "../../tests/fixtures/native_activation_credentials.rs"]
pub(super) mod fixture;

struct Fixture {
    plugin: Arc<NativePlugin>,
    probe: extern "C" fn(u32) -> u64,
    reset: extern "C" fn(),
}

fn fixtures() -> anyhow::Result<Vec<Fixture>> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let target = match std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        Some(path) => workspace.join(path),
        None => workspace.join("target"),
    };
    let path = target.join("debug/examples").join(format!(
        "{}native_activation_credentials{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX,
    ));
    let plugin = load(&path)?;
    let library = unsafe { libloading::Library::new(&path)? };
    let probe =
        unsafe { *library.get::<extern "C" fn(u32) -> u64>(b"drasi_activation_fixture_probe\0")? };
    let reset = unsafe { *library.get::<extern "C" fn()>(b"drasi_activation_fixture_reset\0")? };
    // The native loader process-pins this same binary.
    drop(library);
    Ok(vec![
        Fixture {
            plugin: NativePlugin::from_definition(fixture::definition()?)?,
            probe: fixture::drasi_activation_fixture_probe,
            reset: fixture::drasi_activation_fixture_reset,
        },
        Fixture {
            plugin,
            probe,
            reset,
        },
    ])
}

#[derive(Clone)]
struct Provider {
    reads: Arc<AtomicUsize>,
    value: Arc<Mutex<Credentials>>,
    failed: Arc<AtomicBool>,
    pending: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    contexts: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
}

impl Provider {
    fn new() -> Self {
        Self {
            reads: Arc::new(AtomicUsize::new(0)),
            value: Arc::new(Mutex::new(Credentials::UsernamePassword {
                username: "provider-user".into(),
                password: "first-secret".into(),
            })),
            failed: Arc::new(AtomicBool::new(false)),
            pending: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicUsize::new(0)),
            contexts: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn resource(&self) -> Arc<NativeIdentityResource> {
        Arc::new(NativeIdentityResource::new(Arc::new(self.clone())))
    }
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl IdentityProvider for Provider {
    async fn get_credentials(&self, context: &CredentialContext) -> anyhow::Result<Credentials> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.contexts
            .lock()
            .unwrap()
            .push(context.properties.clone().into_iter().collect());
        self.active.fetch_add(1, Ordering::SeqCst);
        let _active = Active(self.active.clone());
        if self.pending.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        anyhow::ensure!(
            !self.failed.load(Ordering::SeqCst),
            "must-not-leak-first-secret"
        );
        Ok(self.value.lock().unwrap().clone())
    }
    fn clone_box(&self) -> Box<dyn IdentityProvider> {
        Box::new(self.clone())
    }
}

fn id() -> ComponentId {
    ComponentId::try_new("credential-component").unwrap()
}

async fn refresh_and_failure_matrix() -> anyhow::Result<()> {
    for fixture in fixtures()? {
        (fixture.reset)();
        let factory = &fixture.plugin.factories()[0];
        let provider = Provider::new();
        let configuration = fixture::configuration();
        let mut proxy = factory.create_component_with_identity(
            id(),
            configuration.clone(),
            provider.resource(),
        )?;
        assert_eq!(provider.reads.load(Ordering::SeqCst), 0);
        assert_eq!((fixture.probe)(fixture::CONSTRUCTED), 1);
        assert_eq!(proxy.configuration()?, configuration);
        proxy.start().await?;
        assert_eq!((fixture.probe)(fixture::LAST_SECRET), 1);
        assert_eq!((fixture.probe)(fixture::LAST_KIND), 1);
        proxy.stop().await?;
        *provider.value.lock().unwrap() = Credentials::Token {
            username: "provider-user".into(),
            token: "second-secret".into(),
        };
        proxy.start().await?;
        assert_eq!((fixture.probe)(fixture::LAST_SECRET), 2);
        assert_eq!((fixture.probe)(fixture::LAST_KIND), 2);
        proxy.stop().await?;
        assert_eq!(provider.reads.load(Ordering::SeqCst), 2);
        assert_eq!((fixture.probe)(fixture::CONSTRUCTED), 1);
        assert_eq!(proxy.configuration()?, configuration);
        assert!(provider
            .contexts
            .lock()
            .unwrap()
            .iter()
            .all(|context| context
                == &BTreeMap::from([
                    ("hostname".into(), "exact.database.test".into()),
                    ("port".into(), "3307".into()),
                ])));

        provider.failed.store(true, Ordering::SeqCst);
        let error = proxy.start().await.unwrap_err();
        assert!(!format!("{error:#}").contains("first-secret"));
        assert_eq!((fixture.probe)(fixture::AUTHENTICATED), 2);
        assert_eq!((fixture.probe)(fixture::LITERAL), 0);
        provider.failed.store(false, Ordering::SeqCst);
        *provider.value.lock().unwrap() = Credentials::Certificate {
            cert_pem: "secret-certificate".into(),
            key_pem: "secret-key".into(),
            username: None,
        };
        let error = proxy.start().await.unwrap_err();
        assert!(error.to_string().contains("certificate"));
        assert!(!format!("{error:#}").contains("secret-key"));
        assert_eq!((fixture.probe)(fixture::AUTHENTICATED), 2);
        *provider.value.lock().unwrap() = Credentials::Token {
            username: "provider-user".into(),
            token: "x".repeat(sdk::activation_credentials::MAX_CREDENTIAL_FIELD_BYTES + 1),
        };
        assert!(proxy.start().await.is_err());
        assert_eq!((fixture.probe)(fixture::AUTHENTICATED), 2);
        proxy.stop().await?;

        let mut literal = configuration.clone();
        literal["user"] = json!("literal-user");
        literal["password"] = json!("literal-password");
        let mut literal_proxy = factory.create_component(id(), literal.clone())?;
        literal_proxy.start().await?;
        literal_proxy.stop().await?;
        literal_proxy.start().await?;
        literal_proxy.stop().await?;
        assert_eq!((fixture.probe)(fixture::LITERAL), 2);
        assert_eq!(literal_proxy.configuration()?, literal);
        assert!(factory
            .create_component(id(), configuration.clone())
            .is_err());
        let mut missing_auth = configuration;
        missing_auth.as_object_mut().unwrap().remove("user");
        missing_auth.as_object_mut().unwrap().remove("password");
        assert!(factory
            .create_component_with_identity(id(), missing_auth, Provider::new().resource())
            .is_ok());
    }
    Ok(())
}

async fn cancellation_matrix() -> anyhow::Result<()> {
    for fixture in fixtures()? {
        (fixture.reset)();
        let factory = &fixture.plugin.factories()[0];
        let provider = Provider::new();
        provider.pending.store(true, Ordering::SeqCst);
        let mut old = factory.create_component_with_identity(
            id(),
            fixture::configuration(),
            provider.resource(),
        )?;
        let mut start = Box::pin(old.start());
        assert!(poll!(start.as_mut()).is_pending());
        assert_eq!(provider.active.load(Ordering::SeqCst), 1);
        assert_eq!((fixture.probe)(fixture::AUTHENTICATED), 0);
        drop(start);
        assert_eq!(provider.active.load(Ordering::SeqCst), 0);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), old.start())
                .await
                .is_err()
        );
        assert_eq!(provider.active.load(Ordering::SeqCst), 0);
        assert_eq!((fixture.probe)(fixture::AUTHENTICATED), 0);
        old.stop().await?;
        drop(old);
        provider.pending.store(false, Ordering::SeqCst);
        *provider.value.lock().unwrap() = Credentials::Token {
            username: "provider-user".into(),
            token: "second-secret".into(),
        };
        let mut replacement = factory.create_component_with_identity(
            id(),
            fixture::configuration(),
            provider.resource(),
        )?;
        replacement.start().await?;
        assert_eq!((fixture.probe)(fixture::AUTHENTICATED), 1);
        assert_eq!((fixture.probe)(fixture::LAST_SECRET), 2);
        replacement.stop().await?;

        let mut configuration = fixture::configuration();
        configuration["mode"] = json!("pending");
        let mut pending =
            factory.create_component_with_identity(id(), configuration, provider.resource())?;
        let inner = pending.inner.clone();
        let mut start = Box::pin(pending.start());
        assert!(poll!(start.as_mut()).is_pending());
        assert_eq!((fixture.probe)(fixture::IN_FLIGHT), 1);
        let busy = inner
            .begin(sdk::abi::operation::STOP, &[], None)
            .err()
            .unwrap();
        assert!(format!("{busy:#}").contains("already active"));
        drop(start);
        assert_eq!((fixture.probe)(fixture::IN_FLIGHT), 0);
        pending.stop().await?;
        assert_eq!((fixture.probe)(fixture::COMPLETED), 1);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), pending.start())
                .await
                .is_err()
        );
        assert_eq!((fixture.probe)(fixture::IN_FLIGHT), 0);
        pending.stop().await?;
        let mut replacement = factory.create_component_with_identity(
            id(),
            fixture::configuration(),
            provider.resource(),
        )?;
        replacement.start().await?;
        replacement.stop().await?;
        assert_eq!((fixture.probe)(fixture::COMPLETED), 2);

        let mut configuration = fixture::configuration();
        configuration["mode"] = json!("failure");
        let mut failed =
            factory.create_component_with_identity(id(), configuration, provider.resource())?;
        let error = failed.start().await.unwrap_err();
        assert!(!format!("{error:#}").contains("second-secret"));
        failed.stop().await?;
        let mut configuration = fixture::configuration();
        configuration["mode"] = json!("panic");
        let mut panicked =
            factory.create_component_with_identity(id(), configuration, provider.resource())?;
        assert!(format!("{:#}", panicked.start().await.unwrap_err()).contains("panicked"));
        assert_eq!((fixture.probe)(fixture::IN_FLIGHT), 0);
        panicked.stop().await?;
        assert!(
            panicked.start().await.is_err(),
            "a panic must quarantine the original native instance"
        );

        let mut malformed = factory.create_component_with_identity(
            id(),
            fixture::configuration(),
            provider.resource(),
        )?;
        let before = (fixture.probe)(fixture::AUTHENTICATED);
        for payload in [
            vec![0; sdk::activation_credentials::MAX_CREDENTIAL_PAYLOAD_BYTES + 1],
            sdk::wire::encode(&json!({"kind":"certificate","secret":"do-not-leak-certificate"}))?,
            sdk::wire::encode(&json!({"kind":"token","username":"provider-user","token":false}))?,
            Vec::new(),
        ] {
            let error = malformed
                .inner
                .unit(sdk::abi::operation::START, &payload)
                .await
                .unwrap_err();
            assert!(!format!("{error:#}").contains("do-not-leak-certificate"));
            assert_eq!((fixture.probe)(fixture::AUTHENTICATED), before);
        }
        malformed.start().await?;
        malformed.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(activation_credentials)]
async fn activation_credentials_current_thread() -> anyhow::Result<()> {
    refresh_and_failure_matrix().await?;
    cancellation_matrix().await?;
    configured_identity_matrix().await?;
    graph_binding_matrix().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(activation_credentials)]
async fn activation_credentials_multi_thread() -> anyhow::Result<()> {
    refresh_and_failure_matrix().await?;
    cancellation_matrix().await?;
    configured_identity_matrix().await?;
    graph_binding_matrix().await
}

struct PendingSource(ComponentDescriptor);
#[async_trait]
impl ComputationComponent for PendingSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for PendingSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}

async fn graph_binding_matrix() -> anyhow::Result<()> {
    for fixture in fixtures()? {
        for wrong_type in [false, true] {
            let factory = &fixture.plugin.factories()[0];
            let provider = Provider::new();
            let resource_id = ResourceId::try_new("identity")?;
            let mut configuration = fixture::configuration();
            configuration.as_object_mut().unwrap().remove("password");
            let mut specification = factory.specification(id(), configuration)?;
            specification
                .dependencies
                .insert("identity".into(), vec![resource_id.clone()]);
            let resource = if wrong_type {
                ResourceHandle::new(
                    ResourceRole::Identity,
                    Arc::new(String::from("not a provider")),
                )
            } else {
                provider.resource().resource()
            };
            let source_id = ComponentId::try_new("input")?;
            let out = PortId::try_new("out")?;
            let graph = ComputationGraph::builder("activation-identity-graph")
                .declare_resource(ResourceSpecification {
                    id: resource_id.clone(),
                    role: ResourceRole::Identity,
                    ownership: ResourceOwnership::Borrowed,
                    binding: "identity".into(),
                })?
                .provide_resource(resource_id, resource)?
                .source(Box::new(PendingSource(ComponentDescriptor::try_new(
                    source_id.clone(),
                    vec![PortDescriptor::new(
                        out.clone(),
                        PortDirection::Output,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?)))
                .component(specification, factory.clone())
                .bind_stream(
                    Endpoint::new(source_id.clone(), out.clone()),
                    StreamId::try_new("input/out")?,
                )
                .connect(
                    EdgeDefinition::new(
                        Endpoint::new(source_id, out),
                        Endpoint::new(id(), PortId::try_new("in")?),
                    ),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                )
                .build();
            assert_eq!(provider.reads.load(Ordering::SeqCst), 0);
            if wrong_type {
                assert!(graph.is_err());
                continue;
            }
            let mut graph = graph?;
            let run = graph.start()?;
            let control = run.control();
            let (result, started) =
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    tokio::join!(run, async {
                        let started = control.startup_report().await;
                        control.cancel();
                        started
                    })
                })
                .await?;
            assert_eq!(started?.summary, OperationSummary::Completed);
            assert!(matches!(result, Ok(()) | Err(GraphError::Cancelled)));
            assert_eq!(provider.reads.load(Ordering::SeqCst), 1);
            graph.shutdown().await?;
        }
    }
    Ok(())
}

struct Descriptor {
    creations: Arc<AtomicUsize>,
    failed: Arc<AtomicBool>,
}
#[async_trait]
impl drasi_plugin_sdk::IdentityProviderPluginDescriptor for Descriptor {
    fn kind(&self) -> &str {
        "credential-fixture"
    }
    fn config_version(&self) -> &str {
        "1"
    }
    fn config_schema_name(&self) -> &str {
        "IdentityConfig"
    }
    fn config_schema_json(&self) -> String {
        json!({"IdentityConfig":{"type":"object","required":["password"],"properties":{
            "password":{"$ref":"#/components/schemas/ConfigValueString"}
        },"additionalProperties":false}})
        .to_string()
    }
    async fn create_identity_provider(
        &self,
        configuration: &Value,
    ) -> anyhow::Result<Box<dyn IdentityProvider>> {
        self.creations.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(
            !self.failed.load(Ordering::SeqCst),
            "provider creation secret-must-not-leak"
        );
        let value: drasi_plugin_sdk::ConfigValue<String> =
            serde_json::from_value(configuration["password"].clone())?;
        let password = drasi_plugin_sdk::DtoMapper::new()
            .resolve_string(&value)
            .await?;
        Ok(Box::new(
            drasi_lib::identity::PasswordIdentityProvider::new("provider-user", password),
        ))
    }
}
struct Secrets {
    value: Mutex<String>,
    reads: AtomicUsize,
}
#[async_trait]
impl drasi_lib::secret_store::SecretStoreProvider for Secrets {
    async fn get_secret(&self, name: &str) -> anyhow::Result<String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(name == "database-password", "unknown fixture secret");
        Ok(self.value.lock().unwrap().clone())
    }
}

async fn configured_identity_matrix() -> anyhow::Result<()> {
    use crate::{
        management::{HostManagementResources, NativeIdentityConfig},
        PluginRegistry,
    };
    use drasi_lib::management::ManagementResourceResolver;
    use drasi_plugin_sdk::computation::{PluginConfiguration, PluginResolution};
    for fixture in fixtures()? {
        (fixture.reset)();
        let factory = &fixture.plugin.factories()[0];
        let creations = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let descriptor = Arc::new(Descriptor {
            creations: creations.clone(),
            failed: failed.clone(),
        });
        let secrets = Arc::new(Secrets {
            value: Mutex::new("first-secret".into()),
            reads: AtomicUsize::new(0),
        });
        let mut registry = PluginRegistry::new();
        registry.register_identity_provider(descriptor.clone());
        let resolver = HostManagementResources::new(
            &registry,
            drasi_lib::DrasiLib::default_middleware_registry(),
            Some(secrets.clone()),
        )?;
        let specification = ResourceSpecification {
            id: ResourceId::try_new("database-identity")?,
            role: ResourceRole::Identity,
            ownership: ResourceOwnership::Graph,
            binding: "database-identity".into(),
        };
        let recipe = json!({"kind":"nativeIdentity","provider":{
            "kind":"credential-fixture","config_version":"1",
            "config":{"password":{"kind":"Secret","name":"database-password"}}
        }});
        let persisted = serde_json::to_string(&recipe)?;
        let resource = resolver
            .resolve("instance", "graph", &specification, &recipe)
            .await?
            .get::<NativeIdentityResource>()?;
        let mut proxy =
            factory.create_component_with_identity(id(), fixture::configuration(), resource)?;
        assert_eq!(creations.load(Ordering::SeqCst), 0);
        assert_eq!(secrets.reads.load(Ordering::SeqCst), 0);
        proxy.start().await?;
        proxy.stop().await?;
        *secrets.value.lock().unwrap() = "second-secret".into();
        proxy.start().await?;
        proxy.stop().await?;
        assert_eq!(creations.load(Ordering::SeqCst), 2);
        assert_eq!(secrets.reads.load(Ordering::SeqCst), 2);
        assert_eq!((fixture.probe)(fixture::LAST_SECRET), 2);
        failed.store(true, Ordering::SeqCst);
        let error = proxy.start().await.unwrap_err();
        assert!(!format!("{error:#}").contains("secret-must-not-leak"));
        assert_eq!((fixture.probe)(fixture::AUTHENTICATED), 2);
        proxy.stop().await?;

        let configuration: NativeIdentityConfig =
            serde_json::from_value(json!({"provider":recipe["provider"]}))?;
        let services =
            LegacyPluginServices::empty("instance").with_secret_store(Some(secrets.clone()));
        let injected = Provider::new();
        let resource = NativeIdentityResource::configured(
            descriptor,
            configuration.provider.clone(),
            PluginResolution::from_services(&services),
        )?
        .with_injected_provider(Arc::new(injected.clone()));
        let mut proxy = factory.create_component_with_identity(
            id(),
            fixture::configuration(),
            Arc::new(resource),
        )?;
        proxy.start().await?;
        proxy.stop().await?;
        assert_eq!(
            creations.load(Ordering::SeqCst),
            3,
            "injected provider must bypass even a failing configured provider"
        );
        assert_eq!(injected.reads.load(Ordering::SeqCst), 1);
        injected.failed.store(true, Ordering::SeqCst);
        let starts = (fixture.probe)(fixture::AUTHENTICATED);
        assert!(proxy.start().await.is_err());
        assert_eq!((fixture.probe)(fixture::AUTHENTICATED), starts);
        assert_eq!(
            creations.load(Ordering::SeqCst),
            3,
            "failed injection must not fall back to configured credentials"
        );
        proxy.stop().await?;
        assert_eq!(serde_json::to_string(&recipe)?, persisted);
        assert!(!format!("{configuration:?}").contains("database-password"));
        let mut wrong_role = specification.clone();
        wrong_role.role = ResourceRole::Checkpoint;
        assert!(configuration
            .validate(&wrong_role, &BTreeMap::new())
            .is_err());
        assert!(configuration
            .validate(
                &specification,
                &BTreeMap::from([(ResourceId::try_new("other")?, ResourceRole::SecretStore)])
            )
            .is_err());
        let unknown = NativeIdentityConfig {
            provider: PluginConfiguration::new("not-loaded", "1", json!({})),
        };
        assert!(unknown
            .resolve(
                "instance",
                &specification,
                &BTreeMap::new(),
                &registry.computation_identity_providers(),
                None
            )
            .is_err());
        let mut component = factory.specification(id(), fixture::configuration())?;
        component
            .dependencies
            .insert("identity".into(), vec![specification.id.clone()]);
        let requirement = &factory.descriptor().dependencies["identity"];
        assert_eq!(requirement.role, ResourceRole::Identity);
        assert_eq!(requirement.minimum, 0);
        assert_eq!(requirement.maximum, Some(1));
        assert_eq!(
            requirement.concrete_type,
            Some(std::any::TypeId::of::<NativeIdentityResource>())
        );
        factory.validate(&component)?;
        component
            .dependencies
            .get_mut("identity")
            .unwrap()
            .push(specification.id.clone());
        assert!(factory.validate(&component).is_err());
        component.dependencies.clear();
        component
            .dependencies
            .insert("unknown".into(), vec![specification.id.clone()]);
        assert!(factory.validate(&component).is_err());
        assert!(specification.role == ResourceRole::Identity);
    }
    Ok(())
}
