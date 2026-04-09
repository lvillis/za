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
- Keep Codex work sessions alive across SSH clients with tmux-backed attach/resume flows.
- Audit Rust dependencies with governance and maintenance signals before they become incidents.
- Track GitHub Actions progress for the current commit without opening the web UI.
- Run tools with consistent runtime and proxy settings.

## Install

### GitHub Release

Formal release contract:

- Versioned archives named `za-<version>-<target>.tar.gz` on Unix-like targets and `za-<version>-<target>.zip` on Windows
- `SHA256SUMS` covering every uploaded asset for that version
- Stable archive layout: `<asset-stem>/za`, `LICENSE`, `README.md`

Supported release matrix:

| Consumer platform | Rust target | Formal asset | Convenience alias |
| --- | --- | --- | --- |
| `linux/amd64 (musl)` | `x86_64-unknown-linux-musl` | `za-<version>-x86_64-unknown-linux-musl.tar.gz` | `za-linux-amd64` |
| `linux/amd64 (gnu)` | `x86_64-unknown-linux-gnu` | `za-<version>-x86_64-unknown-linux-gnu.tar.gz` | - |
| `linux/arm64 (musl)` | `aarch64-unknown-linux-musl` | `za-<version>-aarch64-unknown-linux-musl.tar.gz` | `za-linux-arm64` |
| `linux/armhf (gnu)` | `arm-unknown-linux-gnueabihf` | `za-<version>-arm-unknown-linux-gnueabihf.tar.gz` | `za-linux-armhf` |
| `linux/arm/v7 (gnu)` | `armv7-unknown-linux-gnueabihf` | `za-<version>-armv7-unknown-linux-gnueabihf.tar.gz` | `za-linux-armv7` |
| `darwin/arm64` | `aarch64-apple-darwin` | `za-<version>-aarch64-apple-darwin.tar.gz` | `za-darwin-arm64` |
| `windows/amd64` | `x86_64-pc-windows-msvc` | `za-<version>-x86_64-pc-windows-msvc.zip` | `za-windows-amd64.exe` |

Manual latest install convenience path:

```bash
curl -fsSL https://github.com/lvillis/za/releases/latest/download/za-linux-amd64 -o /usr/local/bin/za && chmod +x /usr/local/bin/za
```

This `latest` shortcut is intended for manual installs. Pinned automation and CI SHOULD prefer a versioned archive plus `SHA256SUMS`.

Pinned install example:

```bash
VERSION=0.1.44 && curl -fsSLO "https://github.com/lvillis/za/releases/download/${VERSION}/za-${VERSION}-x86_64-unknown-linux-musl.tar.gz" && curl -fsSLO "https://github.com/lvillis/za/releases/download/${VERSION}/SHA256SUMS" && grep "  za-${VERSION}-x86_64-unknown-linux-musl.tar.gz$" SHA256SUMS | sha256sum -c - && tar -xzf "za-${VERSION}-x86_64-unknown-linux-musl.tar.gz" && install -m 0755 "za-${VERSION}-x86_64-unknown-linux-musl/za" /usr/local/bin/za
```

If `/usr/local/bin` is not writable, change the output path to somewhere already on `PATH`, such as `~/.local/bin/za`.

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

### 2) Manage CLI tools

```bash
za tool install codex
za tool install docker-compose
za tool install rg
za tool install fd
za tool install tcping
za tool install dust
za tool install just
za tool install oha
za tool install starship
za tool install ble.sh
za tool install git-cliff
za tool install cargo-release
za tool install cargo-nextest
za tool install cross

za tool ls
za tool ls --outdated
za tool doctor
za tool update codex
za tool show codex
za run codex
```

### 3) Keep Codex sessions alive across devices

```bash
za codex up
za codex -- resume
za codex attach
za codex exec -- bash
za codex resume
za codex top
za codex ps
za codex stop
```

`za codex` uses `tmux` to keep a per-workspace Codex session alive. Start from your Windows IDE terminal, then SSH in from your phone and run `za codex attach` from the same repository to take over the session.

### 4) Audit dependency risk

```bash
za deps
za deps --jobs 16
za deps --fail-on-high
za deps latest serde regex
za deps latest --manifest Cargo.toml --toml
za deps latest --manifest Cargo.toml --suggest
```

### 5) Inspect local ports

```bash
za port ls
za port open 3000
za port who 8080
za port follow 3000
za port stop 3000 --dry-run
za port wait 3000
```

