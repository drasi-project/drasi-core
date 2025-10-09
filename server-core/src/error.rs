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

use std::fmt;

#[derive(Debug)]
pub enum DrasiError {
    Configuration(String),
    ComponentNotFound(String),
    ComponentAlreadyExists(String),
    InvalidState(String),
    IoError(std::io::Error),
    SerializationError(serde_json::Error),
    Other(anyhow::Error),
}

impl fmt::Display for DrasiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DrasiError::Configuration(msg) => write!(f, "Configuration error: {}", msg),
            DrasiError::ComponentNotFound(name) => write!(f, "Component not found: {}", name),
            DrasiError::ComponentAlreadyExists(name) => {
                write!(f, "Component already exists: {}", name)
            }
            DrasiError::InvalidState(msg) => write!(f, "Invalid state: {}", msg),
            DrasiError::IoError(err) => write!(f, "IO error: {}", err),
            DrasiError::SerializationError(err) => write!(f, "Serialization error: {}", err),
            DrasiError::Other(err) => write!(f, "Error: {}", err),
        }
    }
}

impl std::error::Error for DrasiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DrasiError::IoError(err) => Some(err),
            DrasiError::SerializationError(err) => Some(err),
            DrasiError::Other(err) => Some(err.as_ref()),
            _ => None,
        }
    }
}

impl From<std::io::Error> for DrasiError {
    fn from(err: std::io::Error) -> Self {
        DrasiError::IoError(err)
    }
}

impl From<serde_json::Error> for DrasiError {
    fn from(err: serde_json::Error) -> Self {
        DrasiError::SerializationError(err)
    }
}

impl From<anyhow::Error> for DrasiError {
    fn from(err: anyhow::Error) -> Self {
        DrasiError::Other(err)
    }
}
