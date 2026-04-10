use anyhow::{Result, anyhow};
use clap::Parser;

mod cli;
mod command;

fn main() -> Result<()> {
    init_tls_crypto_provider()?;

    let args = cli::Cli::parse();
    command::style::set_color_mode(match args.color {
        cli::ColorWhen::Auto => command::style::ColorMode::Auto,
        cli::ColorWhen::Always => command::style::ColorMode::Always,
        cli::ColorWhen::Never => command::style::ColorMode::Never,
    });
    match args.cmd {
        cli::Commands::Completion { cmd } => {
            let exit_code = command::completion::run(cmd)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        cli::Commands::Diff { args, cmd } => {
            let exit_code = match cmd {
                Some(cli::DiffCommands::Stats {
                    since,
                    include_worktree,
                    json,
                    kind,
                }) => {
                    if args != cli::DiffArgs::default() {
                        return Err(anyhow!(
                            "`za diff stats` does not accept workspace diff flags before the subcommand; pass stats flags after `stats`"
                        ));
                    }
                    command::diff::run_stats(command::diff::DiffStatsRunOptions {
                        since,
                        include_worktree,
                        json,
                        kinds: kind
                            .into_iter()
                            .map(command::diff::DiffFileKind::from)
                            .collect(),
                    })?
                }
                None => command::diff::run(command::diff::DiffRunOptions {
                    tui: args.tui,
                    json: args.json,
                    files: args.files,
                    name_only: args.name_only,
                    path_patterns: args.path,
                    scopes: [
                        (args.staged, command::diff::DiffScope::Staged),
                        (args.unstaged, command::diff::DiffScope::Unstaged),
                        (args.untracked, command::diff::DiffScope::Untracked),
                    ]
                    .into_iter()
                    .filter_map(|(enabled, scope)| enabled.then_some(scope))
                    .collect(),
                    kinds: args
                        .kind
                        .into_iter()
                        .map(command::diff::DiffFileKind::from)
                        .collect(),
                    exclude_risks: args
                        .exclude_risk
                        .into_iter()
                        .map(command::diff::DiffRiskKind::from)
                        .collect(),
                })?,
            };
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        cli::Commands::Gen {
            max_lines,
            output,
            include_binary,
            repo,
            r#ref,
        } => command::r#gen::run(max_lines, output, include_binary, repo, r#ref),
        cli::Commands::Deps { audit, cmd } => match cmd {
            None => command::deps::run(command::deps::DepsRunOptions {
                manifest_path: audit.manifest_path,
                github_token_override: audit.github_token,
                jobs: audit.jobs,
                include_dev: audit.include_dev,
                include_build: audit.include_build,
                include_optional: audit.include_optional,
                json_out: audit.json,
                fail_on_high: audit.fail_on_high,
                verbose: audit.verbose,
            }),
            Some(cli::DepsCommands::Latest {
                crates,
                manifest_path,
                jobs,
                include_dev,
                include_build,
                include_optional,
                json,
                toml,
                suggest,
            }) => command::deps::run_latest(command::deps::DepsLatestOptions {
                crates,
                manifest_path,
                jobs,
                include_dev,
                include_build,
                include_optional,
                json,
                toml,
                suggest,
            }),
        },
        cli::Commands::Port { cmd } => {
            let exit_code = command::port::run(cmd)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        cli::Commands::Tool { user, cmd } => {
            let exit_code = command::tool::run(cmd, user)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        cli::Commands::Run { tool, args } => {
            let exit_code = command::run::run(&tool, &args)?;
            std::process::exit(exit_code);
        }
        cli::Commands::Codex { cmd, args } => {
            let exit_code = command::codex::run(cmd, &args)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        cli::Commands::Update {
            user,
            check,
            version,
        } => {
            let exit_code = command::tool::update_self(user, check, version)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        cli::Commands::Config { cmd } => command::za_config::run(cmd),
        cli::Commands::Ide { cmd } => {
            let exit_code = command::ide::run(cmd)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        cli::Commands::Gh { cmd } => match cmd {
            cli::GhCommands::Auth { cmd } => {
                let exit_code = command::git::run_auth(cmd)?;
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
                Ok(())
            }
            cli::GhCommands::Ci {
                json,
                github_token,
                cmd,
            } => {
                let exit_code = command::ci::run(cmd, json, github_token)?;
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
                Ok(())
            }
            cli::GhCommands::Credential { operation } => {
                let exit_code = command::git::run_credential(operation)?;
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
                Ok(())
            }
        },
    }
}

fn init_tls_crypto_provider() -> Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .map_err(|_| anyhow!("failed to install rustls ring crypto provider"))?;
    }
    Ok(())
}