### 6) Manage JetBrains remote IDE sessions

```bash
za ide ps
za ide ps --duplicates
za ide reconcile
za ide reconcile --apply
za ide stop 42589
```

`za ide reconcile` reads policy from config:
- `ide-max-per-project` (default `1`)
- `ide-orphan-ttl-minutes` (default `30`)

### 7) Track GitHub Actions

```bash
za gh ci
za gh ci inspect
za gh ci watch
za gh ci list --repo lvillis/za --repo lvillis/reqx-rs
za gh ci list --group work
za gh ci list --group work --all
```

`za gh ci` inspects the current repository `HEAD` commit. `za gh ci watch` follows that commit until GitHub Actions reaches a terminal state. `za gh ci inspect` drills into failing/cancelled workflows for the current commit. `za gh ci list` can read repo groups from `~/.config/za/ci.toml`.

### 8) Enable GitHub auth for IDE/CLI Git operations

```bash
za gh auth enable
za gh auth status
za gh auth doctor
za gh auth repair
za gh auth test
za gh auth test --repo https://github.com/org/repo.git
```

`za gh auth enable` configures GitHub HTTPS auth through a credential helper (`za gh credential`) so remote URLs can stay clean (`https://github.com/org/repo.git`) without embedding token secrets.
`za gh auth repair` makes the common fixes in one pass: it ensures the `za` credential helper is first for `github.com`, restores the expected username/`useHttpPath` settings, normalizes the current repo remote to clean GitHub HTTPS when possible, and then runs the same auth verification probe.
`za gh auth test` uses an authenticated probe plus an anonymous comparison probe; it only reports verified auth when the anonymous probe is explicitly rejected, and treats network/transport failures as inconclusive. Use a private repo target for strict auth verification.

## Command Map

| Command | Purpose |
| --- | --- |
| `za diff` | Summarize current Git workspace additions/deletions for review. |
| `za completion` | Generate shell completion scripts. |
| `za gen` | Generate project context snapshots (`CONTEXT.md`). |
| `za port` | Inspect local TCP/UDP ownership, follow port changes, and act on visible owners. |
| `za tool` | Tool management with `install`, `update`, `ls`, `doctor`, `show`, and `uninstall`. |
| `za run` | Launch a tool directly with normalized proxy environment variables. |
| `za codex` | Manage long-lived Codex tmux sessions for the current workspace. |
| `za deps` | Audit Rust dependency governance and resolve latest crate versions with upgrade guidance. |
| `za gh` | Unified GitHub shortcuts for auth repair and Actions status. |
| `za config` | Persist CLI config (`[auth]`, `[proxy]`, `[run]`, `[tool]`, `[update]`). |
| `za ide` | Inspect and reconcile JetBrains remote IDE server processes. |

## Workspace Diff

Use `za diff` to review current workspace changes against the current Git state:

```bash
za diff
za diff --files
za diff --json
za diff --unstaged --path 'src/**'
za diff --exclude-risk generated --exclude-risk lockfile
za diff --name-only
```

The terminal report is review-oriented: it shows one merged file list with status markers (`M/A/D/R/?`), a narrow attention marker (`!` high, `~` medium), scope labels (`staged`, `unstaged`, `untracked`), directory-dimmed file paths, rename arrows, colored `+`/`-` counts on TTY output, and a compact 5-block `STAT` diffstat column so larger files stand out immediately without taking over the row. On narrower terminals, scope labels compact automatically and file paths bias toward the trailing path/file name so the part you actually review stays visible; redirected non-TTY output drops the stat bar and stays plain. Risk kinds (`large`, `ci`, `config`, `lock`, `generated`, `binary`) stay in the top `attention` summary and JSON output instead of occupying a wide per-row column. The `large` cutoff is computed from recent Git history (`p90`, clamped to a sane range) and falls back to a fixed threshold when history is too shallow. Use `--path` for gitignore-style path globs, scope flags such as `--staged` / `--unstaged` / `--untracked` to narrow review focus, `--exclude-risk` to hide noisy generated or lockfile churn, and `--name-only` when you only want the review queue. Binary files are counted separately and excluded from `+/-` line totals. `--json` now carries stable filter metadata, risk policy metadata, workspace totals, renamed path fields, and per-file risk tags; use `--json --files` when you also want per-file detail in machine-readable output.

## Shell Completion

Use `za completion <shell>` to emit a shell-native completion script:

