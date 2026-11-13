// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use async_trait::async_trait;
use drasi_lib::{computation::v1::*, management::*, DrasiLib, Query};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[path = "managed_ordinary_identity/updates.rs"]
mod updates;

fn name(value: &str) -> ComponentName {
    ComponentName::try_new(value).expect("managed ordinary identity fixture")
}

#[derive(Default, Serialize, Deserialize)]
struct Document {
    current: CommittedConfiguration,
    receipts: BTreeMap<String, (DesiredInstance, AcceptanceReceipt)>,
    snapshots: BTreeMap<String, CommittedConfiguration>,
}

fn load(path: &Path) -> anyhow::Result<Document> {
    Ok(serde_yaml::from_slice(&std::fs::read(path)?)?)
}

fn save(path: &Path, value: &Document) -> anyhow::Result<()> {
    use std::io::Write;
    let staging = path.with_extension("next");
    let mut file = std::fs::File::create(&staging)?;
    file.write_all(serde_yaml::to_string(value)?.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(staging, path)?;
    Ok(())
}

struct OwnedFile(PathBuf);
impl OwnedFile {
    fn new() -> anyhow::Result<Self> {
        let path = PathBuf::from(format!(".ordinary-identity-{}.yaml", uuid::Uuid::new_v4()));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        save(&path, &Document::default())?;
        Ok(Self(path))
    }
}
impl Drop for OwnedFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(self.0.with_extension("next"));
    }
}

struct FileStore {
    path: PathBuf,
    writes: Mutex<()>,
    owned: Arc<AtomicBool>,
    lose_reply: AtomicBool,
    reject_commit: AtomicBool,
    reject_complete: AtomicBool,
    lose_complete_reply: AtomicBool,
}
impl FileStore {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            writes: Mutex::new(()),
            owned: Arc::new(AtomicBool::new(false)),
            lose_reply: AtomicBool::new(false),
            reject_commit: AtomicBool::new(false),
            reject_complete: AtomicBool::new(false),
            lose_complete_reply: AtomicBool::new(false),
        }
    }
}
struct Session(Arc<FileStore>);
struct Provider(Arc<FileStore>);

#[async_trait]
impl ConfigurationStore for Provider {
    fn guarantees(&self) -> ConfigurationStoreGuarantees {
        ConfigurationStoreGuarantees::DURABLE
    }

    async fn open(&self, _: &str) -> anyhow::Result<Arc<dyn ConfigurationSession>> {
        anyhow::ensure!(
            !self.0.owned.swap(true, Ordering::SeqCst),
            "file already owned"
        );
        Ok(Arc::new(Session(self.0.clone())))
    }
}

