//! Shared helpers and constants for all sub-commands.

pub mod ai;
pub mod ci;
pub mod codex;
pub mod completion;
pub mod deps;
pub mod diff;
pub mod r#gen;
pub mod git;
pub mod ide;
pub mod paths;
pub mod pin;
pub mod port;
pub mod render;
pub mod run;
pub mod style;
pub mod tool;
pub mod za_config;

use anyhow::{Context, Result, anyhow};
use humantime::format_rfc3339_seconds;
use ignore::WalkBuilder;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// ---------- constants ----------
pub const DEFAULT_MAX_LINES_PER_FILE: usize = 400;

/// Files to skip regardless of ignore settings.
const SKIP_BASENAMES: &[&str] = &[".gitignore", ".aiignore", "CONTEXT.md"];

/// ---------- data structs ----------
#[derive(Clone)]
pub struct TextFile {
    pub rel: PathBuf,
    pub lines: Vec<String>,
}

#[derive(Clone)]
pub struct BinaryFile {
    pub rel: PathBuf,
    pub bytes: usize,
}

/// ---------- workspace traversal ----------
/// Walk current workspace and collect text & binary files.
/// NOTE:
/// - Text files are fully read into memory as lines.
/// - Binary files are never fully read; we record size via metadata only.
pub fn walk_workspace(include_binary: bool) -> Result<(Vec<TextFile>, Vec<BinaryFile>)> {
    let root = std::env::current_dir()?;

    // Configure spinner: steady tick; hide on non-TTY (e.g., CI).
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner} {wide_msg}")?
            .tick_strings(&["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"]),
    );
    pb.enable_steady_tick(Duration::from_millis(80));
    if !std::io::stderr().is_terminal() {
        pb.set_draw_target(ProgressDrawTarget::hidden());
    }

    let mut texts = Vec::new();
    let mut bins = Vec::new();

    let mut wb = WalkBuilder::new(&root);
    wb.standard_filters(true)
        .hidden(false)
        .add_custom_ignore_filename(".aiignore");

    for dent in wb.build() {
        let dent = dent?;
        let p = dent.path();

        if !p.is_file() {
            continue;
        }
        if p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| SKIP_BASENAMES.contains(&n))
            .unwrap_or(false)
        {
            continue;
        }

        pb.set_message(p.strip_prefix(&root)?.display().to_string());

        let meta = fs::metadata(p)?;
        match fs::read_to_string(p) {
            Ok(txt) => {
                texts.push(TextFile {
                    rel: p.strip_prefix(&root)?.to_path_buf(),
                    lines: txt.lines().map(|s| s.to_owned()).collect(),
                });
            }
            Err(_) if include_binary => {
                bins.push(BinaryFile {
                    rel: p.strip_prefix(&root)?.to_path_buf(),
                    bytes: meta.len() as usize,
                });
            }
            Err(_) => { /* ignore binary if not requested */ }
        }
    }

    pb.finish_and_clear();
    Ok((texts, bins))
}

/// ---------- language detection ----------
pub fn lang_of(path: &Path) -> &'static str {
    // Handle common no-extension filenames.
    if let Some(name) = path.file_name().and_then(OsStr::to_str) {
        if name.eq_ignore_ascii_case("Dockerfile") {
            return "dockerfile";
        }
        if name.eq_ignore_ascii_case("Makefile") {
            return "make";
        }
    }
    let ext = match path.extension().and_then(OsStr::to_str) {
        Some(e) => e.to_ascii_lowercase(),
        None => return "other",
    };
    match ext.as_str() {
        "rs" => "rust",
        "go" => "go",
        "py" => "python",
        "ts" => "typescript",
        "tsx" => "tsx",
        "js" => "javascript",
        "jsx" => "jsx",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "hpp" | "cc" | "cxx" | "hh" => "cpp",
        "cs" => "csharp",
        "kt" | "kts" => "kotlin",
        "php" => "php",
        "rb" => "ruby",
        "swift" => "swift",
        "sh" | "bash" | "zsh" => "shell",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        "md" | "mdx" => "markdown",
        "html" | "htm" => "html",
        "css" | "scss" | "sass" => "css",
        "sql" => "sql",
        "proto" => "protobuf",
        "xml" => "xml",
        _ => "other",
    }
}

