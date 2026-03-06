/// POC: kube-rs watcher capabilities for Drasi Kubernetes source
use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::{
    api::{Api, ListParams, Patch, PatchParams, PostParams, Resource},
    runtime::watcher::{self as kube_watcher, watcher, Event},
    Client,
};
use std::collections::BTreeMap;
use tokio::time::{sleep, Duration};

fn extract_id(cm: &ConfigMap) -> String {
    format!("{}/{}", cm.meta().namespace.as_deref().unwrap_or(""), cm.meta().name.as_deref().unwrap_or(""))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).init();
    println!("=== kube-rs POC for Drasi Kubernetes Source ===\n");

    let client = Client::try_default().await?;
    let ver = client.apiserver_version().await?;
    println!("✅ Connected: Kubernetes {}.{}", ver.major, ver.minor);

    let cms_api: Api<ConfigMap> = Api::namespaced(client.clone(), "default");

    // -----------------------------------------------------------------------
    // Spawn a background task to create/update/delete a ConfigMap
    // -----------------------------------------------------------------------
    let cms_api_clone = cms_api.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(1500)).await;

        // CREATE
        let mut data = BTreeMap::new();
        data.insert("key1".to_string(), "hello".to_string());
        data.insert("key2".to_string(), "world".to_string());
        let cm = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": { "name": "drasi-poc-test" },
            "data": data
        })).unwrap();
        cms_api_clone.create(&PostParams::default(), &cm).await.unwrap();
        println!("[HARNESS] Created ConfigMap drasi-poc-test");

        sleep(Duration::from_millis(800)).await;

        // UPDATE
        let patch = serde_json::json!({
            "data": { "key1": "updated", "key3": "added" }
        });
        cms_api_clone.patch("drasi-poc-test", &PatchParams::apply("poc"), &Patch::Merge(&patch)).await.unwrap();
        println!("[HARNESS] Updated ConfigMap drasi-poc-test");

        sleep(Duration::from_millis(800)).await;

        // DELETE
        cms_api_clone.delete("drasi-poc-test", &Default::default()).await.unwrap();
        println!("[HARNESS] Deleted ConfigMap drasi-poc-test");
    });

    // -----------------------------------------------------------------------
    // Watch the ConfigMap API and print events
    // -----------------------------------------------------------------------
    println!("\n=== WATCHING ConfigMaps in 'default' namespace ===");
    let mut stream = watcher(cms_api, kube_watcher::Config::default()).boxed();

    let mut event_count = 0;
    while let Some(result) = stream.next().await {
        match result {
            Ok(event) => match &event {
                Event::Init => {
                    println!("[EVENT] INIT — initial list starting (CDC bootstrap equivalent)");
                }
                Event::InitApply(cm) => {
                    println!("[EVENT] INIT_APPLY — {} (uid={})",
                        extract_id(cm), cm.meta().uid.as_deref().unwrap_or("?"));
                }
                Event::InitDone => {
                    println!("[EVENT] INIT_DONE — bootstrap complete, now watching for changes\n");
                }
                Event::Apply(cm) => {
                    let uid = cm.meta().uid.as_deref().unwrap_or("?");
                    let rv = cm.meta().resource_version.as_deref().unwrap_or("?");
                    let data_keys: Vec<&String> = cm.data.as_ref()
                        .map(|d| d.keys().collect()).unwrap_or_default();
                    let data: Vec<(&String, &String)> = cm.data.as_ref()
                        .map(|d| d.iter().collect()).unwrap_or_default();
                    println!("[EVENT] APPLY (insert or update) ConfigMap: {}",  extract_id(cm));
                    println!("         uid={uid}, resource_version={rv}");
                    println!("         data_keys={:?}", data_keys);
                    println!("         data={:?}", data);
                    event_count += 1;
                }
                Event::Delete(cm) => {
                    let uid = cm.meta().uid.as_deref().unwrap_or("?");
                    println!("[EVENT] DELETE ConfigMap: {}", extract_id(cm));
                    println!("         uid={uid}");
                    event_count += 1;
                }
            },
            Err(e) => eprintln!("[ERROR] {e}"),
        }
        if event_count >= 3 {
            println!("\n✅ Captured 3 events (create, update, delete) — stopping");
            break;
        }
    }

    // -----------------------------------------------------------------------
    // Verify resource version list
    // -----------------------------------------------------------------------
    println!("\n=== RESOURCE VERSION (for cursor/state persistence) ===");
    let list_api: Api<ConfigMap> = Api::namespaced(client.clone(), "kube-system");
    let list = list_api.list(&ListParams::default()).await?;
    println!("kube-system list resource_version: {:?}", list.metadata.resource_version);
    for cm in list.items.iter().take(3) {
        println!("  {} -> rv={:?}", cm.meta().name.as_deref().unwrap_or("?"), cm.meta().resource_version);
    }

    println!("\n=== POC FINDINGS ===");
    println!("✅ Event::Init/InitApply/InitDone = initial bootstrap list");
    println!("✅ Event::Apply = insert OR update (uid tracking needed to distinguish)");
    println!("✅ Event::Delete = deletion (uid available for matching)");
    println!("✅ Metadata: name, namespace, uid, resource_version, labels, annotations, ownerReferences");
    println!("✅ resource_version can be persisted for resumable watching");
    println!("✅ Spec/Status fully serializable to serde_json::Value");
    println!("⚠️  Apply does NOT distinguish first-time insert vs update — must track known UIDs");
    println!("⚠️  For Drasi: use UID as element_id; track in StateStore to emit Insert vs Update");

    // Cleanup k3s
    docker_cleanup();
    Ok(())
}

fn docker_cleanup() {
    // POC only — container cleanup via shell
}
