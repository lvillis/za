use crate::cli::{CompletionCommands, CompletionShell};
use anyhow::{Context, Result, anyhow};
use shellcomp::{
    ActivationMode, ActivationPolicy, Availability, FileChange, InstallReport, InstallRequest,
    LegacyManagedBlock, MigrateManagedBlocksRequest, Shell as ShellcompShell,
};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

const PROGRAM_NAME: &str = "za";
const BASH_COMPLETION_START_MARKER: &str = "# >>> za completion (bash) >>>";
const BASH_COMPLETION_END_MARKER: &str = "# <<< za completion (bash) <<<";
const ZSH_COMPLETION_START_MARKER: &str = "# >>> za completion (zsh) >>>";
const ZSH_COMPLETION_END_MARKER: &str = "# <<< za completion (zsh) <<<";

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
    let script = render_completion(shell)?;
    let request = InstallRequest {
        shell: shell.into(),
        program_name: PROGRAM_NAME,
        script: &script,
        path_override,
    };
    let report = if custom_path {
        shellcomp::install_with_policy(request, ActivationPolicy::AutoManaged)
    } else {
        shellcomp::install(request)
    }
    .context("install completion")?;
    let legacy_status = reconcile_legacy_markers(shell, &report)
        .context("reconcile legacy za completion markers")?;

    println!(
        "installed {} completion: {}",
        shell.label(),
        report.target_path.display()
    );
    println!(
        "activation: {}",
        activation_mode_label(shell, report.activation.mode)
    );
    println!(
        "status: completion file {}{}",
        file_change_label(report.file_change),
        legacy_status_suffix(legacy_status)
    );
    println!(
        "availability: {}",
        availability_label(&report.activation, shell)
    );
    if let Some(location) = &report.activation.location {
        println!("location: {}", location.display());
    }
    if let Some(reason) = &report.activation.reason {
        println!("reason: {reason}");
    }
    if let Some(next_step) = &report.activation.next_step {
        println!("next: {next_step}");
    }

    Ok(0)
}

fn render_completion(shell: CompletionShell) -> Result<Vec<u8>> {
    let generator_shell: shellcomp::clap_complete::Shell = shell.into();
    shellcomp::render_clap_completion::<crate::cli::Cli>(generator_shell, PROGRAM_NAME)
        .context("render clap completion")
}

fn reconcile_legacy_markers(
    shell: CompletionShell,
    report: &InstallReport,
) -> Result<LegacyMarkerStatus> {
    match shell {
        CompletionShell::Bash => reconcile_bash_legacy_markers(report),
        CompletionShell::Zsh => reconcile_migrated_legacy_markers(shell, &report.target_path),
        CompletionShell::Fish | CompletionShell::Elvish | CompletionShell::Powershell => {
            Ok(LegacyMarkerStatus::None)
        }
    }
}

fn reconcile_bash_legacy_markers(report: &InstallReport) -> Result<LegacyMarkerStatus> {
    if report.activation.mode == ActivationMode::SystemLoader {
        let rc_path = resolve_home()?.join(".bashrc");
        let legacy_change = remove_managed_block(
            &rc_path,
            BASH_COMPLETION_START_MARKER,
            BASH_COMPLETION_END_MARKER,
        )?;
        return Ok(match legacy_change {
            FileChange::Removed => LegacyMarkerStatus::CleanedPreviousBashBlock,
            FileChange::Absent => LegacyMarkerStatus::None,
            other => {
                return Err(anyhow!(
                    "unexpected bash legacy cleanup result: {}",
                    file_change_label(other)
                ));
            }
        });
    }

    reconcile_migrated_legacy_markers(CompletionShell::Bash, &report.target_path)
}

fn reconcile_migrated_legacy_markers(
    shell: CompletionShell,
    target_path: &Path,
) -> Result<LegacyMarkerStatus> {
    let legacy_blocks = legacy_managed_blocks(shell);
    if legacy_blocks.is_empty() {
        return Ok(LegacyMarkerStatus::None);
    }

    let report = shellcomp::migrate_managed_blocks(MigrateManagedBlocksRequest {
        shell: shell.into(),
        program_name: PROGRAM_NAME,
        path_override: Some(target_path.to_path_buf()),
        legacy_blocks,
    })
    .context("migrate legacy managed completion block")?;

    Ok(match report.legacy_change {
        FileChange::Removed => LegacyMarkerStatus::MigratedLegacyBlock(shell),
        FileChange::Absent => LegacyMarkerStatus::None,
        other => {
            return Err(anyhow!(
                "unexpected legacy migration result for {}: {}",
                shell.label(),
                file_change_label(other)
            ));
        }
    })
}

fn legacy_managed_blocks(shell: CompletionShell) -> Vec<LegacyManagedBlock> {
    match shell {
        CompletionShell::Bash => vec![LegacyManagedBlock {
            start_marker: BASH_COMPLETION_START_MARKER.to_string(),
            end_marker: BASH_COMPLETION_END_MARKER.to_string(),
        }],
        CompletionShell::Zsh => vec![LegacyManagedBlock {
            start_marker: ZSH_COMPLETION_START_MARKER.to_string(),
            end_marker: ZSH_COMPLETION_END_MARKER.to_string(),
        }],
        CompletionShell::Fish | CompletionShell::Elvish | CompletionShell::Powershell => Vec::new(),
    }
}

