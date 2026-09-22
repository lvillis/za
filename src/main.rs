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
        cli::Commands::Completion { cmd } => exit_with(command::completion::run(cmd)?),
        cli::Commands::Deps { args, cmd } => match cmd {
            None => command::deps::run_latest(command::deps::DepsLatestOptions {
                manifest_path: args.manifest_path,
                project_path: args.path,
                jobs: args.jobs,
                include_dev: args.include_dev,
                include_build: args.include_build,
                include_optional: args.include_optional,
                refresh: args.refresh,
                json: args.json,
                toml: args.emit.is_some(),
                suggest: true,
            }),
            Some(cli::DepsCommands::Resolve { cmd }) => {
                reject_parent_deps_args(&args)?;
                command::deps::resolve::run(cmd)
            }
            Some(cli::DepsCommands::Audit { args: audit }) => {
                reject_parent_deps_args(&args)?;
                run_deps_audit(audit)
            }
        },
        cli::Commands::Port { cmd } => exit_with(command::port::run(cmd)?),
        cli::Commands::Tool { user, global, cmd } => exit_with(command::tool::run(
            cmd,
            command::tool::ToolScopeRequest::from_flags(user, global)?,
        )?),
        cli::Commands::Run { tool, args } => exit_with(command::run::run(&tool, &args)?),
        cli::Commands::Codex { cmd, args } => exit_with(command::codex::run(cmd, &args)?),
        cli::Commands::Update {
            user,
            global,
            check,
            version,
        } => exit_with(command::tool::update_self(
            command::tool::ToolScopeRequest::from_flags(user, global)?,
            check,
            version,
        )?),
        cli::Commands::Config { cmd } => command::za_config::run(cmd),
        cli::Commands::Ide { cmd } => exit_with(command::ide::run(cmd)?),
        cli::Commands::Gh { cmd } => match cmd {
            cli::GhCommands::Auth { cmd } => exit_with(command::git::run_auth(cmd)?),
            cli::GhCommands::Ci { json, cmd } => exit_with(command::ci::run(cmd, json)?),
            cli::GhCommands::Credential { operation } => {
                exit_with(command::git::run_credential(operation)?)
            }
        },
    }
}

fn run_deps_audit(audit: cli::DepsAuditArgs) -> Result<()> {
    command::deps::run(command::deps::DepsRunOptions {
        manifest_path: audit.manifest_path,
        project_path: audit.path,
        jobs: audit.jobs,
        include_dev: audit.include_dev,
        include_build: audit.include_build,
        include_optional: audit.include_optional,
        refresh: audit.refresh,
        json_out: audit.json,
        fail_on_high: audit.fail_on_high,
        verbose: audit.verbose,
    })
}

fn reject_parent_deps_args(args: &cli::DepsArgs) -> Result<()> {
    if args != &cli::DepsArgs::default() {
        return Err(anyhow!(
            "`za deps <subcommand>` does not accept update options before the subcommand; pass options after the relevant subcommand"
        ));
    }
    Ok(())
}

fn exit_with(code: i32) -> Result<()> {
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

fn init_tls_crypto_provider() -> Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .map_err(|_| anyhow!("failed to install rustls ring crypto provider"))?;
    }
    Ok(())
}