#[async_trait]
impl ConfigurationSession for Session {
    async fn load(&self) -> anyhow::Result<CommittedConfiguration> {
        let _lock = self
            .0
            .writes
            .lock()
            .expect("managed ordinary identity fixture");
        Ok(load(&self.0.path)?.current)
    }
    async fn commit(
        &self,
        expected: u64,
        request: &str,
        desired: &DesiredInstance,
        creation: Option<&AcceptedComponentCreation>,
        updates: &[AcceptedComponentUpdate],
    ) -> anyhow::Result<AcceptanceReceipt> {
        let _lock = self
            .0
            .writes
            .lock()
            .expect("managed ordinary identity fixture");
        anyhow::ensure!(
            !self.0.reject_commit.swap(false, Ordering::SeqCst),
            "fixture rejected the configuration write"
        );
        let mut document = load(&self.0.path)?;
        if let Some((prior, receipt)) = document.receipts.get(request) {
            anyhow::ensure!(
                prior == desired
                    && receipt.creation.as_ref() == creation
                    && receipt.updates == updates,
                ManagementError::RequestConflict
            );
            return Ok(receipt.clone());
        }
        anyhow::ensure!(
            document.current.revision == expected,
            ManagementError::RevisionConflict {
                expected,
                actual: document.current.revision
            }
        );
        let mut pending = document.current.pending_updates.clone();
        for update in updates {
            anyhow::ensure!(
                !pending
                    .iter()
                    .any(|old| old.component_id == update.component_id),
                "component already has an incomplete update"
            );
            pending.push(update.clone());
        }
        pending.sort_by(|left, right| left.component_id.cmp(&right.component_id));
        if document.current.desired != *desired {
            document.current = CommittedConfiguration {
                revision: expected + 1,
                desired: desired.clone(),
                pending_updates: pending,
            };
        } else {
            document.current.pending_updates = pending;
        }
        let receipt = AcceptanceReceipt {
            request_id: request.into(),
            revision: document.current.revision,
            durable: true,
            creation: creation.cloned(),
            updates: updates.to_vec(),
        };
        document
            .receipts
            .insert(request.into(), (desired.clone(), receipt.clone()));
        save(&self.0.path, &document)?;
        anyhow::ensure!(
            !self.0.lose_reply.swap(false, Ordering::SeqCst),
            "lost commit acknowledgement"
        );
        Ok(receipt)
    }
    async fn complete_updates(
        &self,
        expected: u64,
        updates: &[ComponentUpdateCompletion],
    ) -> anyhow::Result<CommittedConfiguration> {
        let _lock = self
            .0
            .writes
            .lock()
            .expect("managed ordinary identity fixture");
        anyhow::ensure!(
            !self.0.reject_complete.load(Ordering::Acquire),
            "fixture rejected update completion"
        );
        let mut document = load(&self.0.path)?;
        anyhow::ensure!(
            document.current.revision == expected,
            ManagementError::RevisionConflict {
                expected,
                actual: document.current.revision
            }
        );
        for update in updates {
            if let Some(pending) = document
                .current
                .pending_updates
                .iter()
                .find(|pending| pending.component_id == update.component_id)
            {
                anyhow::ensure!(
                    pending.operation_id == update.operation_id,
                    ManagementError::RequestConflict
                );
            }
        }
        document.current.pending_updates.retain(|pending| {
            !updates.iter().any(|update| {
                update.component_id == pending.component_id
                    && update.operation_id == pending.operation_id
            })
        });
        save(&self.0.path, &document)?;
        anyhow::ensure!(
            !self.0.lose_complete_reply.swap(false, Ordering::AcqRel),
            "lost update completion acknowledgement"
        );
        Ok(document.current)
    }
    async fn receipt(&self, request: &str) -> anyhow::Result<Option<AcceptanceReceipt>> {
        let _lock = self
            .0
            .writes
            .lock()
            .expect("managed ordinary identity fixture");
        Ok(load(&self.0.path)?
            .receipts
            .get(request)
            .map(|(_, receipt)| receipt.clone()))
    }
    async fn snapshot(&self, name: &str) -> anyhow::Result<CommittedConfiguration> {
        let _lock = self
            .0
            .writes
            .lock()
            .expect("managed ordinary identity fixture");
        let mut document = load(&self.0.path)?;
        let current = document.current.clone();
        anyhow::ensure!(
            !document.snapshots.contains_key(name),
            ManagementError::SnapshotExists(name.into())
        );
        document.snapshots.insert(name.into(), current.clone());
        save(&self.0.path, &document)?;
        Ok(current)
    }
    async fn load_snapshot(&self, name: &str) -> anyhow::Result<Option<CommittedConfiguration>> {
        let _lock = self
            .0
            .writes
            .lock()
            .expect("managed ordinary identity fixture");
        Ok(load(&self.0.path)?.snapshots.get(name).cloned())
    }
    async fn close(&self) -> anyhow::Result<()> {
        self.0.owned.store(false, Ordering::SeqCst);
        Ok(())
    }
}

struct Resolver {
    path: PathBuf,
    sources: AtomicUsize,
    reactions: AtomicUsize,
    fail_source: AtomicBool,
    source_handles: Mutex<BTreeMap<ComponentId, drasi_source_application::ApplicationSourceHandle>>,
    reaction_handles:
        Mutex<BTreeMap<ComponentId, drasi_reaction_application::ApplicationReactionHandle>>,
}
impl Resolver {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            sources: AtomicUsize::new(0),
            reactions: AtomicUsize::new(0),
            fail_source: AtomicBool::new(false),
            source_handles: Mutex::new(BTreeMap::new()),
            reaction_handles: Mutex::new(BTreeMap::new()),
        }
    }
    fn assert_committed(&self, record: &OrdinaryComponentRecord) -> anyhow::Result<()> {
        let document = load(&self.path)?;
        assert!(document
            .current
            .desired
            .ordinary
            .iter()
            .any(|stored| stored == record));
        assert_eq!(
            document.current.desired,
            document.current.desired.clone().normalized()?
        );
        Ok(())
    }
}

