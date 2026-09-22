//! Shared helpers and constants for all sub-commands.

pub mod ci;
pub mod codex;
pub mod completion;
pub mod deps;
pub mod git;
pub mod http;
pub mod ide;
pub mod paths;
pub mod port;
pub mod process;
pub mod render;
pub mod run;
pub mod style;
pub mod tool;
pub mod za_config;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

/// ---------- constants ----------
pub const JSON_SCHEMA_VERSION: u8 = 1;

#[derive(Serialize)]
struct JsonEnvelope<'a, T> {
    schema_version: u8,
    data: &'a T,
}

pub fn json_string<T: Serialize>(value: &T, context: &'static str) -> Result<String> {
    serde_json::to_string_pretty(&JsonEnvelope {
        schema_version: JSON_SCHEMA_VERSION,
        data: value,
    })
    .context(context)
}

pub fn print_json<T: Serialize>(value: &T, context: &'static str) -> Result<()> {
    println!("{}", json_string(value, context)?);
    Ok(())
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

    #[test]
    fn json_output_has_stable_versioned_envelope() {
        let rendered = json_string(&serde_json::json!({"ok": true}), "serialize test output")
            .expect("serialize envelope");
        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("parse envelope");
        assert_eq!(parsed["schema_version"], JSON_SCHEMA_VERSION);
        assert_eq!(parsed["data"]["ok"], true);
        assert_eq!(parsed.as_object().map(serde_json::Map::len), Some(2));
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
