use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// Top-level CLI parser
#[derive(Parser)]
#[command(name = "za", version)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Commands,
}

/// Sub-command definitions
#[derive(Subcommand)]
pub enum Commands {
    /// Generate shell completion scripts
    Completion {
        #[command(subcommand)]
        cmd: CompletionCommands,
    },
    /// Summarize current Git workspace changes for review
    Diff {
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
        /// Hide files carrying the selected review risk tag. Repeatable.
        #[arg(long, value_enum, value_name = "RISK")]
        exclude_risk: Vec<DiffRiskFilter>,
    },
    /// Generate `CONTEXT.md`
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
    /// Audit Rust dependency governance and maintenance signals (crates.io + GitHub)
    Deps {
        /// Optional path to Cargo.toml (defaults to current workspace root).
        #[arg(long, value_name = "PATH")]
        manifest_path: Option<PathBuf>,
        /// Optional GitHub token override for this run.
        #[arg(long, value_name = "TOKEN")]
        github_token: Option<String>,
        /// Number of concurrent workers for API queries (default: auto, based on CPU count).
        #[arg(long, value_name = "JOBS")]
        jobs: Option<usize>,
        /// Include dev-dependencies in audit.
        #[arg(long)]
        include_dev: bool,
        /// Include build-dependencies in audit.
        #[arg(long)]
        include_build: bool,
        /// Also include optional dependencies that are not active in the current resolved feature set.
        #[arg(long)]
        include_optional: bool,
        /// Write full audit report to JSON.
        #[arg(long, value_name = "PATH")]
        json: Option<PathBuf>,
        /// Exit with non-zero status when any high-risk dependency is found.
        #[arg(long)]
        fail_on_high: bool,
    },
    /// Manage versioned CLI tools
    Tool {
        /// Use user-level paths (`~/.local/...`) instead of system-level paths.
        #[arg(long)]
        user: bool,
        #[command(subcommand)]
        cmd: ToolCommands,
    },
    /// Run a tool with proxy environment normalization
    Run {
        /// Tool name, e.g. `codex`
        tool: String,
        /// Arguments passed through to the tool
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Manage long-lived Codex work sessions backed by tmux
    Codex {
        #[command(subcommand)]
        cmd: Option<CodexCommands>,
        /// Arguments passed through to `codex` when prefixed by `--`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Update za itself from GitHub releases
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
    /// Manage persisted za configuration values
    Config {
        #[command(subcommand)]
        cmd: Option<ConfigCommands>,
    },
    /// Manage JetBrains remote IDE server processes
    Ide {
        #[command(subcommand)]
        cmd: IdeCommands,
    },
    /// Unified GitHub shortcuts (`za gh auth`, `za gh ci`)
    #[command(visible_alias = "github")]
    Gh {
        #[command(subcommand)]
        cmd: GhCommands,
    },
}

/// `za tool` sub-commands
#[derive(Subcommand)]
pub enum ToolCommands {
    /// Install a tool, e.g. `codex` or `codex:0.104.0`
    Install {
        /// Tool spec in `name[:version]` format.
        spec: String,
    },
    /// Update a tool to latest or a target version
    Update {
        /// Tool spec in `name[:version]` format.
        spec: String,
    },
    /// List tools and installed versions
    List {
        /// Show built-in supported tools and source policies.
        #[arg(long)]
        supported: bool,
        /// Query upstream releases and show whether updates are available.
        #[arg(long)]
        updates: bool,
        /// Print JSON output for scripting.
        #[arg(long)]
        json: bool,
        /// Return non-zero when updates are available (requires `--updates`).
        #[arg(long)]
        fail_on_updates: bool,
        /// Return non-zero when update checks fail (requires `--updates`).
        #[arg(long)]
        fail_on_check_errors: bool,
    },
    /// Sync tools from a manifest file (`za.tools.toml`)
    Sync {
        /// Manifest path.
        #[arg(long, value_name = "PATH", default_value = "za.tools.toml")]
        file: PathBuf,
    },
    /// Select the active tool version, e.g. `codex:0.104.0`
    Use {
        /// Tool reference in `name:version` format.
        image: String,
    },
    /// Uninstall a tool version or all versions, e.g. `codex:0.104.0` or `codex`
    Uninstall {
        /// Tool spec in `name[:version]` format.
        spec: String,
    },
}

/// `za completion` sub-commands
#[derive(Subcommand)]
pub enum CompletionCommands {
    /// Print Bash completion script to stdout
    Bash,
    /// Print Zsh completion script to stdout
    Zsh,
    /// Print Fish completion script to stdout
    Fish,
    /// Print Elvish completion script to stdout
    Elvish,
    /// Print PowerShell completion script to stdout
    Powershell,
    /// Install a completion script into a common user-level path
    Install {
        #[arg(value_enum)]
        shell: CompletionShell,
        /// Override the install path instead of using the default shell-specific location.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
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

#[cfg(test)]
mod tests {
    use super::{
        CiCommands, Cli, CodexCommands, Commands, CompletionCommands, CompletionShell,
        DiffRiskFilter, GhCommands, GitAuthCommands,
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
    fn diff_parses_review_filters() {
        let cli = Cli::try_parse_from([
            "za",
            "diff",
            "--json",
            "--files",
            "--name-only",
            "--staged",
            "--path",
            "src/**",
            "--exclude-risk",
            "generated",
        ])
        .expect("must parse");
        match cli.cmd {
            Commands::Diff {
                json,
                files,
                name_only,
                staged,
                unstaged,
                untracked,
                path,
                exclude_risk,
            } => {
                assert!(json);
                assert!(files);
                assert!(name_only);
                assert!(staged);
                assert!(!unstaged);
                assert!(!untracked);
                assert_eq!(path, vec!["src/**"]);
                assert_eq!(exclude_risk, vec![DiffRiskFilter::Generated]);
            }
            _ => panic!("unexpected command"),
        }
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