#[async_trait]
impl OrdinaryComponentResolver for Resolver {
    fn plan_update(
        &self,
        _: &str,
        current: &OrdinaryComponentRecord,
        proposed: &OrdinaryComponentRecord,
    ) -> anyhow::Result<ComponentUpdatePlan> {
        anyhow::ensure!(
            std::mem::discriminant(&current.component)
                == std::mem::discriminant(&proposed.component),
            "fixture cannot change its component role"
        );
        Ok(ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Preserve,
        })
    }
    async fn create_source(
        &self,
        _: &str,
        record: &OrdinaryComponentRecord,
        _: Option<&ComponentUpdateDirective>,
    ) -> anyhow::Result<(Box<dyn drasi_lib::Source>, HashMap<String, String>)> {
        self.assert_committed(record)?;
        self.sources.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(
            !self.fail_source.load(Ordering::SeqCst),
            "fixture source is unavailable"
        );
        let OrdinaryComponent::Source { configuration, .. } = &record.component else {
            anyhow::bail!("not a source");
        };
        let properties = configuration
            .as_object()
            .expect("managed ordinary identity fixture")
            .clone()
            .into_iter()
            .collect();
        let (source, handle) = drasi_source_application::ApplicationSource::new(
            record.id.clone(),
            drasi_source_application::ApplicationSourceConfig {
                properties,
                durability: None,
            },
        )?;
        self.source_handles
            .lock()
            .expect("managed ordinary identity fixture")
            .insert(record.id.clone(), handle);
        Ok((
            Box::new(source),
            HashMap::from([("owner".into(), "yaml".into())]),
        ))
    }
    async fn create_reaction(
        &self,
        _: &str,
        record: &OrdinaryComponentRecord,
        inputs: &[ComponentId],
        _: Option<&ComponentUpdateDirective>,
    ) -> anyhow::Result<(Box<dyn drasi_lib::Reaction>, HashMap<String, String>)> {
        self.assert_committed(record)?;
        self.reactions.fetch_add(1, Ordering::SeqCst);
        let (reaction, handle) = drasi_reaction_application::ApplicationReaction::new(
            record.id.clone(),
            inputs
                .iter()
                .cloned()
                .map(drasi_lib::config::ComponentReference::by_id)
                .collect(),
        );
        self.reaction_handles
            .lock()
            .expect("managed ordinary identity fixture")
            .insert(record.id.clone(), handle);
        Ok((Box::new(reaction), HashMap::new()))
    }
}

fn source(label: &str) -> OrdinaryComponentDefinition {
    OrdinaryComponentDefinition {
        id: None,
        name: name(label),
        auto_start: false,
        component: OrdinaryComponent::Source {
            kind: "application".into(),
            configuration: serde_json::json!({"origin": "saved-file"}),
        },
    }
}
fn query() -> anyhow::Result<OrdinaryComponentDefinition> {
    OrdinaryComponentDefinition::query(
        Query::cypher(
            drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
        )
        .query("MATCH (n) RETURN n")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("orders").expect("fixture component name"),
        )
        .enable_bootstrap(false)
        .auto_start(false)
        .build()?,
    )
}
fn reaction() -> anyhow::Result<OrdinaryComponentDefinition> {
    Ok(OrdinaryComponentDefinition {
        id: None,
        name: name("consumer"),
        auto_start: false,
        component: OrdinaryComponent::Reaction {
            kind: "application".into(),
            configuration: serde_json::json!({}),
            inputs: vec![drasi_lib::config::ComponentReference::by_name(
                drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
            )],
        },
    })
}
async fn core(store: Arc<FileStore>, resolver: Arc<Resolver>) -> anyhow::Result<DrasiLib> {
    Ok(DrasiLib::builder()
        .with_id("yaml-owner")
        .with_construction_timeout_secs(2)
        .with_resource_construction_timeout_secs(3)
        .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
        .with_ordinary_component_resolver(resolver)
        .with_configuration_store(Arc::new(Provider(store)))
        .build()
        .await?)
}
async fn settle(core: &DrasiLib) -> anyhow::Result<()> {
    let state =
        tokio::time::timeout(Duration::from_secs(10), core.reconcile_desired_state()).await??;
    anyhow::ensure!(
        state.error.is_none() && state.resource_errors.is_empty(),
        "reconciliation failed: {state:?}"
    );
    for (id, component) in core.computation_control()?.observed().components.iter() {
        anyhow::ensure!(
            component.failure.is_none(),
            "component {id} failed: {:?}",
            component.failure
        );
    }
    Ok(())
}

