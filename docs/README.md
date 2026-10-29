# Drasi documentation

The documentation for Drasi is at https://drasi-docs.azurewebsites.net/, with the source in the [drasi-project/docs repo](https://github.com/drasi-project/docs).

## Contributing to Drasi-Core

This folder contains the documentation for contributing to Drasi-Core. 

To browse the table of contents, refer to the [contributing docs](./../CONTRIBUTING.md).

## Runtime architecture

The [ComputationGraph maintainer's guide](../lib/docs/computation-graph-maintainers-guide.md)
explains runtime ownership, lifecycle, opt-in recovery, plugin services and
the corresponding implementation and qualification entry points.

The [schemas, ports, and pipes guide](../lib/docs/computation-graph-schemas-ports-pipes.md)
explains how data contracts and connections fit together, what each pipe family
provides, and the boundaries of ordering, acknowledgement, and recovery.

The [ComputationGraph runtime architecture review](../lib/docs/runtime-architecture.md)
includes DrasiLib and graph ownership diagrams, an alphabetical production-type
inventory, a runtime cardinality matrix, and a namespace LOC heatmap. It also
documents how to regenerate and compare the artifacts during architecture review.

The [ComputationGraph remaining-work document](../lib/docs/computation-graph-remaining-work.md)
is the single active list of known issues, investigations, qualification gaps
and suggested improvements across Core and its consumers, with priorities,
scope and completion criteria.

Release operators: [repairing signatures on existing plugin releases](./contributing/plugin-signature-repair.md).