```bash
za completion bash
za completion zsh
za completion fish
```

Or let `za` install into common user-level locations:

```bash
za completion install bash
za completion install zsh
za completion install fish
za completion install elvish
za completion install powershell
za completion status bash
za completion doctor zsh
za completion uninstall fish
```

`za completion install` now uses `shellcomp` for install and activation management, while preserving `za`'s legacy marker migration for existing users. Bash prefers the system `bash-completion` loader when a loader is detected and falls back to a managed `~/.bashrc` block otherwise; zsh uses a managed `~/.zshrc` block so `fpath` and `compinit` are wired consistently; PowerShell and Elvish now also have managed profile wiring by default.

Use `--path` when you want a non-default target:

```bash
za completion install powershell --path ~/.config/powershell/completions/za.ps1
```

`za completion status` is the quick activation view. `za completion doctor` expands the same state into target path, availability, location/reason/next-step guidance, and any remaining legacy `za` marker. `za completion uninstall` removes the managed script, managed shell wiring, and any legacy `za completion` block that still exists.

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
za tool ls --supported
za tool ls --supported --json
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
| `bpftop` | - | GitHub Release (SHA-256 verify) |
| `just` | - | GitHub Release (SHA-256 verify) |
| `oha` | - | GitHub Release (SHA-256 verify) |
| `starship` | - | GitHub Release (SHA-256 verify) |
| `ble.sh` | `blesh` | GitHub nightly rolling release (commit-tracked; SHA-256 unavailable) |
| `git-cliff` | - | GitHub Release (SHA-256 verify) |
| `cargo-release` | - | GitHub Release (SHA-256 verify) |
| `cargo-nextest` | - | GitHub Release (SHA-256 verify) |
| `cross` | - | GitHub Release (SHA-256 unavailable; unverified) |

### Common workflows

```bash
# install the latest release and make it active
za tool install codex
za tool install just bpftop starship ble.sh git-cliff cargo-release cargo-nextest cross
za tool install just starship ble.sh --verbose

# install a specific version and make it active
za tool install codex --version 0.105.0

# preview the resolved install/update plan without changing anything
za tool install ble.sh --dry-run
za tool update codex --dry-run

# inspect the current managed state
za tool ls
za tool doctor
za tool show codex
za tool show codex --path

# check for newer upstream releases (human + json)
za tool ls --outdated
za tool ls --outdated --json

# CI-friendly exit codes:
# 20 => updates available
# 21 => update checks failed
za tool ls --outdated --fail-on-updates
za tool ls --outdated --fail-on-check-errors

# update one tool or refresh everything managed in this scope
za tool update codex
za tool update
za tool update --all
za tool update --all --verbose

# sync from a manifest
za tool sync
za tool sync --dry-run --verbose

# adopt an existing unmanaged binary already on disk
za tool install codex --adopt

# uninstall one version or all managed versions
za tool uninstall codex --version 0.104.0
za tool uninstall codex
```

`za tool install starship` also upserts a managed `~/.bashrc` block that initializes the prompt only when `TERMINAL_EMULATOR=JetBrains-JediTerm`. Installation does not patch the current shell process; open a new JetBrains bash shell or `source ~/.bashrc` after install.

`za tool install ble.sh` tracks the upstream rolling `nightly` release instead of the stale stable release. It installs `ble.sh` as a managed package payload, keeps a stable active path under the tool state directory, and upserts JetBrains-only top/bottom `~/.bashrc` blocks for the current user so `source -- .../ble.sh --attach=none` and `ble-attach` stay in sync across updates, regardless of whether the managed payload lives in system or user scope.

`za tool install --dry-run` and `za tool update --dry-run` resolve the target version and source policy, preview activation/shell-init changes, and make no filesystem changes.

Multi-tool `za tool install`, `za tool update`, and `za tool sync` default to compact batch output: they show the plan summary plus changed/repaired/failed entries only. Use `--verbose` to expand per-tool `resolve` / `source` / `install` / `activate` / `prune` stages.
`za tool doctor` is the read-only health check for managed state: it verifies active version pointers, payload presence, manifests, and active paths across all managed tools in the current scope, then highlights anything that needs repair.

`za tool install` and `za tool update` are interruption-safe: pressing `Ctrl+C` aborts cleanly and temporary download directories are removed automatically (stale leftovers are cleaned on next run). Output is stage-oriented (`resolve`, `source`, `install`, `activate`, `prune`) so it is obvious where a run is spending time.
For large GitHub release assets, `za` will use parallel HTTP range downloads when the upstream supports it, emit explicit `download` / `verify` / `extract` stages, and automatically fall back to a single stream otherwise.

