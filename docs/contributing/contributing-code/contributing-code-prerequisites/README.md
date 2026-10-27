# Repository Prerequisites

This page lists the prerequisites for working with the repository. The Drasi-Core is written in Rust, and packaged as a set of libraries.

## Operating system

We support developing on macOS, Linux and Windows with [WSL](https://docs.microsoft.com/windows/wsl/install).

## Development environment

Contributing to Drasi requires several tools to get started.

### Using Dev Container (Recommended)

The easiest way to get started is to use the provided Dev Container configuration. This will automatically set up all required dependencies, including:

- Git
- Rust toolchain
- libjq (required for the middleware component)
- Recommended VS Code extensions

To use the Dev Container:

1. Install [Docker](https://www.docker.com/products/docker-desktop) or [Podman](https://podman.io/)
2. Install [Visual Studio Code](https://code.visualstudio.com/)
3. Install the [Dev Containers extension](https://marketplace.visualstudio.com/items?itemName=ms-vscode-remote.remote-containers)
4. Open the repository in VS Code
5. When prompted, click "Reopen in Container" (or use the Command Palette: `Dev Containers: Reopen in Container`)

The Dev Container will automatically install all dependencies and configure the Rust build environment with the necessary library paths.

### Local installation

This is the list of core dependencies to install for the most common tasks. In general we expect all contributors to have all of these tools present:

- [Git](https://git-scm.com/downloads)
- [Rust](https://www.rust-lang.org/tools/install)
- libjq development libraries (required for the middleware component)
  - On Ubuntu/Debian: `sudo apt-get install libjq-dev libonig-dev`
  - On macOS: `brew install jq`
  - On other systems, consult your package manager

After installing libjq, you need to set the `JQ_LIB_DIR` environment variable so the Rust build system can find the library.

**Linux:**
```bash
# Add to your shell profile (e.g., ~/.bashrc or ~/.zshrc)
echo 'export JQ_LIB_DIR="/usr/lib/x86_64-linux-gnu"' >> ~/.bashrc
source ~/.bashrc
```

> **Note for non-x86_64 architectures:** The path above is for x86_64. For ARM64 or other architectures, adjust the path accordingly (e.g., `/usr/lib/aarch64-linux-gnu`). Alternatively, use the Dev Container setup which automatically detects the correct path for your architecture.

**macOS with Homebrew:**
```bash
# For Apple Silicon Macs (M1/M2/M3):
echo 'export JQ_LIB_DIR="/opt/homebrew/lib"' >> ~/.zshrc
source ~/.zshrc

# For Intel Macs:
# echo 'export JQ_LIB_DIR="/usr/local/lib"' >> ~/.bash_profile
# source ~/.bash_profile
```

Adjust the path as needed based on where your package manager installed libjq.

### Shared Libraries

#### libjq

Building the `middleware` crate will require `libjq` to be installed in your system. This can be done by [installing JQ](https://jqlang.org/download/) using your system package manager, such as Homebrew on MacOS, vcpkg on Windows or apt on Linux. Depending on your installation, you may also need to manually set the `JQ_LIB_DIR` environment variable if the build cannot automatically find it. Some common places where you might find this are:

| Package Manager | Path |
|-|-|
| Homebrew | `/opt/homebrew/lib` |
| apt | `/usr/lib/x86_64-linux-gnu` |
| tdnf | `/usr/lib64` |
| vcpkg | `C:\vcpkg\installed\x64-windows\lib` |
| Chocolatey | `C:\ProgramData\chocolatey\lib` |

### Enable Git Hooks

We use pre-commit hooks to catch some issues early, enable the local git hooks using the following commands

```
chmod +x .githooks/pre-commit
git config core.hooksPath .githooks
```

### Set up optional architecture checks

You do not need cargo-bylaw for normal builds, tests, Clippy, hooks, or editor use. The architecture workflow installs it separately in CI. Dev-container setup does not install it.

To run architecture checks locally, install the pinned release in an isolated directory:

```sh
cargo install --git https://github.com/danielgerlag/cargo-bylaw \
	--rev 650ca7d61597ddc3814370de1edd4eadee294540 --locked cargo-bylaw \
	--root "$HOME/.local/share/drasi-bylaw"
rustup component add rust-src --toolchain 1.95.0
PATH="$HOME/.local/share/drasi-bylaw/bin:$PATH" cargo bylaw --version
```

The version command prints `cargo-bylaw 0.1.1`. The Git revision pins that release because its CLI publication to crates.io failed. Do not substitute version 0.1.0, which reports incomplete analysis on valid Drasi code.

This installation does not replace a cargo-bylaw binary used by another project. It does not change your shell profile or PATH. Prefix each Bylaw command with the same `PATH=...` assignment, or add that directory to PATH yourself. Installing without `--root` uses Cargo's shared installation directory and may replace another installed version.

Bylaw uses the repository's Rust 1.95.0 toolchain and its own embedded analyzer. No editor extension or separate rust-analyzer binary is needed. The `rust-src` component is an explicit addition to that toolchain, not a change to the repository's toolchain file. Python 3.11 or later is needed only for the supplemental manifest checks and fixtures, not for `cargo bylaw check`.

Analysis enables build scripts and procedural macros. It can execute repository build code, fetch dependencies, and populate Cargo's caches and target directory. Selecting five origin crates still loads the workspace and can build native dependencies such as RocksDB. One macOS probe used about 4.2 GiB of memory. That observation is not a resource limit.

Prepare the native tools before checking. CI uses these packages on Ubuntu:

```sh
sudo apt-get install -y --no-install-recommends \
	cmake clang libclang-dev pkg-config libssl-dev protobuf-compiler
```

On macOS, provide a C++ toolchain, libclang, CMake, and `protoc`. Bylaw itself does not need Docker. Use WSL for Windows development; native Windows analysis is not verified.

See [Run architecture checks](../contributing-code-tests/#run-architecture-checks) for the commands and policy scope.

### Editors

You can choose whichever editor you are most comfortable for working on Rust code. If you don't have a code editor set up for Rust, we recommend VS Code.

Alternatively, you can choose whichever editor you are most comfortable for working on Rust code. Feel free to skip this section if you want to make another choice.

- [Visual Studio Code](https://code.visualstudio.com/)
- [Rust Analyzer extension](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)
