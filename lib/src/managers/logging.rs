// Copyright 2025 The Drasi Authors.
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

use log::{error, info, warn};

pub fn log_component_start(component: &str, id: &str) {
    info!("Starting {component} component: {id}");
}

pub fn log_component_stop(component: &str, id: &str) {
    info!("Stopping {component} component: {id}");
}

pub fn log_component_error(component: &str, id: &str, error: &str) {
    error!("Error in {component} component {id}: {error}");
}

#[allow(dead_code)]
pub fn log_component_warning(component: &str, id: &str, warning: &str) {
    warn!("Warning in {component} component {id}: {warning}");
}
