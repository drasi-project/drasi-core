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

use crate::channels::ComponentStatus;

pub enum Operation {
    #[allow(dead_code)]
    Get,
    Start,
    Stop,
    Update,
    Delete,
}

pub fn is_operation_valid(status: &ComponentStatus, operation: &Operation) -> Result<(), String> {
    match (status, operation) {
        // Stopped state: get, start, update, and delete are allowed
        (ComponentStatus::Stopped, Operation::Get) => Ok(()),
        (ComponentStatus::Stopped, Operation::Start) => Ok(()),
        (ComponentStatus::Stopped, Operation::Update) => Ok(()),
        (ComponentStatus::Stopped, Operation::Delete) => Ok(()),
        (ComponentStatus::Stopped, Operation::Stop) => {
            Err("Cannot stop a component that is already stopped".to_string())
        }

        // Starting state: only get and stop are allowed
        (ComponentStatus::Starting, Operation::Get) => Ok(()),
        (ComponentStatus::Starting, Operation::Stop) => Ok(()),
        (ComponentStatus::Starting, Operation::Start) => {
            Err("Component is already starting".to_string())
        }
        (ComponentStatus::Starting, Operation::Update) => {
            Err("Cannot update a component while it is starting".to_string())
        }
        (ComponentStatus::Starting, Operation::Delete) => {
            Err("Cannot delete a component while it is starting".to_string())
        }

        // Running state: only get and stop are allowed
        (ComponentStatus::Running, Operation::Get) => Ok(()),
        (ComponentStatus::Running, Operation::Stop) => Ok(()),
        (ComponentStatus::Running, Operation::Start) => {
            Err("Component is already running".to_string())
        }
        (ComponentStatus::Running, Operation::Update) => {
            Err("Cannot update a running component. Stop it first".to_string())
        }
        (ComponentStatus::Running, Operation::Delete) => {
            Err("Cannot delete a running component. Stop it first".to_string())
        }

        // Stopping state: only get is allowed
        (ComponentStatus::Stopping, Operation::Get) => Ok(()),
        (ComponentStatus::Stopping, Operation::Start) => {
            Err("Cannot start a component while it is stopping".to_string())
        }
        (ComponentStatus::Stopping, Operation::Stop) => {
            Err("Component is already stopping".to_string())
        }
        (ComponentStatus::Stopping, Operation::Update) => {
            Err("Cannot update a component while it is stopping".to_string())
        }
        (ComponentStatus::Stopping, Operation::Delete) => {
            Err("Cannot delete a component while it is stopping".to_string())
        }

        // Reconfiguring state: only get is allowed (same as Stopping)
        (ComponentStatus::Reconfiguring, Operation::Get) => Ok(()),
        (ComponentStatus::Reconfiguring, Operation::Start) => {
            Err("Cannot start a component while it is reconfiguring".to_string())
        }
        (ComponentStatus::Reconfiguring, Operation::Stop) => {
            Err("Cannot stop a component while it is reconfiguring".to_string())
        }
        (ComponentStatus::Reconfiguring, Operation::Update) => {
            Err("Cannot update a component while it is already reconfiguring".to_string())
        }
        (ComponentStatus::Reconfiguring, Operation::Delete) => {
            Err("Cannot delete a component while it is reconfiguring".to_string())
        }

        // Error state: same as Stopped (allows recovery)
        (ComponentStatus::Error, Operation::Get) => Ok(()),
        (ComponentStatus::Error, Operation::Start) => Ok(()),
        (ComponentStatus::Error, Operation::Update) => Ok(()),
        (ComponentStatus::Error, Operation::Delete) => Ok(()),
        (ComponentStatus::Error, Operation::Stop) => {
            Err("Cannot stop a component that is in error state".to_string())
        }
    }
}

#[allow(dead_code)]
pub fn get_allowed_operations(status: &ComponentStatus) -> Vec<&'static str> {
    match status {
        ComponentStatus::Stopped => vec!["get", "start", "update", "delete"],
        ComponentStatus::Starting => vec!["get", "stop"],
        ComponentStatus::Running => vec!["get", "stop"],
        ComponentStatus::Stopping => vec!["get"],
        ComponentStatus::Reconfiguring => vec!["get"],
        ComponentStatus::Error => vec!["get", "start", "update", "delete"],
    }
}
