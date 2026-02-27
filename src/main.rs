use anyhow::{Result, anyhow};
use clap::Parser;

mod cli;
mod command;

fn main() -> Result<()> {
    init_tls_crypto_provider()?;

    let args = cli::Cli::parse();
    match args.cmd {
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
        } => command::deps::run(command::deps::DepsRunOptions {
            manifest_path,
            github_token_override: github_token,
            jobs,
            include_dev,
            include_build,
            include_optional,
            json_out: json,
            fail_on_high,
        }),
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