#[tokio::test]
async fn pending_records_and_bindings_commit_before_construction_and_restore_from_file(
) -> anyhow::Result<()> {
    let file = OwnedFile::new()?;
    let store = Arc::new(FileStore::new(file.0.clone()));
    let resolver = Arc::new(Resolver::new(file.0.clone()));
    let core = core(store.clone(), resolver.clone()).await?;
    let q = core
        .create_managed_ordinary_component(0, "query", query()?)
        .await?;
    let query_id = q
        .creation
        .as_ref()
        .expect("managed ordinary identity fixture")
        .components[0]
        .id
        .clone();
    settle(&core).await?;
    assert!(core.component_by_id(&query_id)?.observed()?.waiting_for.iter().any(|wait|
        matches!(wait, DependencyWait::ComponentName { name } if name.as_str() == "orders")));
    assert_eq!(resolver.sources.load(Ordering::SeqCst), 0);
    assert_eq!(load(&file.0)?.current.desired.ordinary.len(), 1);
    store.lose_reply.store(true, Ordering::SeqCst);
    assert!(core
        .create_managed_ordinary_component(q.revision, "source", source("orders"))
        .await
        .is_err());
    let s = core
        .create_managed_ordinary_component(q.revision, "source", source("orders"))
        .await?;
    let source_id = s
        .creation
        .as_ref()
        .expect("managed ordinary identity fixture")
        .components[0]
        .id
        .clone();
    settle(&core).await?;
    tokio::time::timeout(
        Duration::from_secs(5),
        core.component_by_id(&query_id)?.wait_created(),
    )
    .await??;
    let r = core
        .create_managed_ordinary_component(s.revision, "reaction", reaction()?)
        .await?;
    let reaction_id = r
        .creation
        .as_ref()
        .expect("managed ordinary identity fixture")
        .components[0]
        .id
        .clone();
    settle(&core).await?;
    tokio::time::timeout(
        Duration::from_secs(5),
        core.component_by_id(&reaction_id)?.wait_created(),
    )
    .await??;
    assert_eq!(
        core.get_query_config_by_id(&query_id).await?.inputs[0]
            .component
            .as_id(),
        Some(&source_id)
    );
    assert!(!core.allow_external_component_ids());
    let before = resolver.sources.load(Ordering::SeqCst);
    let before_reactions = resolver.reactions.load(Ordering::SeqCst);
    let query_generation = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
        )?
        .generation();
    let mut desired = core.desired_configuration()?.desired;
    for (id, label) in [
        (&source_id, "purchases"),
        (&query_id, "view-renamed"),
        (&reaction_id, "consumer-renamed"),
    ] {
        desired
            .ordinary
            .iter_mut()
            .find(|record| &record.id == id)
            .expect("managed ordinary identity fixture")
            .name = name(label);
    }
    let renamed = core
        .apply_desired_state(r.revision, "rename", desired)
        .await?;
    settle(&core).await?;
    assert_eq!(resolver.sources.load(Ordering::SeqCst), before);
    assert_eq!(resolver.reactions.load(Ordering::SeqCst), before_reactions);
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("purchases").expect("fixture component name")
        )?
        .id(),
        &source_id
    );
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("view-renamed")
                .expect("fixture component name")
        )?
        .generation(),
        query_generation
    );
    assert_eq!(
        core.get_query_config_by_id(&query_id).await?.name,
        drasi_core_types::ComponentName::try_new("view-renamed").expect("fixture component name")
    );
    core.create_managed_ordinary_component(renamed.revision, "reuse-name", source("orders"))
        .await?;
    settle(&core).await?;
    assert_eq!(
        core.get_query_config_by_id(&query_id).await?.inputs[0]
            .component
            .as_id(),
        Some(&source_id)
    );
    core.shutdown().await?;
    let fresh = core_from_file(&file.0).await?;
    assert!(!fresh.allow_external_component_ids());
    assert_eq!(
        fresh
            .component_by_name(
                &drasi_core_types::ComponentName::try_new("purchases")
                    .expect("fixture component name")
            )?
            .id(),
        &source_id
    );
    assert_eq!(
        fresh
            .component_by_name(
                &drasi_core_types::ComponentName::try_new("view-renamed")
                    .expect("fixture component name")
            )?
            .id(),
        &query_id
    );
    assert_eq!(
        fresh
            .component_by_name(
                &drasi_core_types::ComponentName::try_new("consumer-renamed")
                    .expect("fixture component name")
            )?
            .id(),
        &reaction_id
    );
    assert_eq!(
        fresh.get_query_config_by_id(&query_id).await?.inputs[0]
            .component
            .as_id(),
        Some(&source_id)
    );
    fresh.shutdown().await?;
    Ok(())
}

async fn core_from_file(path: &Path) -> anyhow::Result<DrasiLib> {
    core(
        Arc::new(FileStore::new(path.to_owned())),
        Arc::new(Resolver::new(path.to_owned())),
    )
    .await
}

