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

use anyhow::Result;
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config};
use tempfile::TempDir;
use testcontainers_modules::k3s::{K3s, KUBE_SECURE_PORT};
use testcontainers_modules::testcontainers::{runners::AsyncRunner, ContainerAsync, ImageExt};

pub struct K3sGuard {
    _temp_dir: TempDir,
    container: ContainerAsync<K3s>,
    kubeconfig: String,
}

impl K3sGuard {
    pub async fn start() -> Result<Self> {
        let temp_dir = tempfile::tempdir()?;
        let container = K3s::default()
            .with_conf_mount(temp_dir.path())
            .with_privileged(true)
            .with_userns_mode("host")
            .start()
            .await?;

        let host_port = container
            .get_host_port_ipv4(KUBE_SECURE_PORT)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get mapped k3s API port: {e}"))?;

        let mut kubeconfig = container
            .image()
            .read_kube_config()
            .map_err(|e| anyhow::anyhow!("Failed to read k3s kubeconfig: {e}"))?;
        kubeconfig = kubeconfig.replace(
            "https://127.0.0.1:6443",
            &format!("https://localhost:{host_port}"),
        );
        kubeconfig = kubeconfig.replace(
            "https://localhost:6443",
            &format!("https://localhost:{host_port}"),
        );

        Ok(Self {
            _temp_dir: temp_dir,
            container,
            kubeconfig,
        })
    }

    pub async fn client(&self) -> Result<Client> {
        let kcfg = Kubeconfig::from_yaml(&self.kubeconfig)?;
        let cfg = Config::from_custom_kubeconfig(kcfg, &KubeConfigOptions::default()).await?;
        Ok(Client::try_from(cfg)?)
    }

    pub fn kubeconfig(&self) -> &str {
        &self.kubeconfig
    }

    #[allow(dead_code)]
    pub fn container_id(&self) -> &str {
        self.container.id()
    }
}