`za deps` defaults to a compact audit view: one verdict line, an `attention` section for high/medium/unknown findings, and a low-risk summary hint. Use `za deps --verbose` to include the low-risk baseline inventory as well. `za deps latest` resolves the latest stable crates.io version for explicit crate names or a manifest's direct dependencies, can emit copy-pastable TOML with `--toml`, and can add upgrade guidance with `--suggest` so you can see whether the current manifest already covers the latest release, needs a same-line bump, or deserves manual review.

Human-readable commands honor `--color auto|always|never` and `NO_COLOR`, so CI logs and redirected output can stay plain while interactive terminals keep semantic status colors.

### Existing binaries adoption

If a supported unmanaged binary is already present in scope bin path (for example `/usr/local/bin/codex`), use `za tool install <tool> --adopt` to move it into the managed store and make it active.

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

## Local Ports (`za port`)

Use `za port` to inspect local TCP/UDP ownership from `/proc` on Linux:

```bash
# default listening/bound view
za port ls

# quick readiness check
za port open 3000

# who owns one port
za port who 8080

# stream changes only when something moves
za port follow 3000

# preview visible owners before stopping them
za port stop 3000 --dry-run

# wait for a service port to come up
za port wait 3000
```

`za port ls` keeps the broad table view. `za port open` is the quick readiness check with a clean exit code, `za port who` narrows that same scan to one local port, `za port follow` streams ownership/state changes only when something actually changes, `za port stop` targets the currently visible owners of a port (use `--dry-run` first), and `za port wait` polls until a matching listener/bound socket appears, which is useful in scripts and local startup flows.

### Managed Codex Sessions (`za codex`)

Use `za codex` when you want a durable Codex work session that survives SSH disconnects and can be reattached from another device. `tmux` is required.

```bash
# create or attach the current workspace session
za codex up

# bare `za codex` is equivalent to `za codex up`
za codex

# force a fresh managed startup path with explicit Codex args
za codex -- resume
za codex -- resume --last

# take over the session from another terminal/device
za codex attach

# open another tmux window in the same session
za codex exec -- bash
za codex exec -- git status

# recreate the managed session by resuming the last Codex conversation
za codex resume

# inspect and watch managed sessions
za codex top
za codex top --all --history
za codex ps

# stop the current workspace session
za codex stop
```

Behavior notes:

- Session name is stable per workspace root, so any terminal in the same repo resolves to the same tmux session.
- `za codex up` launches the active managed `codex` binary with `--no-alt-screen`.
- `za codex -- <codex args>` is treated as an explicit startup request, not an attach. If a managed session already exists for the workspace, `za` recreates it first and then launches `codex --no-alt-screen <args>`.
- `za codex attach` uses `tmux attach -d` semantics outside tmux, so one device can cleanly take over the session from another.
- `za codex exec` creates a new tmux window inside the existing session; its exit code reflects tmux window creation, not the spawned command result.
- `za codex resume` starts a managed tmux session running `codex resume --last` when no session exists yet.
- `za codex top` opens a live terminal dashboard for managed Codex sessions. By default it scopes to the current workspace and active sessions only; `--all` includes every local workspace and `--history` keeps inactive sessions visible.
- While `za codex top` is open, new managed `za codex` launches automatically get a temporary local OTLP listener injected so the dashboard can show live event, API, and tool-failure counters without extra setup. Explicit user OTEL config still wins.
- `za codex top` hotkeys: `j`/`k` move, `PgUp`/`PgDn` page, `a` toggles current-workspace vs all-workspaces scope, `h` toggles history, `Enter` opens the selected session's event stream, `f` toggles follow mode in the stream view, `0`/`a`/`s`/`t`/`e` filter stream events to all/API/SSE/tool/error, `[`/`]` scroll event detail, `{`/`}` page event detail, `Esc` returns, `q` quits.
- `za codex ps` now surfaces the Codex session id plus the same `MODEL`, `EFFORT`, and remaining context percentage (`LEFT%`) shown in the Codex TUI by reading the latest `token_count` event in the local Codex session log, with older TUI sampling logs kept only as a compatibility fallback.
- If `tmux` is not installed, `za codex ps` still shows locally recorded sessions as `unavailable`, and `za codex stop` degrades to local metadata cleanup instead of failing with an opaque error.