#[tokio::test]
async fn historical_owner_records_restore_deleted_ids_but_copies_cannot_claim_restoration(
) -> anyhow::Result<()> {
    let file = OwnedFile::new()?;
    let core = core_from_file(&file.0).await?;
    let receipt = core
        .create_managed_ordinary_component(0, "create", source("orders"))
        .await?;
    settle(&core).await?;
    let id = receipt
        .creation
        .as_ref()
        .expect("managed ordinary identity fixture")
        .components[0]
        .id
        .clone();
    let old = core.component_by_id(&id)?;
    old.wait_created().await?;
    let saved = core.snapshot_desired_configuration("saved").await?;
    let removed = core
        .apply_desired_state(receipt.revision, "remove", DesiredInstance::default())
        .await?;
    settle(&core).await?;
    assert!(old.observed().is_err());
    let error = core
        .apply_desired_state(removed.revision, "import", saved.desired.clone())
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<GraphError>(),
        Some(GraphError::ExternalComponentId { .. })
    ));
    assert_eq!(
        core.create_managed_ordinary_component(0, "create", source("orders"))
            .await?,
        receipt
    );
    assert!(core.component_by_id(&id).is_err());
    core.restore_configuration_snapshot("saved", removed.revision, "restore")
        .await?;
    settle(&core).await?;
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("orders").expect("fixture component name")
        )?
        .id(),
        &id
    );
    assert!(old.observed().is_err());
    let mut explicit = source("external");
    explicit.id = Some(ComponentId::generate());
    assert!(core
        .create_managed_ordinary_component(
            core.desired_configuration()?.revision,
            "external",
            explicit
        )
        .await
        .is_err());
    assert!(core
        .create_source_async(
            drasi_core_types::ComponentName::try_new("bypass").expect("fixture component name"),
            None,
            false,
            |id| async move {
                let (source, _) = drasi_source_application::ApplicationSource::new(
                    id.clone(),
                    drasi_source_application::ApplicationSourceConfig {
                        properties: HashMap::new(),
                        durability: None,
                    },
                )?;
                Ok((source, HashMap::new()))
            }
        )
        .await
        .is_err());
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn actual_loaded_records_bind_names_durably_before_open_constructs_them() -> anyhow::Result<()>
{
    let file = OwnedFile::new()?;
    let mut document = load(&file.0)?;
    let source_id = ComponentId::generate();
    let query_id = ComponentId::generate();
    let reaction_id = ComponentId::generate();
    document.current.desired.ordinary = vec![
        source("orders").with_assigned_id(source_id.clone()),
        query()?.with_assigned_id(query_id.clone()),
        reaction()?.with_assigned_id(reaction_id.clone()),
    ];
    save(&file.0, &document)?;
    let core = core_from_file(&file.0).await?;
    settle(&core).await?;
    assert!(load(&file.0)?.current.revision > 0);
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("orders").expect("fixture component name")
        )?
        .id(),
        &source_id
    );
    assert_eq!(
        core.get_query_config_by_id(&query_id).await?.inputs[0]
            .component
            .as_id(),
        Some(&source_id)
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        core.component_by_id(&reaction_id)?.wait_created(),
    )
    .await??;
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn binding_write_with_lost_reply_uses_confirmed_current_records() -> anyhow::Result<()> {
    let file = OwnedFile::new()?;
    let mut document = load(&file.0)?;
    document.current.desired.ordinary = running_records()?;
    save(&file.0, &document)?;
    let store = Arc::new(FileStore::new(file.0.clone()));
    store.lose_reply.store(true, Ordering::SeqCst);
    let resolver = Arc::new(Resolver::new(file.0.clone()));
    let core = core(store, resolver.clone()).await?;
    settle(&core).await?;
    assert_eq!(resolver.sources.load(Ordering::SeqCst), 1);
    assert!(load(&file.0)?.current.revision > 0);
    core.start().await?;
    deliver(&core, &resolver, "confirmed-binding").await?;
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn rejected_binding_write_constructs_nothing_and_releases_the_file_owner(
) -> anyhow::Result<()> {
    let file = OwnedFile::new()?;
    let mut document = load(&file.0)?;
    document.current.desired.ordinary = running_records()?;
    save(&file.0, &document)?;
    let store = Arc::new(FileStore::new(file.0.clone()));
    store.reject_commit.store(true, Ordering::SeqCst);
    let resolver = Arc::new(Resolver::new(file.0.clone()));
    assert!(core(store.clone(), resolver.clone()).await.is_err());
    assert_eq!(resolver.sources.load(Ordering::SeqCst), 0);
    assert_eq!(resolver.reactions.load(Ordering::SeqCst), 0);
    assert!(!store.owned.load(Ordering::SeqCst));
    assert_eq!(load(&file.0)?.current, document.current);
    let reopened = core(store, resolver).await?;
    settle(&reopened).await?;
    reopened.shutdown().await?;
    Ok(())
}

#[test]
fn ordinary_records_require_uuid_identity_and_have_no_restore_flag() -> anyhow::Result<()> {
    let mut input = serde_json::to_value(source("orders"))?;
    input
        .as_object_mut()
        .expect("managed ordinary identity fixture")
        .remove("id");
    assert!(
        serde_json::from_value::<OrdinaryComponentDefinition>(input.clone())?
            .id
            .is_none()
    );
    assert!(serde_json::from_value::<OrdinaryComponentRecord>(input.clone()).is_err());
    input["id"] = serde_json::json!(ComponentId::generate());
    input["trusted"] = true.into();
    assert!(serde_json::from_value::<OrdinaryComponentRecord>(input).is_err());
    Ok(())
}

async fn deliver(core: &DrasiLib, resolver: &Resolver, row: &str) -> anyhow::Result<()> {
    let source = core.component_by_name(
        &drasi_core_types::ComponentName::try_new("orders").expect("fixture component name"),
    )?;
    let query = core.component_by_name(
        &drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
    )?;
    let reaction = core.component_by_name(
        &drasi_core_types::ComponentName::try_new("consumer").expect("fixture component name"),
    )?;
    tokio::time::timeout(Duration::from_secs(5), reaction.wait_started()).await??;
    let input = resolver
        .source_handles
        .lock()
        .expect("managed ordinary identity fixture")[source.id()]
    .clone();
    let output = resolver
        .reaction_handles
        .lock()
        .expect("managed ordinary identity fixture")[reaction.id()]
    .clone();
    let mut results = output
        .take_receiver()
        .await
        .expect("managed ordinary identity fixture");
    input
        .send_node_insert(
            row,
            vec!["Item"],
            drasi_source_application::PropertyMapBuilder::new()
                .with_integer("value", 7)
                .build(),
        )
        .await?;
    let result = tokio::time::timeout(Duration::from_secs(5), results.recv())
        .await?
        .expect("managed ordinary identity fixture");
    assert_eq!(result.query_id, query.id().clone());
    Ok(())
}

fn running_records() -> anyhow::Result<Vec<OrdinaryComponentRecord>> {
    [source("orders"), query()?, reaction()?]
        .into_iter()
        .map(|mut definition| {
            definition.auto_start = true;
            Ok(definition.with_assigned_id(ComponentId::generate()))
        })
        .collect()
}

#[tokio::test]
async fn configured_file_restart_in_an_independent_process_delivers_with_recorded_ids(
) -> anyhow::Result<()> {
    let file = OwnedFile::new()?;
    let mut document = load(&file.0)?;
    document.current.desired.ordinary = running_records()?;
    save(&file.0, &document)?;
    let resolver = Arc::new(Resolver::new(file.0.clone()));
    let core = core(Arc::new(FileStore::new(file.0.clone())), resolver.clone()).await?;
    settle(&core).await?;
    core.start().await?;
    deliver(&core, &resolver, "before-restart").await?;
    core.shutdown().await?;
    let committed = load(&file.0)?.current;
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "file_restart_child", "--nocapture"])
        .env("DRASI_IDENTITY_RESTART_FILE", &file.0)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while child.try_wait()?.is_none() {
        if tokio::time::Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            anyhow::bail!("independent file restart timed out");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let output = child.wait_with_output()?;
    anyhow::ensure!(
        output.status.success(),
        "child restart failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(load(&file.0)?.current, committed);
    Ok(())
}

#[tokio::test]
async fn file_restart_child() -> anyhow::Result<()> {
    let Some(path) = std::env::var_os("DRASI_IDENTITY_RESTART_FILE").map(PathBuf::from) else {
        return Ok(());
    };
    let committed = load(&path)?.current;
    let resolver = Arc::new(Resolver::new(path.clone()));
    let core = core(Arc::new(FileStore::new(path)), resolver.clone()).await?;
    settle(&core).await?;
    assert!(!core.allow_external_component_ids());
    for record in &committed.desired.ordinary {
        assert_eq!(core.component_by_name(&record.name)?.id(), &record.id);
    }
    core.start().await?;
    deliver(&core, &resolver, "after-restart").await?;
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn ordinary_record_replacement_rebinds_resources_and_preserves_uuid() -> anyhow::Result<()> {
    let file = OwnedFile::new()?;
    let mut document = load(&file.0)?;
    document.current.desired.ordinary = running_records()?;
    save(&file.0, &document)?;
    let resolver = Arc::new(Resolver::new(file.0.clone()));
    let core = core(Arc::new(FileStore::new(file.0.clone())), resolver.clone()).await?;
    settle(&core).await?;
    let old_source = core.component_by_name(
        &drasi_core_types::ComponentName::try_new("orders").expect("fixture component name"),
    )?;
    let old_reaction = core.component_by_name(
        &drasi_core_types::ComponentName::try_new("consumer").expect("fixture component name"),
    )?;
    let query_id = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
        )?
        .id()
        .clone();
    let mut desired = core.desired_configuration()?;
    let mut invalid_removal = desired.desired.clone();
    invalid_removal
        .ordinary
        .retain(|record| &record.id != old_source.id());
    assert!(core
        .apply_desired_state(desired.revision, "referenced-source", invalid_removal)
        .await
        .is_err());
    assert_eq!(load(&file.0)?.current.revision, desired.revision);
    for record in &mut desired.desired.ordinary {
        match &mut record.component {
            OrdinaryComponent::Source { configuration, .. }
            | OrdinaryComponent::Reaction { configuration, .. } => {
                configuration["revision"] = 2.into()
            }
            OrdinaryComponent::Query { .. } => {}
            OrdinaryComponent::ProcessLocal { .. } => {
                anyhow::bail!("durable fixture contains a process-local record")
            }
        }
    }
    core.apply_desired_state(desired.revision, "replace", desired.desired)
        .await?;
    settle(&core).await?;
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("orders").expect("fixture component name")
        )?
        .id(),
        old_source.id()
    );
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("consumer").expect("fixture component name")
        )?
        .id(),
        old_reaction.id()
    );
    assert!(old_source.observed().is_err());
    assert!(old_reaction.observed().is_err());
    assert_eq!(resolver.sources.load(Ordering::SeqCst), 2);
    assert_eq!(resolver.reactions.load(Ordering::SeqCst), 2);
    assert_eq!(
        core.get_query_config_by_id(&query_id).await?.inputs[0]
            .component
            .as_id(),
        Some(old_source.id())
    );
    core.start().await?;
    deliver(&core, &resolver, "after-replacement").await?;
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn pending_and_failed_recipes_remain_real_records_and_retry_under_their_ids(
) -> anyhow::Result<()> {
    let file = OwnedFile::new()?;
    let resolver = Arc::new(Resolver::new(file.0.clone()));
    resolver.fail_source.store(true, Ordering::SeqCst);
    let core = core(Arc::new(FileStore::new(file.0.clone())), resolver.clone()).await?;
    let waiting = core
        .create_managed_ordinary_component(0, "reaction-first", reaction()?)
        .await?;
    let id = waiting
        .creation
        .as_ref()
        .expect("managed ordinary identity fixture")
        .components[0]
        .id
        .clone();
    settle(&core).await?;
    assert!(core
        .component_by_id(&id)?
        .observed()?
        .waiting_for
        .iter()
        .any(
            |wait| matches!(wait, DependencyWait::ComponentName { name } if name.as_str() == "view")
        ));
    assert_eq!(
        load(&file.0)?.current.desired.ordinary[0].component,
        reaction()?.component
    );
    assert!(core.snapshot_configuration().await.is_err());
    let q = core
        .create_managed_ordinary_component(waiting.revision, "query", query()?)
        .await?;
    let s = core
        .create_managed_ordinary_component(q.revision, "source", source("orders"))
        .await?;
    let status = core.reconcile_desired_state().await?;
    assert!(!status.converged);
    let failed = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("orders").expect("fixture component name"),
        )?
        .observed()?;
    assert_eq!(failed.realization, RealizationState::CreationFailed);
    assert!(failed.failure.is_some());
    assert_eq!(load(&file.0)?.current.desired.ordinary.len(), 3);
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("orders").expect("fixture component name")
        )?
        .id(),
        &s.creation
            .expect("managed ordinary identity fixture")
            .components[0]
            .id
    );
    resolver.fail_source.store(false, Ordering::SeqCst);
    settle(&core).await?;
    tokio::time::timeout(
        Duration::from_secs(5),
        core.component_by_id(&id)?.wait_created(),
    )
    .await??;
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("consumer").expect("fixture component name")
        )?
        .id(),
        &id
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn replacement_does_not_silently_reset_existing_strict_recovery_checkpoints(
) -> anyhow::Result<()> {
    let file = OwnedFile::new()?;
    let mut document = load(&file.0)?;
    document.current.desired.ordinary = running_records()?;
    save(&file.0, &document)?;
    let resolver = Arc::new(Resolver::new(file.0.clone()));
    let core = core(Arc::new(FileStore::new(file.0.clone())), resolver.clone()).await?;
    settle(&core).await?;
    core.start().await?;
    deliver(&core, &resolver, "checkpointed").await?;
    core.stop().await?;
    let mut desired = core.desired_configuration()?;
    let source = desired
        .desired
        .ordinary
        .iter_mut()
        .find(|record| matches!(record.component, OrdinaryComponent::Source { .. }))
        .expect("managed ordinary identity fixture");
    let id = source.id.clone();
    if let OrdinaryComponent::Source { configuration, .. } = &mut source.component {
        configuration["revision"] = 2.into();
    }
    core.apply_desired_state(
        desired.revision,
        "replace-after-processing",
        desired.desired,
    )
    .await?;
    settle(&core).await?;
    let error = core.start().await.unwrap_err();
    assert!(error.to_string().contains("Strict recovery policy"));
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("orders").expect("fixture component name")
        )?
        .id(),
        &id
    );
    core.shutdown().await?;
    Ok(())
}

