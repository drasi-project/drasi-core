# Legacy SQL Server bootstrap

This crate retains the existing `MsSqlBootstrapProvider` API and table snapshot
behavior. The independent [native bootstrap crate](../../computation-plugins/bootstrappers/mssql/README.md)
provides a standalone read-only SNAPSHOT mode and a separately selected,
explicitly provisioned marker-coordinated CDC mode.

The existing legacy maximum-CDC-LSN sample is **not** proof of a consistent
snapshot/CDC handover: capture is asynchronous even inside a SNAPSHOT transaction.
Native standalone snapshots preserve the useful read-only workflow without
advertising that watermark as gap-free. The stronger native mode changes locking
and marker permissions explicitly; real concurrent-write qualification remains
blocked on the current ARM64 Podman environment.
