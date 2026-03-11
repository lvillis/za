use crate::cli::{CompletionCommands, CompletionShell};
use anyhow::{Context, Result, anyhow, bail};
use clap::CommandFactory;
use clap_complete::{Shell, generate};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

const BASH_COMPLETION_START_MARKER: &str = "# >>> za completion (bash) >>>";
const BASH_COMPLETION_END_MARKER: &str = "# <<< za completion (bash) <<<";
const ZSH_COMPLETION_START_MARKER: &str = "# >>> za completion (zsh) >>>";
const ZSH_COMPLETION_END_MARKER: &str = "# <<< za completion (zsh) <<<";
const BASH_COMPLETION_PROFILED_SCRIPT: &str = "/etc/profile.d/bash_completion.sh";
const BASH_COMPLETION_LOADER_CANDIDATES: &[&str] = &[
    "/usr/share/bash-completion/bash_completion",
    "/etc/bash_completion",
];

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
    let custom_path = path_override.is_some();
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
    let activation = ensure_shell_activation(shell, &target_path, custom_path)?;

    println!(
        "installed {} completion: {}",
        shell.label(),
        target_path.display()
    );
    println!("activation: {}", activation.summary);
    if let Some(ref location) = activation.location {
        println!("location: {}", location.display());
    }
    if let Some(reason) = activation.reason {
        println!("reason: {reason}");
    }
    if let Some(next_step) = activation.next_step {
        println!("next: {next_step}");
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

fn ensure_shell_activation(
    shell: CompletionShell,
    target_path: &Path,
    custom_path: bool,
) -> Result<CompletionActivation> {
    match shell {
        CompletionShell::Bash => configure_bash_activation(target_path, custom_path),
        CompletionShell::Zsh => {
            let rc_path = resolve_zdotdir_or_home()?.join(".zshrc");
            upsert_managed_block(
                &rc_path,
                ZSH_COMPLETION_START_MARKER,
                ZSH_COMPLETION_END_MARKER,
                &zsh_activation_block(target_path),
            )?;
            Ok(CompletionActivation {
                summary: "managed zsh rc block".to_string(),
                location: Some(rc_path.clone()),
                reason: Some(
                    "zsh completion needs both `fpath` and `compinit`, so `za` manages a small rc block"
                        .to_string(),
                ),
                next_step: Some(format!(
                    "open a new zsh shell, or run `source {}`",
                    rc_path.display()
                )),
            })
        }
        CompletionShell::Fish => Ok(CompletionActivation {
            summary: "native fish completion directory".to_string(),
            location: target_path.parent().map(Path::to_path_buf),
            reason: None,
            next_step: Some("open a new fish shell".to_string()),
        }),
        CompletionShell::Elvish => Ok(CompletionActivation {
            summary: "manual profile sourcing".to_string(),
            location: Some(target_path.to_path_buf()),
            reason: Some("`za` does not guess an Elvish profile path automatically".to_string()),
            next_step: Some(format!(
                "source `{}` from your `~/.elvish/rc.elv`",
                target_path.display()
            )),
        }),
        CompletionShell::Powershell => Ok(CompletionActivation {
            summary: "manual profile sourcing".to_string(),
            location: Some(target_path.to_path_buf()),
            reason: Some("`za` does not guess a PowerShell profile path automatically".to_string()),
            next_step: Some(format!(
                "dot-source `{}` from your PowerShell profile",
                target_path.display()
            )),
        }),
    }
}

fn configure_bash_activation(
    target_path: &Path,
    custom_path: bool,
) -> Result<CompletionActivation> {
    let rc_path = resolve_home()?.join(".bashrc");
    let default_path = default_install_path(CompletionShell::Bash)?;
    if !custom_path
        && target_path == default_path
        && let Some(loader_path) = detect_bash_completion_loader()?
    {
        remove_managed_block(
            &rc_path,
            BASH_COMPLETION_START_MARKER,
            BASH_COMPLETION_END_MARKER,
        )?;
        return Ok(CompletionActivation {
            summary: "bash-completion discovery".to_string(),
            location: Some(loader_path),
            reason: Some(
                "detected a bash startup path that already loads the standard bash-completion framework"
                    .to_string(),
            ),
            next_step: Some("open a new bash shell if completion is not already active".to_string()),
        });
    }

    upsert_managed_block(
        &rc_path,
        BASH_COMPLETION_START_MARKER,
        BASH_COMPLETION_END_MARKER,
        &bash_activation_block(target_path),
    )?;
    let reason = if custom_path {
        "custom completion paths are not auto-discovered by bash, so `za` added a managed source block"
            .to_string()
    } else {
        "no standard bash-completion loader was detected, so `za` added a managed source block"
            .to_string()
    };
    Ok(CompletionActivation {
        summary: "managed bash rc block".to_string(),
        location: Some(rc_path.clone()),
        reason: Some(reason),
        next_step: Some(format!(
            "open a new bash shell, or run `. {}`",
            rc_path.display()
        )),
    })
}

fn upsert_managed_block(
    target_path: &Path,
    start_marker: &str,
    end_marker: &str,
    body: &str,
) -> Result<()> {
    let existing = match fs::read_to_string(target_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("read `{}`", target_path.display())),
    };
    let block = format!("{start_marker}\n{body}\n{end_marker}");
    let updated = if let Some(start) = existing.find(start_marker) {
        let end = existing[start..]
            .find(end_marker)
            .map(|offset| start + offset + end_marker.len())
            .ok_or_else(|| {
                anyhow!(
                    "found `{start_marker}` in `{}` without matching `{end_marker}`",
                    target_path.display()
                )
            })?;
        format!(
            "{}{}{}",
            &existing[..start],
            block,
            &existing[end..].trim_start_matches('\n')
        )
    } else if existing.trim().is_empty() {
        format!("{block}\n")
    } else {
        format!("{}\n\n{block}\n", existing.trim_end())
    };
    fs::write(target_path, updated).with_context(|| format!("write `{}`", target_path.display()))
}

