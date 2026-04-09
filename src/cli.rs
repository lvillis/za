use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// Top-level CLI parser
#[derive(Parser)]
#[command(name = "za", version)]
pub struct Cli {
    /// Control ANSI color output for human-readable commands.
    #[arg(long, global = true, value_enum, default_value_t = ColorWhen::Auto)]
    pub color: ColorWhen,
    #[command(subcommand)]
    pub cmd: Commands,
}

/// Sub-command definitions
#[derive(Subcommand)]
pub enum Commands {
    /// Print or install shell completions
    Completion {
        #[command(subcommand)]
        cmd: CompletionCommands,
    },
    /// Review current Git workspace changes
    Diff {
        /// Open the continuous review TUI.
        #[arg(long, conflicts_with_all = ["json", "files", "name_only"])]
        tui: bool,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
        /// Include per-file additions/deletions in JSON output.
        #[arg(long)]
        files: bool,
        /// Only print status/scope/path rows, without numeric diff columns.
        #[arg(long)]
        name_only: bool,
        /// Only include staged changes.
        #[arg(long)]
        staged: bool,
        /// Only include unstaged changes.
        #[arg(long)]
        unstaged: bool,
        /// Only include untracked changes.
        #[arg(long)]
        untracked: bool,
        /// Restrict results to paths matching this gitignore-style glob. Repeatable.
        #[arg(long, value_name = "GLOB")]
        path: Vec<String>,
        /// Only include files matching these change kinds. Repeatable.
        #[arg(long, value_enum, value_name = "KIND")]
        kind: Vec<DiffKindFilter>,
        /// Hide files carrying the selected review risk tag. Repeatable.
        #[arg(long, value_enum, value_name = "RISK")]
        exclude_risk: Vec<DiffRiskFilter>,
    },
    /// Generate a project context snapshot
    Gen {
        #[arg(long, default_value_t = crate::command::DEFAULT_MAX_LINES_PER_FILE)]
        max_lines: usize,
        #[arg(long, default_value = "CONTEXT.md")]
        output: PathBuf,
        #[arg(long)]
        include_binary: bool,
        /// Optional GitHub repository URL, e.g. https://github.com/owner/repo
        #[arg(long)]
        repo: Option<String>,
        /// Optional ref for remote snapshot (branch/tag/commit). Defaults to `HEAD`.
        #[arg(long, value_name = "REF")]
        r#ref: Option<String>,
    },
    /// Audit Rust dependency risk and maintenance signals
    Deps {
        #[command(flatten)]
        audit: DepsAuditArgs,
        #[command(subcommand)]
        cmd: Option<DepsCommands>,
    },
    /// Inspect local TCP/UDP port bindings
    Port {
        #[command(subcommand)]
        cmd: PortCommands,
    },
    /// Manage CLI tools in the current scope
    Tool {
        /// Use user-level paths (`~/.local/...`) instead of system-level paths.
        #[arg(long)]
        user: bool,
        #[command(subcommand)]
        cmd: ToolCommands,
    },
    /// Run a tool with normalized proxy settings
    Run {
        /// Tool name, e.g. `codex`
        tool: String,
        /// Arguments passed through to the tool
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Manage tmux-backed Codex work sessions
    Codex {
        #[command(subcommand)]
        cmd: Option<CodexCommands>,
        /// Arguments passed through to `codex` when prefixed by `--`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Update the za binary
    Update {
        /// Install to user-level paths (`~/.local/...`) instead of system-level paths.
        #[arg(long)]
        user: bool,
        /// Only check whether an update is available.
        #[arg(long)]
        check: bool,
        /// Target version (defaults to latest release).
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
    },
    /// Manage persisted za configuration
    Config {
        #[command(subcommand)]
        cmd: Option<ConfigCommands>,
    },
    /// Manage JetBrains remote IDE sessions
    Ide {
        #[command(subcommand)]
        cmd: IdeCommands,
    },
    /// Run GitHub auth and CI shortcuts
    #[command(visible_alias = "github")]
    Gh {
        #[command(subcommand)]
        cmd: GhCommands,
    },
}

/// `za tool` sub-commands
#[derive(Subcommand)]
pub enum ToolCommands {
    /// Install one or more tools and make them active in this scope
    #[command(alias = "pull")]
    Install {
        /// Tool names, e.g. `codex just`
        #[arg(required = true, num_args = 1.., value_name = "TOOL")]
        tools: Vec<String>,
        /// Install a specific version instead of the latest release. Requires exactly one tool.
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
        /// Adopt an existing unmanaged binary already present in this scope. Requires exactly one tool.
        #[arg(long)]
        adopt: bool,
        /// Preview the resolved install plan without downloading or changing any files.
        #[arg(long)]
        dry_run: bool,
        /// Print per-tool resolution and stage details.
        #[arg(long)]
        verbose: bool,
    },
    /// Diagnose managed tool state and repair hints
    Doctor {
        /// Tool names. Omit to inspect all managed tools in this scope.
        tools: Vec<String>,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
    },
    /// List managed tools and availability in this scope
    #[command(name = "ls", alias = "list")]
    Ls {
        /// Tool names. Only valid together with `--outdated`.
        tools: Vec<String>,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
        /// List built-in supported tools and source policies.
        #[arg(long)]
        supported: bool,
        /// Check managed tools for newer upstream versions.
        #[arg(long)]
        outdated: bool,
        /// Return non-zero when updates are available. Requires `--outdated`.
        #[arg(long)]
        fail_on_updates: bool,
        /// Return non-zero when update checks fail. Requires `--outdated`.
        #[arg(long)]
        fail_on_check_errors: bool,
    },
    /// Update tools to the newest available version
    Update {
        /// Update all managed tools in this scope.
        #[arg(long)]
        all: bool,
        /// Tool names. Omit to update all managed tools in this scope.
        tools: Vec<String>,
        /// Pin the update target to a specific version. Requires exactly one tool.
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
        /// Preview the resolved update plan without downloading or changing any files.
        #[arg(long)]
        dry_run: bool,
        /// Print per-tool resolution and stage details.
        #[arg(long)]
        verbose: bool,
    },
    /// Show detailed managed state for one tool
    #[command(alias = "inspect")]
    Show {
        /// Tool name, e.g. `codex`
        tool: String,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
        /// Only print the active managed executable path.
        #[arg(long)]
        path: bool,
    },
    /// Sync tool versions from a manifest
    Sync {
        /// Manifest path.
        #[arg(long, value_name = "PATH", default_value = "za.tools.toml")]
        file: PathBuf,
        /// Preview the resolved sync plan without downloading or changing any files.
        #[arg(long)]
        dry_run: bool,
        /// Print per-tool resolution and stage details.
        #[arg(long)]
        verbose: bool,
    },
    /// Remove one or all installed tool versions
    #[command(alias = "rm")]
    Uninstall {
        /// Tool name, e.g. `codex`
        tool: String,
        /// Remove only one version instead of all managed versions.
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
    },
    /// Print the active managed executable path for one tool
    #[command(hide = true)]
    Which {
        /// Tool name, e.g. `codex`
        tool: String,
    },
    /// List built-in supported tools and source policies
    #[command(hide = true)]
    Catalog {
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
    },
    /// Check installed tools for newer upstream versions
    #[command(hide = true)]
    Outdated {
        /// Tool names. Omit to check all managed tools in this scope.
        tools: Vec<String>,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
        /// Return non-zero when updates are available.
        #[arg(long)]
        fail_on_updates: bool,
        /// Return non-zero when update checks fail.
        #[arg(long)]
        fail_on_check_errors: bool,
    },
    /// Adopt an existing unmanaged binary already present in this scope
    #[command(hide = true)]
    Adopt {
        /// Tool name, e.g. `codex`
        tool: String,
    },
}

/// `za port` sub-commands
#[derive(Subcommand)]
pub enum PortCommands {
    /// List local TCP/UDP ports and owning processes
    #[command(name = "ls", alias = "list")]
    Ls {
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
        /// Include connected and non-listening sockets, not just listening/bound ports.
        #[arg(long)]
        all: bool,
        /// Filter by local port. Repeatable.
        #[arg(long, value_name = "PORT")]
        port: Vec<u16>,
        /// Filter by owning PID. Repeatable.
        #[arg(long, value_name = "PID", value_parser = clap::value_parser!(u32).range(1..))]
        pid: Vec<u32>,
        /// Only include TCP sockets.
        #[arg(long)]
        tcp: bool,
        /// Only include UDP sockets.
        #[arg(long)]
        udp: bool,
    },
    /// Show who currently owns a local port
    Who {
        /// Local port to inspect.
        port: u16,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
        /// Include connected and non-listening sockets, not just listening/bound ports.
        #[arg(long)]
        all: bool,
        /// Only include TCP sockets.
        #[arg(long)]
        tcp: bool,
        /// Only include UDP sockets.
        #[arg(long)]
        udp: bool,
    },
    /// Exit zero when a local port currently has at least one visible socket
    Open {
        /// Local port to inspect.
        port: u16,
        /// Include connected and non-listening sockets, not just listening/bound ports.
        #[arg(long)]
        all: bool,
        /// Only include TCP sockets.
        #[arg(long)]
        tcp: bool,
        /// Only include UDP sockets.
        #[arg(long)]
        udp: bool,
    },
    /// Send a signal to processes currently owning a local port
    Stop {
        /// Local port to inspect.
        port: u16,
        /// Signal to send to visible owning processes.
        #[arg(long, value_enum, default_value_t = PortSignal::Term)]
        signal: PortSignal,
        /// Preview target processes without sending a signal.
        #[arg(long)]
        dry_run: bool,
        /// Include connected and non-listening sockets, not just listening/bound ports.
        #[arg(long)]
        all: bool,
        /// Only include TCP sockets.
        #[arg(long)]
        tcp: bool,
        /// Only include UDP sockets.
        #[arg(long)]
        udp: bool,
    },
    /// Follow local port ownership/state changes
    Follow {
        /// Local port to inspect.
        port: u16,
        /// Stop following after this many seconds.
        #[arg(long, value_name = "SECS")]
        timeout_secs: Option<u64>,
        /// Poll interval in milliseconds.
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
        /// Include connected and non-listening sockets, not just listening/bound ports.
        #[arg(long)]
        all: bool,
        /// Only include TCP sockets.
        #[arg(long)]
        tcp: bool,
        /// Only include UDP sockets.
        #[arg(long)]
        udp: bool,
    },
    /// Wait until a local port becomes available
    Wait {
        /// Local port to wait for.
        port: u16,
        /// Stop waiting after this many seconds.
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
        /// Poll interval in milliseconds.
        #[arg(long, default_value_t = 500)]
        interval_ms: u64,
        /// Include connected and non-listening sockets, not just listening/bound ports.
        #[arg(long)]
        all: bool,
        /// Only include TCP sockets.
        #[arg(long)]
        tcp: bool,
        /// Only include UDP sockets.
        #[arg(long)]
        udp: bool,
    },
}

/// `za completion` sub-commands
#[derive(Subcommand)]
pub enum CompletionCommands {
    /// Print Bash completion script
    Bash,
    /// Print Zsh completion script
    Zsh,
    /// Print Fish completion script
    Fish,
    /// Print Elvish completion script
    Elvish,
    /// Print PowerShell completion script
    Powershell,
    /// Install a completion script into a user-level path
    Install {
        #[arg(value_enum)]
        shell: CompletionShell,
        /// Override the install path instead of using the default shell-specific location.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Show current completion activation status
    Status {
        #[arg(value_enum)]
        shell: CompletionShell,
        /// Inspect an explicit completion path instead of the shell-managed default path.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Diagnose completion wiring and next steps
    Doctor {
        #[arg(value_enum)]
        shell: CompletionShell,
        /// Inspect an explicit completion path instead of the shell-managed default path.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Remove a previously installed completion script and managed wiring
    Uninstall {
        #[arg(value_enum)]
        shell: CompletionShell,
        /// Remove an explicit completion path instead of the shell-managed default path.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
    },
}

#[derive(Args, Clone, Debug, Default)]
pub struct DepsAuditArgs {
    /// Optional path to Cargo.toml (defaults to current workspace root).
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    /// Optional GitHub token override for this run.
    #[arg(long, value_name = "TOKEN")]
    pub github_token: Option<String>,
    /// Number of concurrent workers for API queries (default: auto, based on CPU count).
    #[arg(long, value_name = "JOBS")]
    pub jobs: Option<usize>,
    /// Include dev-dependencies in audit.
    #[arg(long)]
    pub include_dev: bool,
    /// Include build-dependencies in audit.
    #[arg(long)]
    pub include_build: bool,
    /// Also include optional dependencies that are not active in the current resolved feature set.
    #[arg(long)]
    pub include_optional: bool,
    /// Write full audit report to JSON.
    #[arg(long, value_name = "PATH")]
    pub json: Option<PathBuf>,
    /// Exit with non-zero status when any high-risk dependency is found.
    #[arg(long)]
    pub fail_on_high: bool,
    /// Print low-risk entries in addition to attention items.
    #[arg(long)]
    pub verbose: bool,
}

#[derive(Subcommand)]
pub enum DepsCommands {
    /// Resolve latest stable versions for one or more crates
    Latest {
        /// Crate names to resolve. Omit only when `--manifest-path` is used.
        #[arg(value_name = "CRATE")]
        crates: Vec<String>,
        /// Optional path to Cargo.toml used to source crate names.
        #[arg(long, alias = "manifest", value_name = "PATH")]
        manifest_path: Option<PathBuf>,
        /// Number of concurrent workers for API queries (default: auto, based on CPU count).
        #[arg(long, value_name = "JOBS")]
        jobs: Option<usize>,
        /// Include dev-dependencies when `--manifest-path` is used.
        #[arg(long)]
        include_dev: bool,
        /// Include build-dependencies when `--manifest-path` is used.
        #[arg(long)]
        include_build: bool,
        /// Also include optional dependencies when `--manifest-path` is used.
        #[arg(long)]
        include_optional: bool,
        /// Print JSON output for scripting.
        #[arg(long, conflicts_with = "toml")]
        json: bool,
        /// Print copy-pastable TOML dependency entries.
        #[arg(long, conflicts_with = "json")]
        toml: bool,
        /// Add upgrade guidance based on current manifest requirements.
        #[arg(long, conflicts_with = "toml")]
        suggest: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ColorWhen {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum PortSignal {
    Term,
    Kill,
    Int,
}

/// `za config` sub-commands
#[derive(Subcommand)]
pub enum ConfigCommands {
    /// Show active config file path
    Path,
    /// Set a config value
    Set {
        #[arg(value_enum)]
        key: ConfigKey,
        value: String,
    },
    /// Get a config value
    Get {
        #[arg(value_enum)]
        key: ConfigKey,
        /// Print raw value (for scripting). Use with care.
        #[arg(long)]
        raw: bool,
    },
    /// Remove a config value
    Unset {
        #[arg(value_enum)]
        key: ConfigKey,
    },
}

/// `za ide` sub-commands
#[derive(Subcommand)]
pub enum IdeCommands {
    /// List JetBrains remote IDE server processes
    Ps {
        /// Only show projects with duplicate server instances.
        #[arg(long)]
        duplicates: bool,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
    },
    /// Stop one JetBrains server process by PID
    Stop {
        /// Target server PID.
        pid: i32,
        /// Graceful shutdown timeout before SIGKILL.
        #[arg(long, default_value_t = 5)]
        timeout_secs: u64,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
    },
    /// Reconcile duplicate server processes for the same IDE+project
    Reconcile {
        /// Apply actions. Without this flag, only print the plan.
        #[arg(long)]
        apply: bool,
        /// Keep strategy when multiple server processes exist.
        #[arg(long, value_enum, default_value_t = IdeReconcileStrategy::Newest)]
        keep: IdeReconcileStrategy,
        /// Graceful shutdown timeout before SIGKILL.
        #[arg(long, default_value_t = 5)]
        timeout_secs: u64,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum IdeReconcileStrategy {
    Newest,
    Oldest,
}

/// `za codex` sub-commands
#[derive(Subcommand)]
pub enum CodexCommands {
    /// Create or attach the current workspace Codex tmux session
    Up {
        /// Arguments passed through to `codex`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Attach to the current workspace Codex tmux session
    Attach,
    /// Open a new tmux window inside the current workspace Codex session
    Exec {
        /// Command to run inside the existing tmux session
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Start a managed session by resuming the most recent Codex conversation
    Resume {
        /// Arguments passed through to `codex resume --last`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// List managed Codex tmux sessions
    Ps {
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
    },
    /// Watch Codex sessions in a live terminal dashboard
    Top {
        /// Show all local Codex sessions instead of filtering to the current workspace.
        #[arg(long)]
        all: bool,
        /// Include historical sessions that are no longer active.
        #[arg(long)]
        history: bool,
    },
    /// Stop the current workspace Codex tmux session
    Stop {
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
    },
}

/// `za gh ci` sub-commands
#[derive(Subcommand)]
pub enum CiCommands {
    /// Watch current commit CI until it reaches a terminal state
    Watch {
        /// Stop waiting after this many seconds.
        #[arg(long, value_name = "SECS")]
        timeout_secs: Option<u64>,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
        /// Optional GitHub token override for this run.
        #[arg(long, value_name = "TOKEN")]
        github_token: Option<String>,
    },
    /// List CI status across repos from args or a group manifest
    List {
        /// Group name from `~/.config/za/ci.toml` (or `--file`).
        #[arg(long, value_name = "GROUP")]
        group: Option<String>,
        /// Repo slug, GitHub URL, or local repo path. Repeat to add more targets.
        #[arg(long, value_name = "REPO_OR_PATH")]
        repo: Vec<String>,
        /// CI manifest path. Defaults to `~/.config/za/ci.toml`.
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
        /// Show all targets, including clean green repos.
        #[arg(long)]
        all: bool,
        /// Optional GitHub token override for this run.
        #[arg(long, value_name = "TOKEN")]
        github_token: Option<String>,
    },
    /// Drill into failing workflows, jobs, and steps for the current commit
    Inspect {
        /// Include successful and skipped workflows instead of only failed/cancelled ones.
        #[arg(long)]
        all: bool,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
        /// Optional GitHub token override for this run.
        #[arg(long, value_name = "TOKEN")]
        github_token: Option<String>,
    },
}

/// `za gh` sub-commands
#[derive(Subcommand)]
pub enum GhCommands {
    /// Manage GitHub credential-helper wiring
    Auth {
        #[command(subcommand)]
        cmd: GitAuthCommands,
    },
    /// Inspect GitHub Actions progress for the current commit or repo groups
    Ci {
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
        /// Optional GitHub token override for this run.
        #[arg(long, value_name = "TOKEN")]
        github_token: Option<String>,
        #[command(subcommand)]
        cmd: Option<CiCommands>,
    },
    /// Internal credential helper entrypoint used by Git
    #[command(hide = true)]
    Credential {
        /// Git credential helper operation: get/store/erase
        operation: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum GitAuthCommands {
    /// Enable GitHub credential helper via za
    Enable,
    /// Show current GitHub credential helper status
    Status {
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
    },
    /// Diagnose common GitHub auth wiring issues
    Doctor {
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
    },
    /// Repair GitHub credential-helper wiring and normalize the current repo remote URL
    Repair {
        /// Remote name used when repairing the current repo remote URL.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Timeout for the verification probe.
        #[arg(long, default_value_t = 15)]
        timeout_secs: u64,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
    },
    /// Run a real GitHub auth connectivity test against a repo
    Test {
        /// Target repository URL. If omitted, use current repo remote URL.
        #[arg(long, value_name = "URL")]
        repo: Option<String>,
        /// Remote name used when `--repo` is omitted.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Timeout for the probe request.
        #[arg(long, default_value_t = 15)]
        timeout_secs: u64,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
    },
    /// Disable GitHub credential helper wiring added by za
    Disable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ConfigKey {
    #[value(name = "github-token")]
    GithubToken,
    #[value(name = "proxy-http")]
    ProxyHttp,
    #[value(name = "proxy-https")]
    ProxyHttps,
    #[value(name = "proxy-all")]
    ProxyAll,
    #[value(name = "proxy-no-proxy")]
    ProxyNoProxy,
    #[value(name = "run-http")]
    RunHttp,
    #[value(name = "run-https")]
    RunHttps,
    #[value(name = "run-all")]
    RunAll,
    #[value(name = "run-no-proxy")]
    RunNoProxy,
    #[value(name = "tool-http")]
    ToolHttp,
    #[value(name = "tool-https")]
    ToolHttps,
    #[value(name = "tool-all")]
    ToolAll,
    #[value(name = "tool-no-proxy")]
    ToolNoProxy,
    #[value(name = "update-http")]
    UpdateHttp,
    #[value(name = "update-https")]
    UpdateHttps,
    #[value(name = "update-all")]
    UpdateAll,
    #[value(name = "update-no-proxy")]
    UpdateNoProxy,
    #[value(name = "ide-max-per-project")]
    IdeMaxPerProject,
    #[value(name = "ide-orphan-ttl-minutes")]
    IdeOrphanTtlMinutes,
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum DiffRiskFilter {
    Binary,
    Ci,
    Config,
    Generated,
    Large,
    Lockfile,
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum DiffKindFilter {
    Code,
    Test,
    Docs,
    Config,
    Generated,
    Binary,
    Other,
}

#[cfg(test)]
mod tests {
    use super::{
        CiCommands, Cli, CodexCommands, ColorWhen, Commands, CompletionCommands, CompletionShell,
        DepsCommands, DiffKindFilter, DiffRiskFilter, GhCommands, GitAuthCommands, PortCommands,
        PortSignal, ToolCommands,
    };
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn completion_parses_generate_shell_subcommand() {
        let cli = Cli::try_parse_from(["za", "completion", "zsh"]).expect("must parse");
        match cli.cmd {
            Commands::Completion { cmd } => assert!(matches!(cmd, CompletionCommands::Zsh)),
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn completion_install_parses_shell_and_path() {
        let cli = Cli::try_parse_from([
            "za",
            "completion",
            "install",
            "fish",
            "--path",
            "/tmp/za.fish",
        ])
        .expect("must parse");
        match cli.cmd {
            Commands::Completion { cmd } => match cmd {
                CompletionCommands::Install { shell, path } => {
                    assert_eq!(shell, CompletionShell::Fish);
                    assert_eq!(path, Some(PathBuf::from("/tmp/za.fish")));
                }
                _ => panic!("unexpected completion command"),
            },
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn global_color_flag_parses_before_subcommand() {
        let cli = Cli::try_parse_from(["za", "--color", "never", "deps"]).expect("must parse");
        assert_eq!(cli.color, ColorWhen::Never);
        assert!(matches!(cli.cmd, Commands::Deps { .. }));
    }

    #[test]
    fn tool_install_parses_version_flag() {
        let cli = Cli::try_parse_from(["za", "tool", "install", "codex", "--version", "0.105.0"])
            .expect("must parse");
        match cli.cmd {
            Commands::Tool { user, cmd } => {
                assert!(!user);
                assert!(matches!(
                    cmd,
                    ToolCommands::Install {
                        tools,
                        version: Some(version),
                        adopt: false,
                        dry_run: false,
                        verbose: false,
                    } if tools == vec!["codex"] && version == "0.105.0"
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn tool_install_parses_multiple_tools() {
        let cli =
            Cli::try_parse_from(["za", "tool", "install", "just", "cross"]).expect("must parse");
        match cli.cmd {
            Commands::Tool { user, cmd } => {
                assert!(!user);
                assert!(matches!(
                    cmd,
                    ToolCommands::Install {
                        tools,
                        version: None,
                        adopt: false,
                        dry_run: false,
                        verbose: false,
                    } if tools == vec!["just", "cross"]
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn tool_update_parses_multiple_tools() {
        let cli = Cli::try_parse_from(["za", "tool", "--user", "update", "codex", "rg"])
            .expect("must parse");
        match cli.cmd {
            Commands::Tool { user, cmd } => {
                assert!(user);
                assert!(matches!(
                    cmd,
                    ToolCommands::Update {
                        all: false,
                        tools,
                        version: None,
                        dry_run: false,
                        verbose: false,
                    } if tools == vec!["codex", "rg"]
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn tool_outdated_parses_policy_flags() {
        let cli = Cli::try_parse_from([
            "za",
            "tool",
            "ls",
            "--json",
            "--outdated",
            "--fail-on-updates",
            "codex",
        ])
        .expect("must parse");
        match cli.cmd {
            Commands::Tool { cmd, .. } => {
                assert!(matches!(
                    cmd,
                    ToolCommands::Ls {
                        tools,
                        json: true,
                        supported: false,
                        outdated: true,
                        fail_on_updates: true,
                        fail_on_check_errors: false,
                    } if tools == vec!["codex"]
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn tool_show_parses_path_flag() {
        let cli =
            Cli::try_parse_from(["za", "tool", "show", "codex", "--path"]).expect("must parse");
        match cli.cmd {
            Commands::Tool { cmd, .. } => {
                assert!(matches!(
                    cmd,
                    ToolCommands::Show {
                        tool,
                        json: false,
                        path: true,
                    } if tool == "codex"
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn tool_install_parses_adopt_flag() {
        let cli =
            Cli::try_parse_from(["za", "tool", "install", "codex", "--adopt"]).expect("must parse");
        match cli.cmd {
            Commands::Tool { cmd, .. } => {
                assert!(matches!(
                    cmd,
                    ToolCommands::Install {
                        tools,
                        version: None,
                        adopt: true,
                        dry_run: false,
                        verbose: false,
                    } if tools == vec!["codex"]
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn tool_install_parses_dry_run_flag() {
        let cli = Cli::try_parse_from(["za", "tool", "install", "ble.sh", "--dry-run"])
            .expect("must parse");
        match cli.cmd {
            Commands::Tool { cmd, .. } => {
                assert!(matches!(
                    cmd,
                    ToolCommands::Install {
                        tools,
                        version: None,
                        adopt: false,
                        dry_run: true,
                        verbose: false,
                    } if tools == vec!["ble.sh"]
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn tool_install_parses_verbose_flag() {
        let cli = Cli::try_parse_from(["za", "tool", "install", "just", "cross", "--verbose"])
            .expect("must parse");
        match cli.cmd {
            Commands::Tool { cmd, .. } => {
                assert!(matches!(
                    cmd,
                    ToolCommands::Install {
                        tools,
                        version: None,
                        adopt: false,
                        dry_run: false,
                        verbose: true,
                    } if tools == vec!["just", "cross"]
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn tool_update_parses_dry_run_flag() {
        let cli = Cli::try_parse_from(["za", "tool", "update", "codex", "--dry-run"])
            .expect("must parse");
        match cli.cmd {
            Commands::Tool { cmd, .. } => {
                assert!(matches!(
                    cmd,
                    ToolCommands::Update {
                        all: false,
                        tools,
                        version: None,
                        dry_run: true,
                        verbose: false,
                    } if tools == vec!["codex"]
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn tool_update_parses_all_flag() {
        let cli = Cli::try_parse_from(["za", "tool", "update", "--all"]).expect("must parse");
        match cli.cmd {
            Commands::Tool { user, cmd } => {
                assert!(!user);
                assert!(matches!(
                    cmd,
                    ToolCommands::Update {
                        all: true,
                        tools,
                        version: None,
                        dry_run: false,
                        verbose: false,
                    } if tools.is_empty()
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn tool_update_parses_verbose_flag() {
        let cli = Cli::try_parse_from(["za", "tool", "update", "--all", "--verbose"])
            .expect("must parse");
        match cli.cmd {
            Commands::Tool { user, cmd } => {
                assert!(!user);
                assert!(matches!(
                    cmd,
                    ToolCommands::Update {
                        all: true,
                        tools,
                        version: None,
                        dry_run: false,
                        verbose: true,
                    } if tools.is_empty()
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn tool_sync_parses_flags() {
        let cli = Cli::try_parse_from([
            "za",
            "tool",
            "sync",
            "--file",
            "za.tools.toml",
            "--dry-run",
            "--verbose",
        ])
        .expect("must parse");
        match cli.cmd {
            Commands::Tool { user, cmd } => {
                assert!(!user);
                assert!(matches!(
                    cmd,
                    ToolCommands::Sync {
                        file,
                        dry_run: true,
                        verbose: true,
                    } if file == std::path::Path::new("za.tools.toml")
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn deps_parses_verbose_flag() {
        let cli = Cli::try_parse_from(["za", "deps", "--verbose"]).expect("must parse");
        match cli.cmd {
            Commands::Deps { audit, cmd } => {
                assert!(cmd.is_none());
                assert!(audit.manifest_path.is_none());
                assert!(audit.github_token.is_none());
                assert!(audit.jobs.is_none());
                assert!(!audit.include_dev);
                assert!(!audit.include_build);
                assert!(!audit.include_optional);
                assert!(audit.json.is_none());
                assert!(!audit.fail_on_high);
                assert!(audit.verbose);
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn deps_latest_parses_manifest_and_toml_flags() {
        let cli = Cli::try_parse_from([
            "za",
            "deps",
            "latest",
            "serde",
            "--manifest",
            "Cargo.toml",
            "--toml",
        ])
        .expect("must parse");
        match cli.cmd {
            Commands::Deps { cmd, .. } => match cmd {
                Some(DepsCommands::Latest {
                    crates,
                    manifest_path,
                    jobs,
                    include_dev,
                    include_build,
                    include_optional,
                    json,
                    toml,
                    suggest,
                }) => {
                    assert_eq!(crates, vec!["serde"]);
                    assert_eq!(manifest_path, Some(PathBuf::from("Cargo.toml")));
                    assert!(jobs.is_none());
                    assert!(!include_dev);
                    assert!(!include_build);
                    assert!(!include_optional);
                    assert!(!json);
                    assert!(toml);
                    assert!(!suggest);
                }
                _ => panic!("unexpected deps command"),
            },
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn deps_latest_parses_suggest_flag() {
        let cli =
            Cli::try_parse_from(["za", "deps", "latest", "reqx", "--suggest"]).expect("must parse");
        match cli.cmd {
            Commands::Deps { cmd, .. } => match cmd {
                Some(DepsCommands::Latest {
                    crates,
                    json,
                    toml,
                    suggest,
                    ..
                }) => {
                    assert_eq!(crates, vec!["reqx"]);
                    assert!(!json);
                    assert!(!toml);
                    assert!(suggest);
                }
                _ => panic!("unexpected deps command"),
            },
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn completion_status_parses_shell_and_path() {
        let cli = Cli::try_parse_from([
            "za",
            "completion",
            "status",
            "bash",
            "--path",
            "/tmp/za.bash",
        ])
        .expect("must parse");
        match cli.cmd {
            Commands::Completion { cmd } => match cmd {
                CompletionCommands::Status { shell, path } => {
                    assert_eq!(shell, CompletionShell::Bash);
                    assert_eq!(path, Some(PathBuf::from("/tmp/za.bash")));
                }
                _ => panic!("unexpected completion command"),
            },
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn tool_doctor_parses_json_flag() {
        let cli =
            Cli::try_parse_from(["za", "tool", "doctor", "codex", "--json"]).expect("must parse");
        match cli.cmd {
            Commands::Tool { cmd, .. } => {
                assert!(matches!(
                    cmd,
                    ToolCommands::Doctor { tools, json: true } if tools == vec!["codex"]
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn port_wait_parses_timeout_and_interval() {
        let cli = Cli::try_parse_from([
            "za",
            "port",
            "wait",
            "3000",
            "--timeout-secs",
            "12",
            "--interval-ms",
            "250",
            "--tcp",
        ])
        .expect("must parse");
        match cli.cmd {
            Commands::Port { cmd } => {
                assert!(matches!(
                    cmd,
                    PortCommands::Wait {
                        port: 3000,
                        timeout_secs: 12,
                        interval_ms: 250,
                        all: false,
                        tcp: true,
                        udp: false,
                    }
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn port_open_parses_protocol_filters() {
        let cli = Cli::try_parse_from(["za", "port", "open", "8080", "--udp"]).expect("must parse");
        match cli.cmd {
            Commands::Port { cmd } => {
                assert!(matches!(
                    cmd,
                    PortCommands::Open {
                        port: 8080,
                        all: false,
                        tcp: false,
                        udp: true,
                    }
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn port_stop_parses_signal_and_dry_run() {
        let cli = Cli::try_parse_from([
            "za",
            "port",
            "stop",
            "3000",
            "--signal",
            "kill",
            "--dry-run",
        ])
        .expect("must parse");
        match cli.cmd {
            Commands::Port { cmd } => {
                assert!(matches!(
                    cmd,
                    PortCommands::Stop {
                        port: 3000,
                        signal: PortSignal::Kill,
                        dry_run: true,
                        all: false,
                        tcp: false,
                        udp: false,
                    }
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn port_follow_parses_timeout_and_interval() {
        let cli = Cli::try_parse_from([
            "za",
            "port",
            "follow",
            "3000",
            "--timeout-secs",
            "20",
            "--interval-ms",
            "750",
            "--tcp",
        ])
        .expect("must parse");
        match cli.cmd {
            Commands::Port { cmd } => {
                assert!(matches!(
                    cmd,
                    PortCommands::Follow {
                        port: 3000,
                        timeout_secs: Some(20),
                        interval_ms: 750,
                        all: false,
                        tcp: true,
                        udp: false,
                    }
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn gh_ci_inspect_parses_all_and_json_flags() {
        let cli = Cli::try_parse_from(["za", "gh", "ci", "inspect", "--all", "--json"])
            .expect("must parse");
        match cli.cmd {
            Commands::Gh {
                cmd:
                    GhCommands::Ci {
                        cmd:
                            Some(CiCommands::Inspect {
                                all: true,
                                json: true,
                                github_token: None,
                            }),
                        ..
                    },
            } => {}
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn gh_ci_list_parses_all_flag() {
        let cli = Cli::try_parse_from(["za", "gh", "ci", "list", "--group", "work", "--all"])
            .expect("must parse");
        match cli.cmd {
            Commands::Gh {
                cmd:
                    GhCommands::Ci {
                        cmd:
                            Some(CiCommands::List {
                                group,
                                repo,
                                file,
                                json: false,
                                all: true,
                                github_token: None,
                            }),
                        ..
                    },
            } => {
                assert_eq!(group.as_deref(), Some("work"));
                assert!(repo.is_empty());
                assert!(file.is_none());
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn diff_parses_review_filters() {
        let cli = Cli::try_parse_from([
            "za",
            "diff",
            "--tui",
            "--staged",
            "--kind",
            "code",
            "--path",
            "src/**",
            "--exclude-risk",
            "generated",
        ])
        .expect("must parse");
        match cli.cmd {
            Commands::Diff {
                tui,
                json,
                files,
                name_only,
                staged,
                unstaged,
                untracked,
                kind,
                path,
                exclude_risk,
            } => {
                assert!(tui);
                assert!(!json);
                assert!(!files);
                assert!(!name_only);
                assert!(staged);
                assert!(!unstaged);
                assert!(!untracked);
                assert_eq!(kind, vec![DiffKindFilter::Code]);
                assert_eq!(path, vec!["src/**"]);
                assert_eq!(exclude_risk, vec![DiffRiskFilter::Generated]);
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn diff_rejects_tui_with_json() {
        assert!(Cli::try_parse_from(["za", "diff", "--tui", "--json"]).is_err());
    }

    #[test]
    fn diff_parses_json_review_filters() {
        let cli = Cli::try_parse_from([
            "za",
            "diff",
            "--json",
            "--files",
            "--name-only",
            "--staged",
            "--kind",
            "docs",
            "--path",
            "src/**",
            "--exclude-risk",
            "generated",
        ])
        .expect("must parse");
        match cli.cmd {
            Commands::Diff {
                tui,
                json,
                files,
                name_only,
                staged,
                unstaged,
                untracked,
                kind,
                path,
                exclude_risk,
            } => {
                assert!(!tui);
                assert!(json);
                assert!(files);
                assert!(name_only);
                assert!(staged);
                assert!(!unstaged);
                assert!(!untracked);
                assert_eq!(kind, vec![DiffKindFilter::Docs]);
                assert_eq!(path, vec!["src/**"]);
                assert_eq!(exclude_risk, vec![DiffRiskFilter::Generated]);
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn diff_parses_multiple_kind_filters() {
        let cli = Cli::try_parse_from([
            "za",
            "diff",
            "--kind",
            "code",
            "--kind",
            "docs",
            "--unstaged",
        ])
        .expect("must parse");
        match cli.cmd {
            Commands::Diff {
                staged,
                unstaged,
                untracked,
                kind,
                ..
            } => {
                assert!(!staged);
                assert!(unstaged);
                assert!(!untracked);
                assert_eq!(kind, vec![DiffKindFilter::Code, DiffKindFilter::Docs]);
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn port_ls_parses_filters() {
        let cli = Cli::try_parse_from([
            "za", "port", "ls", "--json", "--all", "--port", "8080", "--pid", "123", "--tcp",
        ])
        .expect("must parse");
        match cli.cmd {
            Commands::Port { cmd } => {
                assert!(matches!(
                    cmd,
                    PortCommands::Ls {
                        json: true,
                        all: true,
                        port,
                        pid,
                        tcp: true,
                        udp: false,
                    } if port == vec![8080] && pid == vec![123]
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn port_ls_rejects_negative_pid_filter() {
        assert!(Cli::try_parse_from(["za", "port", "ls", "--pid=-1"]).is_err());
    }

    #[test]
    fn port_ls_rejects_zero_pid_filter() {
        assert!(Cli::try_parse_from(["za", "port", "ls", "--pid=0"]).is_err());
    }

    #[test]
    fn codex_passthrough_args_are_captured_after_double_dash() {
        let cli = Cli::try_parse_from(["za", "codex", "--", "resume"]).expect("must parse");
        match cli.cmd {
            Commands::Codex { cmd, args } => {
                assert!(cmd.is_none());
                assert_eq!(args, vec!["resume"]);
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn codex_subcommand_still_parses_normally() {
        let cli = Cli::try_parse_from(["za", "codex", "attach"]).expect("must parse");
        match cli.cmd {
            Commands::Codex { cmd, args } => {
                assert!(args.is_empty());
                assert!(matches!(cmd, Some(CodexCommands::Attach)));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn codex_top_parses_all_flag() {
        let cli =
            Cli::try_parse_from(["za", "codex", "top", "--all", "--history"]).expect("must parse");
        match cli.cmd {
            Commands::Codex { cmd, args } => {
                assert!(args.is_empty());
                assert!(matches!(
                    cmd,
                    Some(CodexCommands::Top {
                        all: true,
                        history: true
                    })
                ));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn gh_auth_parses_to_github_auth_shortcut() {
        let cli = Cli::try_parse_from(["za", "gh", "auth", "status"]).expect("must parse");
        match cli.cmd {
            Commands::Gh { cmd } => match cmd {
                GhCommands::Auth { cmd } => {
                    assert!(matches!(cmd, GitAuthCommands::Status { json: false }));
                }
                _ => panic!("unexpected gh command"),
            },
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn gh_auth_repair_parses_timeout_and_json() {
        let cli = Cli::try_parse_from([
            "za",
            "gh",
            "auth",
            "repair",
            "--remote",
            "upstream",
            "--timeout-secs",
            "30",
            "--json",
        ])
        .expect("must parse");
        match cli.cmd {
            Commands::Gh { cmd } => match cmd {
                GhCommands::Auth { cmd } => {
                    assert!(matches!(
                        cmd,
                        GitAuthCommands::Repair {
                            remote,
                            timeout_secs: 30,
                            json: true,
                        } if remote == "upstream"
                    ));
                }
                _ => panic!("unexpected gh command"),
            },
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn github_alias_parses_to_gh_command() {
        let cli = Cli::try_parse_from(["za", "github", "ci", "watch"]).expect("must parse");
        match cli.cmd {
            Commands::Gh { cmd } => match cmd {
                GhCommands::Ci {
                    json,
                    github_token,
                    cmd,
                } => {
                    assert!(!json);
                    assert!(github_token.is_none());
                    assert!(matches!(
                        cmd,
                        Some(CiCommands::Watch {
                            timeout_secs: None,
                            json: false,
                            github_token: None,
                        })
                    ));
                }
                _ => panic!("unexpected gh command"),
            },
            _ => panic!("unexpected command"),
        }
    }
}
