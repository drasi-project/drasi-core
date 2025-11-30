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

//! Source management operations for DrasiLib
//!
//! This module provides all source-related operations including adding, removing,
//! starting, and stopping sources.

use crate::channels::ComponentStatus;
use crate::component_ops::map_component_error;
use crate::config::SourceRuntime;
use crate::error::{DrasiError, Result};
use crate::lib_core::DrasiLib;
use crate::plugin_core::Source;

impl DrasiLib {
    /// Add a source instance to a running server, taking ownership.
    ///
    /// The source instance is wrapped in an Arc internally - callers transfer
    /// ownership rather than pre-wrapping in Arc.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// // Create the source and transfer ownership
    /// // let source = MySource::new("new-source", config)?;
    /// // core.add_source(source).await?;  // Ownership transferred
    /// # Ok(())
    /// # }
    /// ```
    pub async fn add_source(&self, source: impl Source + 'static) -> Result<()> {
        self.state_guard.require_initialized().await?;

        self.source_manager
            .add_source(source)
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to add source: {}", e)))?;

        Ok(())
    }

    /// Remove a source from a running server
    ///
    /// If the source is running, it will be stopped first before removal.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.remove_source("old-source").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn remove_source(&self, id: &str) -> Result<()> {
        self.state_guard.require_initialized().await?;

        // Stop if running
        let status = self
            .source_manager
            .get_source_status(id.to_string())
            .await
            .map_err(|_| DrasiError::component_not_found("source", id))?;

        if matches!(status, ComponentStatus::Running) {
            self.source_manager
                .stop_source(id.to_string())
                .await
                .map_err(|e| DrasiError::provisioning(format!("Failed to stop source: {}", e)))?;
        }

        // Delete the source
        self.source_manager
            .delete_source(id.to_string())
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to delete source: {}", e)))?;

        Ok(())
    }

    /// Start a stopped source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.start_source("my-source").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_source(&self, id: &str) -> Result<()> {
        self.state_guard.require_initialized().await?;

        map_component_error(
            self.source_manager.start_source(id.to_string()).await,
            "source",
            id,
            "start",
        )
    }

    /// Stop a running source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.stop_source("my-source").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn stop_source(&self, id: &str) -> Result<()> {
        self.state_guard.require_initialized().await?;

        map_component_error(
            self.source_manager.stop_source(id.to_string()).await,
            "source",
            id,
            "stop",
        )
    }

    /// List all sources with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let sources = core.list_sources().await?;
    /// for (id, status) in sources {
    ///     println!("Source {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_sources(&self) -> Result<Vec<(String, ComponentStatus)>> {
        self.inspection.list_sources().await
    }

    /// Get detailed information about a specific source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let source_info = core.get_source_info("my-source").await?;
    /// println!("Source type: {}", source_info.source_type);
    /// println!("Status: {:?}", source_info.status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_info(&self, id: &str) -> Result<SourceRuntime> {
        self.inspection.get_source_info(id).await
    }

    /// Get the current status of a specific source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_source_status("my-source").await?;
    /// println!("Source status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_status(&self, id: &str) -> Result<ComponentStatus> {
        self.inspection.get_source_status(id).await
    }
}
