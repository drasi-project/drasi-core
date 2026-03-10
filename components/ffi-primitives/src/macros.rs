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

//! Declarative macros for generating FFI vtable structs and bridge code.
//!
//! These macros reduce boilerplate for the most common FFI patterns:
//!
//! - [`ffi_vtable!`] — Generate `#[repr(C)]` vtable structs
//! - [`vtable_fn_getter!`] — Generate synchronous getter `extern "C"` functions
//! - [`vtable_fn_async!`] — Generate async method `extern "C"` functions
//! - [`impl_vtable_proxy!`] — Generate proxy structs wrapping vtables

/// Generate a `#[repr(C)]` vtable struct with `state`, `executor`, function
/// pointers for each declared method, and a `drop_fn`.
///
/// Each method declaration specifies mutability (`*const` or `*mut` for the
/// state pointer), optional extra parameters, and an optional return type.
///
/// `Send + Sync` are automatically implemented.
///
/// # Example
///
/// ```rust
/// use drasi_ffi_primitives::*;
///
/// ffi_vtable! {
///     /// FFI vtable for a Widget.
///     pub struct WidgetVtable {
///         fn id_fn(state: *const) -> FfiStr,
///         fn start_fn(state: *mut) -> FfiResult,
///         fn enabled_fn(state: *const) -> bool,
///     }
/// }
/// ```
///
/// Expands to a `#[repr(C)]` struct with fields: `state`, `executor`,
/// `id_fn`, `start_fn`, `enabled_fn`, and `drop_fn`.
#[macro_export]
macro_rules! ffi_vtable {
    (
        $(#[$meta:meta])*
        pub struct $name:ident {
            $(
                $(#[$fn_meta:meta])*
                fn $method:ident ( state: *$mutability:ident $(, $param:ident : $param_ty:ty)* ) $(-> $ret:ty)?
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[repr(C)]
        pub struct $name {
            pub state: *mut ::std::ffi::c_void,
            pub executor: $crate::AsyncExecutorFn,
            $(
                $(#[$fn_meta])*
                pub $method: extern "C" fn(
                    state: *$mutability ::std::ffi::c_void
                    $(, $param: $param_ty)*
                ) $(-> $ret)?,
            )*
            pub drop_fn: extern "C" fn(state: *mut ::std::ffi::c_void),
        }

        unsafe impl Send for $name {}
        unsafe impl Sync for $name {}
    };
}

/// Generate a proxy struct that wraps a vtable into method calls.
///
/// Each method uses closure-like `|vt| { ... }` syntax where `vt` is bound
/// to `&self.vtable`, working around `macro_rules!` hygiene limitations
/// that prevent direct `self` access in user-provided blocks.
///
/// The generated struct automatically implements `Drop` via `drop_fn`.
///
/// # Example
///
/// ```rust
/// use drasi_ffi_primitives::*;
/// use std::ffi::c_void;
///
/// ffi_vtable! {
///     pub struct WidgetVtable {
///         fn id_fn(state: *const) -> FfiStr,
///         fn start_fn(state: *mut) -> FfiResult,
///     }
/// }
///
/// impl_vtable_proxy! {
///     /// Host-side proxy wrapping WidgetVtable.
///     pub struct WidgetProxy wraps WidgetVtable {
///         fn id() -> String
///             |vt| { unsafe { (vt.id_fn)(vt.state as *const c_void).to_string() } }
///
///         fn start() -> Result<(), String>
///             |vt| { unsafe { (vt.start_fn)(vt.state).into_result() } }
///     }
/// }
/// ```
#[macro_export]
macro_rules! impl_vtable_proxy {
    (
        $(#[$meta:meta])*
        pub struct $proxy_name:ident wraps $vtable_name:ident {
            $(
                $(#[$fn_meta:meta])*
                fn $method:ident ( $($param:ident : $param_ty:ty),* ) $(-> $ret:ty)?
                    |$vt:ident| $body:block
            )*
        }
    ) => {
        $(#[$meta])*
        pub struct $proxy_name {
            vtable: $vtable_name,
        }

        unsafe impl Send for $proxy_name {}
        unsafe impl Sync for $proxy_name {}

        impl $proxy_name {
            pub fn new(vtable: $vtable_name) -> Self {
                Self { vtable }
            }

            $(
                $(#[$fn_meta])*
                #[allow(unused_variables)]
                pub fn $method(&self $(, $param: $param_ty)*) $(-> $ret)? {
                    let $vt = &self.vtable;
                    $body
                }
            )*
        }

        impl Drop for $proxy_name {
            fn drop(&mut self) {
                (self.vtable.drop_fn)(self.vtable.state);
            }
        }
    };
}

/// Generate an `extern "C"` function that runs an async trait method via
/// `thread::spawn` + `block_on`, with panic catching.
///
/// The function is generic over a wrapper type containing the trait impl,
/// matching the pattern used in vtable generation.
///
/// # Example
///
/// ```ignore
/// vtable_fn_async!(start_fn, SourceWrapper<T>, T: Source, runtime_handle, |w| {
///     w.inner.start().await
/// });
/// ```
#[macro_export]
macro_rules! vtable_fn_async {
    (
        $fn_name:ident,
        $wrapper:ty,
        $bound:ident : $trait:path,
        $runtime_field:ident,
        |$w:ident| $body:expr
    ) => {
        extern "C" fn $fn_name<$bound: $trait + 'static>(
            state: *mut ::std::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_panic_ffi(|| {
                let $w = unsafe { &*(state as *const $wrapper) };
                let handle = ($w.$runtime_field)().handle().clone();
                let result = ::std::thread::spawn({
                    let $w = unsafe { &*(state as *const $wrapper) };
                    move || handle.block_on(async { $body })
                })
                .join()
                .expect("vtable async thread panicked");
                match result {
                    Ok(()) => $crate::FfiResult::ok(),
                    Err(e) => $crate::FfiResult::err(e.to_string()),
                }
            })
        }
    };
}

/// Generate a synchronous getter `extern "C"` function.
///
/// Supports `-> FfiStr` and `-> bool` return types.
///
/// # Example
///
/// ```ignore
/// vtable_fn_getter!(id_fn -> FfiStr, SourceWrapper<T>, T: Source, |w| &w.cached_id);
/// vtable_fn_getter!(auto_start_fn -> bool, SourceWrapper<T>, T: Source, |w| w.inner.auto_start());
/// ```
#[macro_export]
macro_rules! vtable_fn_getter {
    (
        $fn_name:ident -> FfiStr,
        $wrapper:ty,
        $bound:ident : $trait:path,
        |$w:ident| $body:expr
    ) => {
        extern "C" fn $fn_name<$bound: $trait + 'static>(
            state: *const ::std::ffi::c_void,
        ) -> $crate::FfiStr {
            let $w = unsafe { &*(state as *const $wrapper) };
            $crate::FfiStr::from_str($body)
        }
    };
    (
        $fn_name:ident -> bool,
        $wrapper:ty,
        $bound:ident : $trait:path,
        |$w:ident| $body:expr
    ) => {
        extern "C" fn $fn_name<$bound: $trait + 'static>(state: *const ::std::ffi::c_void) -> bool {
            let $w = unsafe { &*(state as *const $wrapper) };
            $body
        }
    };
}
