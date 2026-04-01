use anyhow::{Result, anyhow};
use clap::Parser;

mod cli;
mod command;

fn main() -> Result<()> {
    init_tls_crypto_provider()?;

    let args = cli::Cli::parse();
    match args.cmd {
        cli::Commands::Completion { cmd } => {
            let exit_code = command::completion::run(cmd)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        cli::Commands::Diff {
            tui,
            json,
            files,
            name_only,
            staged,
            unstaged,
            untracked,
            path,
            exclude_risk,
        } => {
            let exit_code = command::diff::run(command::diff::DiffRunOptions {
                tui,
                json,
                files,
                name_only,
                path_patterns: path,
                scopes: [
                    (staged, command::diff::DiffScope::Staged),
                    (unstaged, command::diff::DiffScope::Unstaged),
                    (untracked, command::diff::DiffScope::Untracked),
                ]
                .into_iter()
                .filter_map(|(enabled, scope)| enabled.then_some(scope))
                .collect(),
                exclude_risks: exclude_risk
                    .into_iter()
                    .map(command::diff::DiffRiskKind::from)
                    .collect(),
            })?;
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
        cli::Commands::Deps {
            manifest_path,
            github_token,
            jobs,
            include_dev,
            include_build,
            include_optional,
            json,
            fail_on_high,
            verbose,
        } => command::deps::run(command::deps::DepsRunOptions {
            manifest_path,
            github_token_override: github_token,
            jobs,
            include_dev,
            include_build,
            include_optional,
            json_out: json,
            fail_on_high,
            verbose,
        }),
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
