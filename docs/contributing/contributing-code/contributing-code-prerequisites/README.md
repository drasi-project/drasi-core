# Repository Prerequisites

This page lists the prerequisites for working with the repository. The Drasi-Core is written in Rust, and packaged as a set of libraries.

## Operating system

We support developing on macOS, Linux and Windows with [WSL](https://docs.microsoft.com/windows/wsl/install).

## Development environment

Contributing to Drasi requires several tools to get started.

### Local installation

This is the list of core dependencies to install for the most common tasks. In general we expect all contributors to have all of these tools present:

- [Git](https://git-scm.com/downloads)
- [Rust](https://www.rust-lang.org/tools/install)

### Shared Libraries

#### libjq

Building the `middleware` crate will require `libjq` to be installed in your system. This can be done by [installing JQ](https://jqlang.org/download/) using your system package manager, such as Homebrew on MacOS, vcpkg on Windows or apt of Linux. Depending on your installation, you may also need to manually set the `JQ_LIB_DIR` environment variable if the build cannot automatically find it. Some common places where you might find this are:

| `/opt/homebrew/lib` | Homebrew |
| `/usr/lib/x86_64-linux-gnu` | apt |
| `/usr/lib64` | tdnf |
| `C:\vcpkg\installed\x64-windows\lib` | vcpkg |
| `C:\ProgramData\chocolatey\lib` | Chocolatey |

### Enable Git Hooks

We use pre-commit hooks to catch some issues early, enable the local git hooks using the following commands

```
chmod +x .githooks/pre-commit
git config core.hooksPath .githooks
```

### Editors

You can choose whichever editor you are most comfortable for working on Rust code. If you don't have a code editor set up for Rust, we recommend VS Code.

Alternatively, you can choose whichever editor you are most comfortable for working on Rust code. Feel free to skip this section if you want to make another choice.

- [Visual Studio Code](https://code.visualstudio.com/)
- [Rust Analyzer extension](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

