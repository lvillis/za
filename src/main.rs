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
            repo,
            rev,
            repo_subdir,
            keep_clone,
        } => command::r#gen::run(
            max_lines,
            output,
            include_binary,
            repo,
            rev,
            repo_subdir,
            keep_clone,
        ),
        cli::Commands::Stats {
            top,
            days,
            json,
            output,
        } => command::stats::run(top, days, json, output),
        cli::Commands::Gate {
            max_binary_mib,
            max_file_size_mib,
            max_complexity,
            deny_glob,
            strict_secrets,
            secrets_json,
            allow_secrets_in,
        } => command::gate::run(
            max_binary_mib,
            max_file_size_mib,
            max_complexity,
            deny_glob,
            strict_secrets,
            secrets_json,
            allow_secrets_in,
        ),
    }
}
