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

use crate::config::{AuthMode, KubernetesSourceConfig};
use anyhow::{anyhow, Result};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config};

pub async fn build_client(config: &KubernetesSourceConfig) -> Result<Client> {
    let kube_config = if let Some(content) = &config.kubeconfig_content {
        let cfg = Kubeconfig::from_yaml(content)?;
        Config::from_custom_kubeconfig(cfg, &KubeConfigOptions::default()).await?
    } else if let Some(path) = &config.kubeconfig_path {
        let cfg = Kubeconfig::read_from(path)?;
        Config::from_custom_kubeconfig(cfg, &KubeConfigOptions::default()).await?
    } else {
        match config.auth_mode {
            AuthMode::InCluster => Config::incluster_env()
                .map_err(|e| anyhow!("Failed to load in-cluster kube config: {e}"))?,
            AuthMode::Kubeconfig => Config::from_kubeconfig(&KubeConfigOptions::default()).await?,
        }
    };

    Ok(Client::try_from(kube_config)?)
}

pub fn parse_api_version(api_version: &str) -> Result<(String, String)> {
    if let Some((group, version)) = api_version.split_once('/') {
        if group.is_empty() || version.is_empty() {
            return Err(anyhow!("Invalid apiVersion '{api_version}'"));
        }
        return Ok((group.to_string(), version.to_string()));
    }
    if api_version.is_empty() {
        return Err(anyhow!("Invalid empty apiVersion"));
    }
    Ok(("".to_string(), api_version.to_string()))
}
