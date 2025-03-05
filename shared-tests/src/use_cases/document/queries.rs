#![allow(clippy::unwrap_used)]
// Copyright 2024 The Drasi Authors.
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

pub fn document_query() -> &'static str {
    "
  MATCH (p:Pod)
  RETURN
    p.metadata.name as name,
    p.metadata.namespace as namespace,
    p.metadata.labels.app as app,
    p.metadata.annotations.`dapr.io/app-id` as app_id,
    p.metadata.annotations['dapr.io/app-id'] as app_id2,
    p.spec.containers[0].image as container_0_image,
    reduce(acc=0, x in p.status.containerStatuses | acc + x.restartCount) as total_restart_count,
    head(container IN p.status.containerStatuses WHERE container.name = 'sidecar').containerID as sidecar_container_id

    "
}