struct NativeService(ComponentDescriptor);

#[async_trait]
impl ComputationComponent for NativeService {
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
impl ComputationService for NativeService {
    async fn run(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

struct NativeFactory(FactoryDescriptor);

#[async_trait]
impl ComponentFactory for NativeFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.0
    }
    fn specification_from_config(
        &self,
        id: ComponentId,
        _: serde_json::Value,
    ) -> anyhow::Result<ComponentSpecification> {
        Ok(ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(id, vec![])?,
            role: ComponentRole::Service,
            completion: None,
            implementation: self.0.implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        })
    }
    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        Ok(ConstructedComponent::service(Box::new(NativeService(
            context.specification.descriptor.clone(),
        ))))
    }
}

#[tokio::test]
async fn native_and_ordinary_records_share_the_owner_without_adopting_each_other(
) -> anyhow::Result<()> {
    let file = OwnedFile::new()?;
    let factory = Arc::new(NativeFactory(FactoryDescriptor {
        implementation: ImplementationIdentity::try_new("identity/native-service", "1")?,
        role: ComponentRole::Service,
        configuration_version: 1,
        configuration: ConfigurationSchema {
            fields: BTreeMap::new(),
            allow_additional: false,
        },
        dependencies: BTreeMap::new(),
    }));
    let mut factories = FactoryRegistry::default();
    factories.register(factory.clone())?;
    let store = Arc::new(FileStore::new(file.0.clone()));
    let resolver = Arc::new(Resolver::new(file.0.clone()));
    let core = DrasiLib::builder()
        .with_id("mixed-owner")
        .with_component_factories(factories.clone())
        .with_ordinary_component_resolver(resolver)
        .with_configuration_store(Arc::new(Provider(store)))
        .build()
        .await?;
    let native = core
        .create_managed_component(
            0,
            "native",
            ComponentDefinition {
                name: name("native"),
                id: None,
                implementation: factory.0.implementation.clone(),
                configuration: serde_json::json!({}),
                dependencies: BTreeMap::new(),
                streams: BTreeMap::new(),
                lifecycle: LifecyclePolicy { auto_start: false },
            },
        )
        .await?;
    let ordinary = core
        .create_managed_ordinary_component(native.revision, "ordinary", source("orders"))
        .await?;
    settle(&core).await?;
    let native_id = native
        .creation
        .expect("managed ordinary identity fixture")
        .components[0]
        .id
        .clone();
    let ordinary_id = ordinary
        .creation
        .expect("managed ordinary identity fixture")
        .components[0]
        .id
        .clone();
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("native").expect("fixture component name")
        )?
        .id(),
        &native_id
    );
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("orders").expect("fixture component name")
        )?
        .id(),
        &ordinary_id
    );
    core.shutdown().await?;
    let reopened = DrasiLib::builder()
        .with_id("mixed-owner")
        .with_component_factories(factories)
        .with_ordinary_component_resolver(Arc::new(Resolver::new(file.0.clone())))
        .with_configuration_store(Arc::new(Provider(Arc::new(FileStore::new(file.0.clone())))))
        .build()
        .await?;
    settle(&reopened).await?;
    assert_eq!(
        reopened
            .component_by_name(
                &drasi_core_types::ComponentName::try_new("native")
                    .expect("fixture component name")
            )?
            .id(),
        &native_id
    );
    assert_eq!(
        reopened
            .component_by_name(
                &drasi_core_types::ComponentName::try_new("orders")
                    .expect("fixture component name")
            )?
            .id(),
        &ordinary_id
    );
    reopened.shutdown().await?;
    Ok(())
}