fn remove_managed_block(target_path: &Path, start_marker: &str, end_marker: &str) -> Result<()> {
    let existing = match fs::read_to_string(target_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("read `{}`", target_path.display())),
    };
    let Some(start) = existing.find(start_marker) else {
        return Ok(());
    };
    let end = existing[start..]
        .find(end_marker)
        .map(|offset| start + offset + end_marker.len())
        .ok_or_else(|| {
            anyhow!(
                "found `{start_marker}` in `{}` without matching `{end_marker}`",
                target_path.display()
            )
        })?;
    let prefix = existing[..start].trim_end_matches('\n');
    let suffix = existing[end..].trim_start_matches('\n');
    let updated = match (prefix.is_empty(), suffix.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("{suffix}\n"),
        (false, true) => format!("{prefix}\n"),
        (false, false) => format!("{prefix}\n\n{suffix}\n"),
    };
    fs::write(target_path, updated).with_context(|| format!("write `{}`", target_path.display()))
}

fn detect_bash_completion_loader() -> Result<Option<PathBuf>> {
    if let Some(profiled_script) = detect_profiled_bash_completion_script()? {
        return Ok(Some(profiled_script));
    }

    let startup_files = bash_startup_files()?;
    for loader in BASH_COMPLETION_LOADER_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .filter(|path| path.is_file())
    {
        let loader_str = loader.display().to_string();
        if startup_files.iter().any(|path| {
            fs::read_to_string(path)
                .map(|content| content.contains(&loader_str))
                .unwrap_or(false)
        }) {
            return Ok(Some(loader));
        }
    }

    Ok(None)
}

fn detect_profiled_bash_completion_script() -> Result<Option<PathBuf>> {
    let profile_script = PathBuf::from(BASH_COMPLETION_PROFILED_SCRIPT);
    if !profile_script.is_file() {
        return Ok(None);
    }

    let startup_files = bash_startup_files()?;
    if startup_files.iter().any(|path| {
        fs::read_to_string(path)
            .map(|content| {
                content.contains("/etc/profile.d/*.sh") || content.contains("/etc/profile.d/")
            })
            .unwrap_or(false)
    }) {
        return Ok(Some(profile_script));
    }

    Ok(None)
}

fn bash_startup_files() -> Result<Vec<PathBuf>> {
    let home = resolve_home()?;
    let mut files = vec![
        home.join(".bashrc"),
        home.join(".bash_profile"),
        home.join(".bash_login"),
        home.join(".profile"),
        PathBuf::from("/etc/bashrc"),
        PathBuf::from("/etc/bash.bashrc"),
        PathBuf::from("/etc/profile"),
    ];
    if let Ok(entries) = fs::read_dir("/etc/profile.d") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("sh") {
                files.push(path);
            }
        }
    }
    Ok(files)
}

