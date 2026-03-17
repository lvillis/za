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
    /// Print or install shell completions
    Completion {
        #[command(subcommand)]
        cmd: CompletionCommands,
    },
    /// Review current Git workspace changes
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
        /// Tool names. Omit to update all managed tools in this scope.
        tools: Vec<String>,
        /// Pin the update target to a specific version. Requires exactly one tool.
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
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
        DiffRiskFilter, GhCommands, GitAuthCommands, ToolCommands,
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
                    ToolCommands::Update { tools, version: None } if tools == vec!["codex", "rg"]
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
                    } if tools == vec!["codex"]
                ));
            }
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
