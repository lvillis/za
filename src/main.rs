use anyhow::Result;
use clap::Parser;

mod cli;
mod command;

fn main() -> Result<()> {
    let args = cli::Cli::parse();
    match args.cmd {
        cli::Commands::Gen {
            max_lines,
            output,
            include_binary,
        } => command::r#gen::run(max_lines, output, include_binary),
        cli::Commands::Stats {
            top,
            days,
            json,
            output,
        } => command::stats::run(top, days, json, output),
    }
}