fn bash_activation_block(target_path: &Path) -> String {
    let target_path = shell_single_quote(target_path);
    format!(
        "if ! complete -p za >/dev/null 2>&1 && [ -r {target_path} ]; then\n  . {target_path}\nfi"
    )
}

fn zsh_activation_block(target_path: &Path) -> String {
    let dir = shell_single_quote(target_path.parent().unwrap_or_else(|| Path::new(".")));
    format!(
        "if [ -d {dir} ]; then\n  if (( ${{fpath[(Ie){dir}]}} == 0 )); then\n    fpath=({dir} $fpath)\n  fi\n  autoload -Uz compinit\n  if ! typeset -p _comps >/dev/null 2>&1 || [[ -z \"${{_comps[za]-}}\" ]]; then\n    compinit -i\n  fi\nfi"
    )
}

fn shell_single_quote(path: &Path) -> String {
    let raw = path.display().to_string();
    format!("'{}'", raw.replace('\'', "'\"'\"'"))
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

struct CompletionActivation {
    summary: String,
    location: Option<PathBuf>,
    reason: Option<String>,
    next_step: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::{
        BASH_COMPLETION_END_MARKER, BASH_COMPLETION_START_MARKER, bash_activation_block,
        remove_managed_block, shell_single_quote, upsert_managed_block, zsh_activation_block,
    };
    use anyhow::Result;
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Result<Self> {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time must be after unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "za-completion-test-{name}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path)?;
            Ok(Self { path })
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn upsert_managed_block_replaces_existing_content_idempotently() {
        let dir = TempDir::new("block").expect("temp dir");
        let rc_path = dir.path.join(".bashrc");
        fs::write(
            &rc_path,
            format!(
                "export PATH=/tmp\n{BASH_COMPLETION_START_MARKER}\nold\n{BASH_COMPLETION_END_MARKER}\n"
            ),
        )
        .expect("write rc");

        upsert_managed_block(
            &rc_path,
            BASH_COMPLETION_START_MARKER,
            BASH_COMPLETION_END_MARKER,
            "new",
        )
        .expect("update rc");
        upsert_managed_block(
            &rc_path,
            BASH_COMPLETION_START_MARKER,
            BASH_COMPLETION_END_MARKER,
            "new",
        )
        .expect("update rc");

        let content = fs::read_to_string(&rc_path).expect("read rc");
        assert!(content.contains("export PATH=/tmp"));
        assert!(content.contains("new"));
        assert_eq!(content.matches(BASH_COMPLETION_START_MARKER).count(), 1);
    }

    #[test]
    fn bash_activation_block_sources_when_completion_is_missing() {
        let block = bash_activation_block(&PathBuf::from("/tmp/za completion"));
        assert!(block.contains("complete -p za"));
        assert!(block.contains(". '/tmp/za completion'"));
    }

    #[test]
    fn remove_managed_block_keeps_surrounding_content() {
        let dir = TempDir::new("remove-block").expect("temp dir");
        let rc_path = dir.path.join(".bashrc");
        fs::write(
            &rc_path,
            format!(
                "export PATH=/tmp\n\n{BASH_COMPLETION_START_MARKER}\nmanaged\n{BASH_COMPLETION_END_MARKER}\n\nalias ll='ls -l'\n"
            ),
        )
        .expect("write rc");

        remove_managed_block(
            &rc_path,
            BASH_COMPLETION_START_MARKER,
            BASH_COMPLETION_END_MARKER,
        )
        .expect("remove block");

        let content = fs::read_to_string(&rc_path).expect("read rc");
        assert!(content.contains("export PATH=/tmp"));
        assert!(content.contains("alias ll='ls -l'"));
        assert!(!content.contains(BASH_COMPLETION_START_MARKER));
    }

    #[test]
    fn zsh_activation_block_prepares_fpath_and_compinit() {
        let block = zsh_activation_block(&PathBuf::from("/tmp/.zfunc/_za"));
        assert!(block.contains("fpath=("));
        assert!(block.contains("compinit -i"));
        assert!(block.contains("_comps[za]"));
    }

    #[test]
    fn shell_single_quote_escapes_single_quotes() {
        assert_eq!(
            shell_single_quote(&PathBuf::from("/tmp/za'file")),
            "'/tmp/za'\"'\"'file'"
        );
    }
}
