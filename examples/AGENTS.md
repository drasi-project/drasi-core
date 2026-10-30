# examples

Runnable demonstrations. Rust examples use two build models. The Durable Task example also has a .NET application.

- examples/query is a workspace member (CI builds and lints it); run demos from the repo root with `cargo run --example <name>`; it appears in BOTH `members` and `exclude` of the root Cargo.toml - members wins and the exclude entry is inert for it (`publish = false` is what blocks publishing); leave the dual listing alone
- examples/lib/* crates are standalone: each has an empty `[workspace]` table and ../../../ path deps, and CI never builds them - after changing drasi-lib or component APIs, build each affected example in its own directory or breakage stays silent
- New examples/lib crates must include the empty `[workspace]` table (or be added to the root Cargo.toml `exclude` list), or cargo commands run inside that example's directory fail with "current package believes it's in a workspace when it's not"
- Nothing under examples/ is published
- examples/lib/durable-task/DurableTaskExample.csproj is the .NET entry point; `dotnet run --project examples/lib/durable-task` guides a single Enter-to-advance PostgreSQL/Drasi/DTS walkthrough; recovery tests and their native host live separately under components/reactions/durable-task/tests/{dotnet,native}; query.cypher, reaction.json, and dotnet/OrderWorkflow.cs are the business-code entry points
