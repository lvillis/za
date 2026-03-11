use crate::cli::{CompletionCommands, CompletionShell};
use anyhow::{Context, Result, anyhow, bail};
use clap::CommandFactory;
use clap_complete::{Shell, generate};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

pub fn run(cmd: CompletionCommands) -> Result<i32> {
    match cmd {
        CompletionCommands::Bash => print_completion(CompletionShell::Bash),
        CompletionCommands::Zsh => print_completion(CompletionShell::Zsh),
        CompletionCommands::Fish => print_completion(CompletionShell::Fish),
        CompletionCommands::Elvish => print_completion(CompletionShell::Elvish),
        CompletionCommands::Powershell => print_completion(CompletionShell::Powershell),
        CompletionCommands::Install { shell, path } => install_completion(shell, path),
    }
}

fn print_completion(shell: CompletionShell) -> Result<i32> {
    print!(
        "{}",
        String::from_utf8(render_completion(shell)?).context("completion output must be utf-8")?
    );
    Ok(0)
}

fn install_completion(shell: CompletionShell, path_override: Option<PathBuf>) -> Result<i32> {
    let target_path = match path_override {
        Some(path) => path,
        None => default_install_path(shell)?,
    };
    let parent = target_path.parent().ok_or_else(|| {
        anyhow!(
            "cannot install {} completion to `{}`: missing parent directory",
            shell.label(),
            target_path.display()
        )
    })?;
    fs::create_dir_all(parent).with_context(|| format!("create `{}`", parent.display()))?;
    fs::write(&target_path, render_completion(shell)?)
        .with_context(|| format!("write completion to `{}`", target_path.display()))?;

    println!(
        "installed {} completion: {}",
        shell.label(),
        target_path.display()
    );
    if let Some(hint) = install_hint(shell, &target_path) {
        println!("{hint}");
    }

    Ok(0)
}

fn render_completion(shell: CompletionShell) -> Result<Vec<u8>> {
    let mut cmd = crate::cli::Cli::command();
    let mut output = Vec::new();
    let bin_name = cmd.get_name().to_string();
    let generator: Shell = shell.into();
    generate(generator, &mut cmd, bin_name, &mut output);
    Ok(output)
}

fn default_install_path(shell: CompletionShell) -> Result<PathBuf> {
    match shell {
        CompletionShell::Bash => Ok(resolve_data_home()?
            .join("bash-completion")
            .join("completions")
            .join("za")),
        CompletionShell::Zsh => Ok(resolve_zdotdir_or_home()?.join(".zfunc").join("_za")),
        CompletionShell::Fish => Ok(resolve_config_home()?
            .join("fish")
            .join("completions")
            .join("za.fish")),
        CompletionShell::Elvish => bail!(
            "automatic install for `elvish` is not configured; use `za completion install elvish --path <PATH>`"
        ),
        CompletionShell::Powershell => bail!(
            "automatic install for `powershell` is not configured; use `za completion install powershell --path <PATH>`"
        ),
    }
}

fn resolve_home() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot resolve home directory: set `HOME`"))
}

fn resolve_zdotdir_or_home() -> Result<PathBuf> {
    Ok(env::var_os("ZDOTDIR")
        .map(PathBuf::from)
        .unwrap_or(resolve_home()?))
}

fn resolve_config_home() -> Result<PathBuf> {
    Ok(env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(resolve_home()?.join(".config")))
}

fn resolve_data_home() -> Result<PathBuf> {
    Ok(env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or(resolve_home()?.join(".local").join("share")))
}

fn install_hint(shell: CompletionShell, target_path: &Path) -> Option<String> {
    match shell {
        CompletionShell::Bash => Some(
            "open a new shell or source your bash-completion setup to load `za` completions"
                .to_string(),
        ),
        CompletionShell::Fish => Some("open a new fish shell to load `za` completions".to_string()),
        CompletionShell::Zsh => target_path.parent().map(|dir| {
            format!(
                "ensure `{}` is in `fpath`, then run `autoload -Uz compinit && compinit` if needed",
                dir.display()
            )
        }),
        CompletionShell::Elvish | CompletionShell::Powershell => None,
    }
}

impl CompletionShell {
    fn label(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::Elvish => "elvish",
            Self::Powershell => "powershell",
        }
    }
}

impl From<CompletionShell> for Shell {
    fn from(value: CompletionShell) -> Self {
        match value {
            CompletionShell::Bash => Shell::Bash,
            CompletionShell::Zsh => Shell::Zsh,
            CompletionShell::Fish => Shell::Fish,
            CompletionShell::Elvish => Shell::Elvish,
            CompletionShell::Powershell => Shell::PowerShell,
        }
    }
}
