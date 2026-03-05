# drasi-ffi-primitives

Core FFI-safe types and vtable generation macros for the Drasi plugin boundary.

This crate provides domain-independent FFI primitives (`FfiStr`, `FfiResult`, etc.) and procedural macros (`ffi_vtable!`, `vtable_fn_getter!`, `vtable_fn_async!`, `impl_vtable_proxy!`) used to define the stable C ABI interface between Drasi plugins and the host runtime.
