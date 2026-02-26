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

//! FFI layer for cdylib dynamic plugin loading.
//!
//! This module provides the `#[repr(C)]` types, vtable structs, and vtable
//! generation functions needed to wrap DrasiLib `Source`/`Reaction`/`BootstrapProvider`
//! trait implementations into stable C ABI entry points for cross-cdylib communication.
//!
//! # Module Structure
//!
//! - [`types`] — Core FFI-safe primitives (FfiStr, FfiResult, enums, etc.)
//! - [`callbacks`] — Log and lifecycle callback types
//! - [`vtables`] — `#[repr(C)]` vtable structs for all component types
//! - [`metadata`] — Plugin metadata for version validation at load time
//! - [`state_store_proxy`] — Plugin-side `StateStoreProvider` impl over vtable
//! - [`bootstrap_proxy`] — Plugin-side `BootstrapProvider` impl over vtable
//! - [`vtable_gen`] — Functions that wrap trait impls into vtables

pub mod bootstrap_proxy;
pub mod callbacks;
pub mod metadata;
pub mod state_store_proxy;
pub mod tracing_bridge;
pub mod types;
pub mod vtable_gen;
pub mod vtables;

// Re-export commonly used types at the ffi module level
pub use callbacks::{
    FfiLifecycleEvent, FfiLifecycleEventType, FfiLogEntry, FfiLogLevel, LifecycleCallbackFn,
    LogCallbackFn,
};
pub use metadata::{PluginMetadata, FFI_SDK_VERSION, TARGET_TRIPLE};
pub use types::{
    catch_panic_ffi, now_us, AsyncExecutorFn, FfiChangeOp, FfiComponentStatus, FfiDispatchMode,
    FfiGetResult, FfiOwnedStr, FfiResult, FfiStr, FfiStringArray, SendMutPtr, SendPtr,
};
pub use vtable_gen::{
    build_bootstrap_plugin_vtable, build_bootstrap_provider_vtable, build_reaction_plugin_vtable,
    build_reaction_vtable, build_reaction_vtable_from_boxed, build_source_plugin_vtable,
    build_source_vtable, build_source_vtable_from_boxed, current_instance_log_ctx,
    InstanceLogContext,
};
pub use vtables::{
    BootstrapPluginVtable, BootstrapProviderVtable, FfiBootstrapEvent, FfiBootstrapReceiver,
    FfiBootstrapSender, FfiChangeReceiver, FfiPluginRegistration, FfiQueryResultEvent,
    FfiQueryResultReceiver, FfiRuntimeContext, FfiSourceEvent, FfiSubscriptionResponse,
    ReactionPluginVtable, ReactionVtable, SourcePluginVtable, SourceVtable, StateStoreVtable,
};

pub use bootstrap_proxy::FfiBootstrapProviderProxy;
pub use state_store_proxy::FfiStateStoreProxy;
