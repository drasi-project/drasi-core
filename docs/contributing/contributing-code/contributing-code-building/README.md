# Building the code

The root directory of the repo is a [Cargo Workspace](https://doc.rust-lang.org/cargo/reference/workspaces.html).  This means you can simply run `cargo build` from the root directory to build everything, the output will be in the `target` directory.
If you wish to build a specific component, simply run `cargo build` from the folder of the component.  See [Understanding the drasi-core repo code organization](../contributing-code-organization/) for more information on the specific components.