/// ---------- Markdown header helper ----------
pub fn md_header(f: &mut impl Write, title: &str) -> io::Result<()> {
    writeln!(
        f,
        "{title}\n_Generated at: {}_\n",
        format_rfc3339_seconds(SystemTime::now())
    )
}

pub(crate) fn write_file_atomically(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    write_file_atomically_inner(path, contents.as_ref(), None)
}

#[cfg(unix)]
pub(crate) fn write_file_atomically_with_mode(
    path: &Path,
    contents: impl AsRef<[u8]>,
    mode: u32,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    write_file_atomically_inner(
        path,
        contents.as_ref(),
        Some(fs::Permissions::from_mode(mode)),
    )
}

fn write_file_atomically_inner(
    path: &Path,
    contents: &[u8],
    permissions: Option<fs::Permissions>,
) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }

    let mut last_exists = None;
    for attempt in 0..16 {
        let tmp = atomic_write_temp_path(path, attempt)?;
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                last_exists = Some(err);
                continue;
            }
            Err(err) => return Err(err).with_context(|| format!("create {}", tmp.display())),
        };

        let permissions = permissions.clone().or_else(|| {
            fs::metadata(path)
                .ok()
                .map(|metadata| metadata.permissions())
        });
        if let Some(permissions) = permissions
            && let Err(err) = fs::set_permissions(&tmp, permissions)
        {
            let _ = fs::remove_file(&tmp);
            return Err(err)
                .with_context(|| format!("preserve permissions for {}", path.display()));
        }

        if let Err(err) = file.write_all(contents) {
            let _ = fs::remove_file(&tmp);
            return Err(err).with_context(|| format!("write {}", tmp.display()));
        }
        if let Err(err) = file.flush() {
            let _ = fs::remove_file(&tmp);
            return Err(err).with_context(|| format!("flush {}", tmp.display()));
        }
        if let Err(err) = file.sync_all() {
            let _ = fs::remove_file(&tmp);
            return Err(err).with_context(|| format!("sync {}", tmp.display()));
        }
        drop(file);

        if let Err(err) = replace_file(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(err)
                .with_context(|| format!("replace {} with {}", path.display(), tmp.display()));
        }
        return Ok(());
    }

    Err(last_exists
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow!("could not allocate temporary file for {}", path.display())))
}

fn atomic_write_temp_path(path: &Path, attempt: usize) -> Result<PathBuf> {
    let mut name = path
        .file_name()
        .map(OsString::from)
        .ok_or_else(|| anyhow!("path has no file name: {}", path.display()))?;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    name.push(format!(".tmp-za-{}-{nanos}-{attempt}", std::process::id()));
    Ok(path.with_file_name(name))
}

#[cfg(not(windows))]
fn replace_file(src: &Path, dst: &Path) -> io::Result<()> {
    fs::rename(src, dst)
}

#[cfg(windows)]
fn replace_file(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            match fs::remove_file(dst) {
                Ok(()) => {}
                Err(remove_err) if remove_err.kind() == io::ErrorKind::NotFound => {}
                Err(remove_err) => return Err(remove_err),
            }
            fs::rename(src, dst)
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("za-{name}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn write_file_atomically_replaces_contents() {
        let dir = unique_temp_dir("atomic-write");
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("state.json");

        write_file_atomically(&path, br#"{"state":"old"}"#).expect("write initial file");
        write_file_atomically(&path, br#"{"state":"new"}"#).expect("replace file");

        assert_eq!(
            fs::read_to_string(&path).expect("read replaced file"),
            r#"{"state":"new"}"#
        );
        let leftovers = fs::read_dir(&dir)
            .expect("read temp dir")
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-za-"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_file_atomically_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_temp_dir("atomic-write-perms");
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("config");
        fs::write(&path, "old").expect("write initial file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("set initial permissions");

        write_file_atomically(&path, "new").expect("replace file");

        let mode = fs::metadata(&path)
            .expect("read metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(dir);
    }
}