struct NativeResources;

#[async_trait]
impl ManagementResourceResolver for NativeResources {
    async fn resolve(
        &self,
        _: &str,
        _: &str,
        specification: &ResourceSpecification,
        configuration: &serde_json::Value,
    ) -> anyhow::Result<ResourceHandle> {
        anyhow::ensure!(
            configuration["fail"] != true,
            "fixture resource is unavailable"
        );
        Ok(ResourceHandle::new(specification.role, Arc::new(())))
    }
}

#[tokio::test]
async fn native_resource_only_failures_remain_visible_with_ordinary_management_enabled(
) -> anyhow::Result<()> {
    let file = OwnedFile::new()?;
    let core = DrasiLib::builder()
        .with_id("resource-owner")
        .with_management_resources(Arc::new(NativeResources))
        .with_configuration_store(Arc::new(Provider(Arc::new(FileStore::new(file.0.clone())))))
        .build()
        .await?;
    let resource = ResourceId::try_new("metadata-only")?;
    let mut desired = DesiredInstance::default();
    desired.topology.resources.push(ResourceSpecification {
        id: resource.clone(),
        role: ResourceRole::Component,
        ownership: ResourceOwnership::Borrowed,
        binding: "fixture".into(),
    });
    desired
        .topology
        .resource_configurations
        .insert(resource.clone(), serde_json::json!({"fail":true}));
    let receipt = core
        .apply_desired_state(0, "resource", desired.clone())
        .await?;
    let failed = core.reconcile_desired_state().await?;
    assert!(!failed.converged);
    let status = core.management_status().await?;
    assert!(!status.converged);
    assert!(status.resource_errors.contains_key(&resource));
    desired
        .topology
        .resource_configurations
        .insert(resource, serde_json::json!({"fail":false}));
    core.apply_desired_state(receipt.revision, "repair", desired)
        .await?;
    settle(&core).await?;
    assert!(core.management_status().await?.converged);
    core.shutdown().await?;
    Ok(())
}
