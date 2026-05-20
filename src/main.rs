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
        cli::Commands::Ai { cmd } => exit_with(command::ai::run(cmd)?),
        cli::Commands::Completion { cmd } => exit_with(command::completion::run(cmd)?),
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
            exit_with(exit_code)
        }
        cli::Commands::Gen {
            max_lines,
            output,
            include_binary,
            repo,
            r#ref,
        } => command::r#gen::run(max_lines, output, include_binary, repo, r#ref),
        cli::Commands::Deps { audit, cmd } => match cmd {
            None => run_deps_audit(audit),
            Some(cli::DepsCommands::Latest {
                crates,
                manifest_path,
                path,
                jobs,
                include_dev,
                include_build,
                include_optional,
                json,
                toml,
                suggest,
            }) => {
                reject_parent_deps_audit_args(&audit)?;
                command::deps::run_latest(command::deps::DepsLatestOptions {
                    crates,
                    manifest_path,
                    project_path: path,
                    jobs,
                    include_dev,
                    include_build,
                    include_optional,
                    json,
                    toml,
                    suggest,
                })
            }
        },
        cli::Commands::Pin { cmd } => exit_with(command::pin::run(cmd)?),
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
            cli::GhCommands::Ci {
                json,
                github_token,
                cmd,
            } => exit_with(command::ci::run(cmd, json, github_token)?),
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
        github_token_override: audit.github_token,
        jobs: audit.jobs,
        include_dev: audit.include_dev,
        include_build: audit.include_build,
        include_optional: audit.include_optional,
        json_out: audit.json,
        fail_on_high: audit.fail_on_high,
        verbose: audit.verbose,
    })
}

fn reject_parent_deps_audit_args(audit: &cli::DepsAuditArgs) -> Result<()> {
    if audit != &cli::DepsAuditArgs::default() {
        return Err(anyhow!(
            "`za deps <subcommand>` does not accept audit options before the subcommand; pass subcommand options after `latest`"
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