### Proxy behavior

`za run`, `za codex`, `za tool`, `za update`, `za deps`, and `za gh ci` respect proxy settings:

- HTTPS: `HTTPS_PROXY` -> `ALL_PROXY` -> `HTTP_PROXY`
- HTTP: `HTTP_PROXY` -> `ALL_PROXY`
- Bypass: `NO_PROXY` / `no_proxy`
- Config scopes: `za deps` and `za gh ci` use `[proxy]` defaults; `za run` / `za codex` / `za tool` / `za update` additionally honor `[run]` / `[tool]` / `[update]`

Example:

```bash
HTTPS_PROXY=http://proxy.internal:1080 za tool update docker-compose
```

## Dependency Audit (`za deps`)

`za deps` inspects Rust dependencies and combines governance and maintenance signals, including yanked latest releases, license metadata, MSRV declarations, crates.io freshness, and GitHub activity. By default it audits the direct dependencies currently activated by Cargo's resolved feature graph for the target manifest; add `--include-optional` to also include optional direct dependencies that are declared but inactive.

```bash
# default project (Cargo.toml in cwd)
za deps

# custom manifest + output JSON
za deps --manifest-path ./Cargo.toml --json deps-audit.json

# include dev/build/optional deps
za deps --include-dev --include-build --include-optional

# override workers
za deps --jobs 12

# resolve latest stable versions from crates.io
za deps latest serde regex
za deps latest --manifest Cargo.toml --toml
za deps latest --manifest Cargo.toml --suggest
```

Token resolution priority:

1. `--github-token`
2. `GITHUB_TOKEN` / `GH_TOKEN`
3. `za config` persisted value

## GitHub CI (`za gh ci`)

`za gh ci` reports GitHub Actions state for the current repository `HEAD` commit. It aggregates workflow runs for the same `head_sha`, so the first screen answers the question you usually care about after a push: did this commit pass yet? `za gh ci watch` also streams the currently active workflows while a commit is still pending or running. `za gh ci inspect` drills into failing/cancelled workflows and shows workflow URLs, job URLs, and attention steps for the current commit. `za gh ci list` keeps a short local cache for recent GitHub API responses and hides clean green repos by default when the board has active or failing targets; pass `--all` when you want the full board.

```bash
# current repo HEAD
za gh ci

# inspect failing/cancelled workflows, jobs, and steps
za gh ci inspect

# wait until terminal state
za gh ci watch --timeout-secs 900

# inspect explicit repos or local clones
za gh ci list --repo lvillis/za --repo /code/reqx-rs

# read a named repo group
za gh ci list --group work

# include clean green repos too
za gh ci list --group work --all
```

Repo groups live in `~/.config/za/ci.toml` by default:

```toml
[groups.work]
repos = [
  "/code/za",
  "/code/reqx-rs",
  "lvillis/some-other-repo",
]
```

Token resolution priority:

1. `--github-token`
2. `ZA_GITHUB_TOKEN`
3. `GITHUB_TOKEN` / `GH_TOKEN`
4. `za config` persisted value

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

Interactive `za config` keymap:

- Move: `Up/Down`, `j/k`, `PgUp/PgDn`, `Home/End`
- Edit selected: `Enter` or `e`
- Unset selected: `u`
- Save edit: `Enter` (empty value unsets)
- Cancel edit: `Esc`
- Toggle help panel: `?` or `F1`
- Quit: `q`

Set global proxy defaults once (works from any directory):

```bash
za config set proxy-http http://127.0.0.1:1080
za config set proxy-https http://127.0.0.1:1080
za config set proxy-no-proxy localhost,127.0.0.1,.corp.local

# optional scope override
za config set tool-https http://127.0.0.1:1080

# ide policy defaults (optional overrides)
za config set ide-max-per-project 1
za config set ide-orphan-ttl-minutes 30

za run codex
za tool update codex
za update
```

Resulting config layout (`za config path`):

```toml
[auth]
github_token = "ghp_xxx"

[proxy]
http_proxy = "http://127.0.0.1:1080"
https_proxy = "http://127.0.0.1:1080"
no_proxy = "localhost,127.0.0.1,.corp.local"

[run]

[tool]
https_proxy = "http://127.0.0.1:1080"

[update]

[ide.jetbrains]
max_per_project = "1"
orphan_ttl_minutes = "30"
```

## License

MIT. See `LICENSE`.
