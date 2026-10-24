// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A host for ordinary DrasiLib instances. Process supervision is a portless
//! native service; data belongs exclusively to separately configured components.

pub mod config;
#[cfg(unix)]
mod host;
#[cfg(unix)]
mod platform;
#[cfg(unix)]
pub mod protocol;
pub mod resources;
#[cfg(unix)]
mod state;
#[cfg(unix)]
pub mod supervisor;
#[cfg(not(unix))]
mod unsupported;

pub use config::{HostConfig, RuntimeBudget, StopStage};
#[cfg(unix)]
pub use host::{run, HostObservation, HostPhase};
#[cfg(unix)]
pub use protocol::{ControlClient, ControlCommand, ControlReply, Credentials, OperationStatus};
pub use resources::InstanceResources;
#[cfg(unix)]
pub use supervisor::{
    ChildObservation, ChildPhase, ChildSupervisor, ChildSupervisorResource, SupervisorFactory,
};
#[cfg(not(unix))]
pub use unsupported::SupervisorFactory;

/// Register the normal portless supervisor implementation. Embedding hosts
/// supply its declared `owner` resource; no process-global owner is created.
pub fn factories() -> Vec<std::sync::Arc<dyn drasi_lib::computation::v1::ComponentFactory>> {
    vec![std::sync::Arc::new(SupervisorFactory::default())]
}

pub const CONTROL_VERSION: u32 = 1;
pub const SUPERVISOR_IMPLEMENTATION: &str = "drasi.host/local-child";

/// Run the same executable at any level of an instance hierarchy.
pub fn main_entry() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        host::main_entry()
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("owned local instance hosting currently requires Unix lifetime pipes and process groups")
    }
}
