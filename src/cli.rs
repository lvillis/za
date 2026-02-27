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
    /// Audit Rust dependency maintenance signals (crates.io + GitHub)
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
        /// Include optional dependencies in audit.
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
    /// Update za itself from GitHub releases
    Update {
        /// Install to user-level paths (`~/.local/...`) instead of system-level paths.
        #[arg(long)]
        user: bool,
        /// Target version (defaults to latest release).
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
    },
    /// Manage persisted za configuration values
    Config {
        #[command(subcommand)]
        cmd: Option<ConfigCommands>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ConfigKey {
    #[value(name = "github-token")]
    GithubToken,
    #[value(name = "run-http-proxy")]
    RunHttpProxy,
    #[value(name = "run-https-proxy")]
    RunHttpsProxy,
    #[value(name = "run-all-proxy")]
    RunAllProxy,
    #[value(name = "run-no-proxy")]
    RunNoProxy,
}
