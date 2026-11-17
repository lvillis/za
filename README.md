# za

A CLI for tool management, dependency checks, and development workflows.

- Install and update CLI tools.
- Check dependency updates, audit vulnerabilities, and resolve package versions.
- Manage Codex sessions, inspect remote IDE processes, and troubleshoot GitHub CI.

## Install

Download the binary for your platform from [GitHub Releases](https://github.com/lvillis/za/releases/latest) and place it on your `PATH`.

To build from source, run in this repository:

```bash
cargo install --path . --locked
```

## Usage

```bash
# Manage tools
za tool install codex just
za tool update
za tool ls

# Check dependencies
za deps
za deps audit
za deps resolve crate serde

# Development workflows
za codex
za ide ps
za gh ci watch

# Update za
za update
```

Run `za --help` or `za <command> --help` for available commands and options.
