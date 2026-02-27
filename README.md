<p align="center">
  <img src="./assets/za-logo.svg" alt="za logo" width="900" />
</p>

<p align="center">
  <strong>AI-native CLI for context, tools, and dependency health.</strong>
</p>


<p align="center">

[![Crates.io](https://img.shields.io/crates/v/za.svg)](https://crates.io/crates/za)
[![CI](https://github.com/lvillis/za/actions/workflows/ci.yaml/badge.svg)](https://github.com/lvillis/za/actions)
[![Repo Size](https://img.shields.io/github/repo-size/lvillis/za?color=328657)](https://github.com/lvillis/za)
[![Docker Pulls](https://img.shields.io/docker/pulls/lvillis/za)](https://hub.docker.com/r/lvillis/za)
[![MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

</p>

## Why `za`

`za` keeps modern AI-native engineering workflows simple:

- Generate high-signal project context files fast.
- Manage critical CLI binaries with version visibility and safe updates.
- Audit Rust dependencies with maintenance signals before they become incidents.
- Run tools with consistent runtime and proxy settings.

## Install

### Prebuilt binary (Linux x86_64)

```bash
curl -fsSL https://github.com/lvillis/za/releases/latest/download/za-x86_64-unknown-linux-musl \
  -o /usr/local/bin/za
chmod +x /usr/local/bin/za
```

### Cargo

```bash
cargo install za
```

## Quick Start

### 1) Generate `CONTEXT.md`

```bash
za gen
za gen --max-lines 800 --include-binary --output docs/CONTEXT.md
```

### 2) Manage tool versions

```bash
za tool install codex
za tool install docker-compose
za tool install rg
za tool install fd
za tool install tcping
za tool install dust
za tool install just

za tool list
za tool list --updates
za tool update codex
za run codex
```

### 3) Audit dependency risk

```bash
za deps
za deps --jobs 16
za deps --fail-on-high
```

## Command Map

| Command | Purpose |
| --- | --- |
| `za gen` | Generate project context snapshots (`CONTEXT.md`). |
| `za tool` | Install/update/list/use/uninstall managed binaries. |
| `za run` | Launch a tool directly with normalized proxy environment variables. |
| `za deps` | Audit Rust dependency maintenance risk. |
| `za config` | Persist global CLI config (`[auth]` and `[run]` modules). |

## Tool Management

`za tool` defaults to **system scope** and supports optional **user scope**.

### Paths

- Global store: `/var/lib/za/tools/store`
- Global active pointers: `/var/lib/za/tools/current`
- Global binaries: `/usr/local/bin`
- User store: `~/.local/share/za/tools/store`
- User active pointers: `~/.local/state/za/tools/current`
- User binaries: `~/.local/bin`

### Supported tools

Run:

```bash
za tool list --supported
za tool list --supported --json
```

Current built-in tool policies:

| Tool | Aliases | Source policy |
| --- | --- | --- |
| `codex` | `codex-cli` | GitHub Release (SHA-256 verify), fallback `cargo install codex-cli` |
| `docker-compose` | - | GitHub Release (SHA-256 verify) |
| `rg` | `ripgrep` | GitHub Release (SHA-256 verify) |
| `fd` | `fdfind` | GitHub Release (SHA-256 verify) |
| `tcping` | `tcping-rs` | GitHub Release (SHA-256 verify) |
| `dust` | - | GitHub Release (SHA-256 verify) |
| `just` | - | GitHub Release (SHA-256 verify) |

### Common workflows

```bash
# install a specific version
za tool install codex:0.105.0

# switch active version
za tool use codex:0.105.0

# check updates (human + json)
za tool list --updates
za tool list --updates --json

# CI-friendly exit codes:
# 20 => updates available
# 21 => update checks failed
za tool list --updates --fail-on-updates
za tool list --updates --fail-on-check-errors

# update keeps active binary current and auto-prunes old versions
za tool update codex

# uninstall one or all versions
za tool uninstall codex:0.104.0
za tool uninstall codex
```

### Existing binaries adoption

If a supported unmanaged binary is already present in scope bin path (for example `/usr/local/bin/codex`), `za tool install <tool>` adopts it first by detecting local version.

### Direct launch

Use `za run <tool>` for a minimal launch flow:

```bash
za run codex
za run codex -- --help
```

Resolution order:

1. User-scope active managed tool
2. Global-scope active managed tool
3. `PATH`

### Proxy behavior

`za run`, `za tool`, and `za deps` respect proxy environment variables:

- HTTPS: `HTTPS_PROXY` -> `ALL_PROXY` -> `HTTP_PROXY`
- HTTP: `HTTP_PROXY` -> `ALL_PROXY`
- Bypass: `NO_PROXY` / `no_proxy`

Example:

```bash
HTTPS_PROXY=http://proxy.internal:1080 za tool update docker-compose
```

## Dependency Audit (`za deps`)

`za deps` inspects Rust dependencies and combines ecosystem/maintenance signals.

```bash
# default project (Cargo.toml in cwd)
za deps

# custom manifest + output JSON
za deps --manifest-path ./Cargo.toml --json deps-audit.json

# include dev/build/optional deps
za deps --include-dev --include-build --include-optional

# override workers
za deps --jobs 12
```

Token resolution priority:

1. `--github-token`
2. `GITHUB_TOKEN` / `GH_TOKEN`
3. `za config` persisted value

Set token once:

```bash
# interactive TUI (default)
za config

# non-interactive commands
za config set github-token <TOKEN>
za config get github-token
za config unset github-token
za config path
```

Set global `za run` proxy once (works from any directory):

```bash
za config set run-http-proxy http://127.0.0.1:1080
za config set run-https-proxy http://127.0.0.1:1080
za config set run-no-proxy localhost,127.0.0.1,.corp.local

za run codex
```

Resulting config layout (`za config path`):

```toml
[auth]
github_token = "ghp_xxx"

[run]
http_proxy = "http://127.0.0.1:1080"
https_proxy = "http://127.0.0.1:1080"
no_proxy = "localhost,127.0.0.1,.corp.local"
```

## License

MIT. See `LICENSE`.
