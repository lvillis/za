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
}
