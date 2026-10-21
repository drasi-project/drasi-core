//! Build both Kubernetes plugins with `dynamic-plugin`, then run with `--ignored`.
//! A host-installed provider cannot initialize the separate Rustls copies in cdylibs.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use drasi_host_sdk::callbacks;
use drasi_host_sdk::loader::{load_plugin_from_path, LoadedPlugin};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapRequest};
use drasi_plugin_sdk::descriptor::{BootstrapPluginDescriptor, SourcePluginDescriptor};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn load(name: &str) -> LoadedPlugin {
    let directory = std::env::var_os("DRASI_TEST_PLUGIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug"));
    let filename = format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        name.replace('-', "_"),
        std::env::consts::DLL_SUFFIX
    );
    load_plugin_from_path(
        &directory.join(filename),
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .expect("Load rebuilt Kubernetes cdylib with the matching host SDK")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires freshly built Kubernetes source and bootstrap cdylibs"]
async fn kubernetes_source_and_bootstrap_initialize_crypto_across_cdylib() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Bind task-owned loopback API");
    let address = listener.local_addr().expect("API address");
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let api = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("Accept Kubernetes request");
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let byte = stream.read_u8().await.expect("Read HTTP request");
                request.push(byte);
                assert!(request.len() < 16_384, "Unbounded HTTP headers");
            }
            observed.fetch_add(1, Ordering::SeqCst);
            let body = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"sdk-release-probe forbidden","reason":"Forbidden","code":403}"#;
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("Send API error");
        }
    });
    let kubeconfig = format!(
        r#"
apiVersion: v1
kind: Config
clusters:
- name: test
  cluster:
    server: http://{address}
contexts:
- name: test
  context:
    cluster: test
    user: test
current-context: test
users:
- name: test
  user: {{}}
"#
    );
    let config = serde_json::json!({
        "resources": [{"apiVersion": "v1", "kind": "Pod"}],
        "namespaces": ["sdk-release-probe"],
        "kubeconfigContent": kubeconfig,
    });
    let source_plugin = load("drasi-source-kubernetes");
    let source = source_plugin.source_plugins[0]
        .create_source("sdk-release-probe", &config, false)
        .await
        .expect("Create source through FFI");
    let error = tokio::time::timeout(Duration::from_secs(15), source.start())
        .await
        .expect("Source initialization must not hang")
        .expect_err("Real API authorization errors must not be suppressed");
    assert!(
        error
            .to_string()
            .contains("Cannot list Kubernetes resource"),
        "Expected the API error, not a crypto/FFI panic: {error:#}"
    );
    source.stop().await.expect("Stop failed source");

    let bootstrap_plugin = load("drasi-bootstrap-kubernetes");
    let provider = bootstrap_plugin.bootstrap_plugins[0]
        .create_bootstrap_provider(&serde_json::json!({}), &config)
        .await
        .expect("Create bootstrap provider through FFI");
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let context = BootstrapContext::new_minimal("sdk-release-host".into(), "probe".into());
    let request = BootstrapRequest {
        query_id: "probe".into(),
        node_labels: vec![],
        relation_labels: vec![],
        request_id: "probe".into(),
    };
    let error = tokio::time::timeout(
        Duration::from_secs(15),
        provider.bootstrap(request, &context, tx, None),
    )
    .await
    .expect("Bootstrap initialization must not hang")
    .expect_err("Real API authorization errors must cross FFI");
    assert!(
        error.to_string().contains("sdk-release-probe forbidden"),
        "Expected the API error, not a crypto/FFI panic: {error:#}"
    );
    tokio::time::timeout(Duration::from_secs(5), api)
        .await
        .expect("Both plugins must reach the API")
        .expect("Mock API task");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}
