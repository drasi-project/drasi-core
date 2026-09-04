# examples

Runnable demonstrations. Two different build models live side by side here.

- examples/query is a workspace member (CI builds and lints it); run demos from the repo root with `cargo run --example <name>`; it appears in BOTH `members` and `exclude` of the root Cargo.toml - members wins and the exclude entry is inert for it (`publish = false` is what blocks publishing); leave the dual listing alone
- examples/lib/* crates are standalone: each has an empty `[workspace]` table and ../../../ path deps, and CI never builds them - after changing drasi-lib or component APIs, build each affected example in its own directory or breakage stays silent
- New examples/lib crates must include the empty `[workspace]` table (or be added to the root Cargo.toml `exclude` list), or cargo commands run inside that example's directory fail with "current package believes it's in a workspace when it's not"
- Nothing under examples/ is published
