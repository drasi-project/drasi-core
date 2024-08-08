# Contributing to Drasi code

This guide includes background and tips for working on the Drasi codebase.


## Formatting

We recommend that you run `cargo fmt` often, as this will correct any linting / formatting issues.  This will also run on the pre commit git hook.  The CI pipeline will fail if you submit a pull request where `cargo fmt` has not been run.

## Validating changes

Be sure the read the [testing guide](../contributing-code-tests/) to be confident your changes are not breaking anything.

