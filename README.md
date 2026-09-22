# za

## Migrating from ai

The `za ai` command and its savings analytics have been removed. Use `git status`, `git diff`, and `git diff --staged` to inspect workspace changes.

Codex launchers no longer install Git wrappers or inject `BASH_ENV` and `ZA_AI_*` variables. Remove any `za ai shell`/`za ai env` initialization from your shell configuration and start a fresh shell or managed session from a clean parent environment. Previously recorded analytics and generated runtime files are left untouched.

## Removed context and review commands

`za gen` and `za diff` (including `diff stats` and the diff TUI) have been removed. za no longer exports `CONTEXT.md` or downloads repository snapshots for context generation. Review changes using Git, your IDE, or pull requests. Existing exported files are left untouched.

## Dependency resolution

`za deps resolve` resolves one dependency without requiring a project or modifying project files:

```bash
za deps resolve crate serde
za deps resolve npm react@latest
za deps resolve action actions/checkout@v4
```

The default output shows the requested reference and resolved version or commit. Use `--json` for a structured result, or `--emit` for a configuration entry:

```bash
za deps resolve crate serde --emit toml
za deps resolve npm react --emit package-json
za deps resolve action actions/checkout@v4 --emit yaml
```

Emitted crate and npm entries use exact versions. Action entries use a full commit SHA and preserve the original ref in a comment. `--json` and `--emit` are mutually exclusive.

`za deps` checks the current project's dependency requirements against the latest stable versions and shows upgrade guidance. It does not modify the manifest or lockfile.

```bash
za deps
za deps --path ./project --refresh
za deps --json
za deps --emit toml
za deps audit
za deps audit --fail-on-high
```

Use `--include-dev`, `--include-build`, and `--include-optional` to broaden the dependency selection. Lookup failures return a nonzero exit status after printing the available results. A project is required for the update overview; single-package queries use `za deps resolve crate <name>`.

The `za deps latest` command has been removed. Move project queries to `za deps`, single-package queries to `za deps resolve crate`, and existing audit invocations to `za deps audit`.

### Migrating from pin

The top-level `pin` command has been removed:

| Previous command | Replacement |
| --- | --- |
| `za pin crate serde` | `za deps resolve crate serde` |
| `za pin npm react@latest` | `za deps resolve npm react@latest` |
| `za pin action actions/checkout@v4` | `za deps resolve action actions/checkout@v4` |

The default output is now a concise resolution result. Use `--emit` when copying configuration. JSON retains the shared `schema_version`/`data` envelope; npm and crate results no longer include installation commands or non-exact configuration alternatives.

## Install

```bash
curl -fsSL https://github.com/lvillis/za/releases/latest/download/za-linux-amd64 | sudo install -m 0755 /dev/stdin /usr/local/bin/za
```

## agent-browser

Install the native binary from [agent-browser releases](https://github.com/vercel-labs/agent-browser/releases) with SHA-256 verification:

```bash
za tool install agent-browser
za run agent-browser -- install  # Download Chrome on first use
za tool update agent-browser
```

Use `za tool install agent-browser@0.38.1` to pin a version. Supported platforms are Linux (x64/ARM64, GNU or musl), macOS (x64/ARM64), and Windows (x64).
