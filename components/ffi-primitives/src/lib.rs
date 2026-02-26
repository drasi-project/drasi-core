//! `drasi-ffi-primitives` — Core FFI-safe types and vtable generation macros.
//!
//! This crate provides the fundamental `#[repr(C)]` types and declarative macros
//! needed to build vtable-based FFI boundaries for Drasi's cdylib plugin system.
//!
//! **No domain-specific dependencies** — this crate depends only on `std`.
//!
//! # Types
//!
//! - [`FfiStr`] / [`FfiOwnedStr`] / [`FfiStringArray`] — String types for FFI
//! - [`FfiResult`] — Result type for FFI operations
//! - [`FfiGetResult`] — Result type for key-value store get operations
//! - [`AsyncExecutorFn`] — Type-erased async executor for cdylib runtimes
//! - [`SendPtr`] / [`SendMutPtr`] — Send-safe raw pointer wrappers
//!
//! # Macros
//!
//! - [`ffi_vtable!`] — Generate `#[repr(C)]` vtable structs from a method list
//! - [`vtable_fn_getter!`] — Generate synchronous getter `extern "C"` functions
//! - [`vtable_fn_async!`] — Generate async method `extern "C"` functions with thread+block_on
//! - [`impl_vtable_proxy!`] — Generate proxy structs that wrap vtables into method calls

pub mod macros;
pub mod types;

pub use types::*;
