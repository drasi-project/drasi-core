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

use crate::reactions::ApplicationReactionHandle;
use crate::sources::ApplicationSourceHandle;

/// Handle for applications to interact with Drasi Server
#[derive(Clone)]
pub struct ApplicationHandle {
    /// Handle for sending source events
    pub source: Option<ApplicationSourceHandle>,
    /// Handle for receiving query results
    pub reaction: Option<ApplicationReactionHandle>,
}

impl ApplicationHandle {
    /// Create a new ApplicationHandle with both source and reaction
    pub fn new(source: ApplicationSourceHandle, reaction: ApplicationReactionHandle) -> Self {
        Self {
            source: Some(source),
            reaction: Some(reaction),
        }
    }

    /// Create a new ApplicationHandle with only a source
    pub fn source_only(source: ApplicationSourceHandle) -> Self {
        Self {
            source: Some(source),
            reaction: None,
        }
    }

    /// Create a new ApplicationHandle with only a reaction
    pub fn reaction_only(reaction: ApplicationReactionHandle) -> Self {
        Self {
            source: None,
            reaction: Some(reaction),
        }
    }
}
