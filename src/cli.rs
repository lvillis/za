use clap::{Parser, Subcommand};
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

        /// (Optional) Git repo URL to clone and scan, e.g. https://github.com/owner/repo.git
        #[arg(long)]
        repo: Option<String>,
        /// (Optional) Rev to checkout: commit/tag/branch; defaults to remote HEAD
        #[arg(long)]
        rev: Option<String>,
        /// (Optional) Subdirectory inside the repo to scan, e.g. crates/abc
        #[arg(long, value_name = "PATH")]
        repo_subdir: Option<PathBuf>,
        /// Keep the cloned directory instead of cleaning it up
        #[arg(long)]
        keep_clone: bool,
    },
    /// Generate `STATS.md` / `stats.json`
    Stats {
        #[arg(long, default_value_t = crate::command::STAT_TOP_N)]
        top: usize,
        #[arg(long, default_value_t = crate::command::STAT_RECENT_DAYS)]
        days: u32,
        #[arg(long)]
        json: Option<PathBuf>,
        #[arg(long, default_value = "STATS.md")]
        output: PathBuf,
    },
    /// CI quality gate: enforce repository thresholds and rules
    Gate {
        /// Fail if total binary size exceeds this (MiB)
        #[arg(long)]
        max_binary_mib: Option<f64>,
        /// Fail if any single file exceeds this size (MiB)
        #[arg(long)]
        max_file_size_mib: Option<f64>,
        /// Fail if naive complexity score exceeds this value
        #[arg(long)]
        max_complexity: Option<usize>,
        /// Deny files matching these globs (comma-separated or repeated)
        #[arg(long, value_delimiter = ',')]
        deny_glob: Vec<String>,
        /// Treat any detected secret as an error (otherwise only warn)
        #[arg(long)]
        strict_secrets: bool,
        /// Write detected secrets to a JSON report
        #[arg(long)]
        secrets_json: Option<PathBuf>,
        /// Allow secrets under these globs (comma-separated or repeated)
        #[arg(long, value_delimiter = ',')]
        allow_secrets_in: Vec<String>,
    },
}
