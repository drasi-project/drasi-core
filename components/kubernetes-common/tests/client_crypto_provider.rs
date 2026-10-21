use drasi_kubernetes_common::{build_client, KubernetesSourceConfig};

#[tokio::test]
async fn client_initializes_crypto_without_plugin_entry_point() {
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    let config = KubernetesSourceConfig {
        kubeconfig_content: Some(
            r#"
apiVersion: v1
kind: Config
clusters:
- name: test
  cluster:
    server: https://127.0.0.1:1
contexts:
- name: test
  context:
    cluster: test
    user: test
current-context: test
users:
- name: test
  user: {}
"#
            .into(),
        ),
        ..Default::default()
    };
    build_client(&config)
        .await
        .expect("TLS client construction");
    build_client(&config)
        .await
        .expect("Repeated initialization");
    assert!(rustls::crypto::CryptoProvider::get_default().is_some());

    let invalid = KubernetesSourceConfig {
        kubeconfig_content: Some("not: [valid YAML".into()),
        ..Default::default()
    };
    assert!(build_client(&invalid).await.is_err());
}
