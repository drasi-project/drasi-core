# drasi-ffi-primitives

Core FFI-safe types and vtable generation macros for the Drasi plugin boundary.

This crate provides domain-independent FFI primitives (`FfiStr`, `FfiResult`, etc.) and declarative macros (`ffi_vtable!`, `vtable_fn_getter!`, `vtable_fn_async!`, `impl_vtable_proxy!`) used to define the C ABI interface between Drasi plugins and the host runtime.

The crate depends only on `std` and has no optional features. It denies the
`unused` lint group in its own code, overriding the workspace's temporary
allowance. This keeps unused imports, variables and private helpers visible
without removing public compatibility interfaces. It is the first package in
the gradual warning cleanup, not a workspace-wide policy change.

```sh
cargo test -p drasi-ffi-primitives
cargo clippy -p drasi-ffi-primitives --all-targets -- -D warnings
```