fn resolve_home() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot resolve home directory: set `HOME`"))
}

fn remove_managed_block(
    target_path: &Path,
    start_marker: &str,
    end_marker: &str,
) -> Result<FileChange> {
    let existing = match fs::read_to_string(target_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(FileChange::Absent),
        Err(err) => return Err(err).with_context(|| format!("read `{}`", target_path.display())),
    };
    let Some(start) = existing.find(start_marker) else {
        return Ok(FileChange::Absent);
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
    fs::write(target_path, updated)
        .with_context(|| format!("write `{}`", target_path.display()))?;
    Ok(FileChange::Removed)
}

fn activation_mode_label(shell: CompletionShell, mode: ActivationMode) -> &'static str {
    match mode {
        ActivationMode::SystemLoader => match shell {
            CompletionShell::Bash => "system bash-completion loader",
            _ => "system shell completion loader",
        },
        ActivationMode::ManagedRcBlock => match shell {
            CompletionShell::Bash => "managed bash rc block",
            CompletionShell::Zsh => "managed zsh rc block",
            CompletionShell::Elvish => "managed elvish rc block",
            CompletionShell::Powershell => "managed powershell profile block",
            CompletionShell::Fish => "managed shell startup block",
        },
        ActivationMode::NativeDirectory => match shell {
            CompletionShell::Fish => "native fish completion directory",
            _ => "native completion directory",
        },
        ActivationMode::Manual => "manual activation",
    }
}

fn availability_label(report: &shellcomp::ActivationReport, shell: CompletionShell) -> String {
    match report.availability {
        Availability::ActiveNow => "active now".to_string(),
        Availability::AvailableAfterNewShell => {
            format!("available after a new {} shell", shell.label())
        }
        Availability::AvailableAfterSource => match &report.location {
            Some(path) => format!("available after `source {}`", path.display()),
            None => "available after sourcing your shell startup file".to_string(),
        },
        Availability::ManualActionRequired => "manual action required".to_string(),
        Availability::Unknown => "availability unknown".to_string(),
    }
}

fn file_change_label(change: FileChange) -> &'static str {
    match change {
        FileChange::Created => "created",
        FileChange::Updated => "updated",
        FileChange::Unchanged => "unchanged",
        FileChange::Removed => "removed",
        FileChange::Absent => "unchanged",
    }
}

fn legacy_status_suffix(status: LegacyMarkerStatus) -> String {
    match status {
        LegacyMarkerStatus::None => String::new(),
        LegacyMarkerStatus::CleanedPreviousBashBlock => {
            "; cleaned previous za-managed bash rc block".to_string()
        }
        LegacyMarkerStatus::MigratedLegacyBlock(shell) => {
            format!("; migrated previous za-managed {} block", shell.label())
        }
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

impl From<CompletionShell> for shellcomp::clap_complete::Shell {
    fn from(value: CompletionShell) -> Self {
        match value {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Zsh => Self::Zsh,
            CompletionShell::Fish => Self::Fish,
            CompletionShell::Elvish => Self::Elvish,
            CompletionShell::Powershell => Self::PowerShell,
        }
    }
}

impl From<CompletionShell> for ShellcompShell {
    fn from(value: CompletionShell) -> Self {
        match value {
            CompletionShell::Bash => ShellcompShell::Bash,
            CompletionShell::Zsh => ShellcompShell::Zsh,
            CompletionShell::Fish => ShellcompShell::Fish,
            CompletionShell::Elvish => ShellcompShell::Elvish,
            CompletionShell::Powershell => ShellcompShell::Powershell,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LegacyMarkerStatus {
    None,
    CleanedPreviousBashBlock,
    MigratedLegacyBlock(CompletionShell),
}

#[cfg(test)]
mod tests {
    use super::{
        BASH_COMPLETION_END_MARKER, BASH_COMPLETION_START_MARKER, CompletionShell,
        LegacyMarkerStatus, availability_label, file_change_label, legacy_managed_blocks,
        legacy_status_suffix, remove_managed_block,
    };
    use shellcomp::{ActivationMode, ActivationReport, Availability, FileChange};
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> anyhow::Result<Self> {
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
    fn legacy_managed_blocks_match_za_markers() {
        let blocks = legacy_managed_blocks(CompletionShell::Bash);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].start_marker, BASH_COMPLETION_START_MARKER);

        let blocks = legacy_managed_blocks(CompletionShell::Zsh);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].start_marker.contains("za completion (zsh)"));
    }

    #[test]
    fn availability_after_source_includes_location() {
        let report = ActivationReport {
            mode: ActivationMode::ManagedRcBlock,
            availability: Availability::AvailableAfterSource,
            location: Some(PathBuf::from("/tmp/.zshrc")),
            reason: None,
            next_step: None,
        };
        assert_eq!(
            availability_label(&report, CompletionShell::Zsh),
            "available after `source /tmp/.zshrc`"
        );
    }

    #[test]
    fn legacy_status_suffix_mentions_migration() {
        assert_eq!(
            legacy_status_suffix(LegacyMarkerStatus::MigratedLegacyBlock(
                CompletionShell::Bash
            )),
            "; migrated previous za-managed bash block"
        );
    }

    #[test]
    fn absent_file_change_is_reported_as_unchanged() {
        assert_eq!(file_change_label(FileChange::Absent), "unchanged");
    }
}
