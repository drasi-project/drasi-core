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

After installing libjq, you need to configure the Rust build system to find the library.

> **Note:** The following commands will create or overwrite `~/.cargo/config.toml`. If you have existing Cargo configuration, you may want to manually add just the `[build]` section and `JQ_LIB_DIR` setting instead.

**Linux:**
```bash
mkdir -p ~/.cargo
cat > ~/.cargo/config.toml << 'EOF'
[build]
JQ_LIB_DIR = "/usr/lib/x86_64-linux-gnu"
EOF
```

**macOS with Homebrew:**
```bash
mkdir -p ~/.cargo

# For Apple Silicon Macs (M1/M2/M3):
cat > ~/.cargo/config.toml << 'EOF'
[build]
JQ_LIB_DIR = "/opt/homebrew/lib"
EOF

# For Intel Macs:
# cat > ~/.cargo/config.toml << 'EOF'
# [build]
# JQ_LIB_DIR = "/usr/local/lib"
# EOF
```

Adjust the path as needed based on where your package manager installed libjq.


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

