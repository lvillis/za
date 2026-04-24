//! Shared filesystem path helpers for za-managed state.

use anyhow::{Result, anyhow};
use std::{env, path::PathBuf};

pub(crate) fn data_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        return Ok(path);
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".local/share"))
        .ok_or_else(|| anyhow!("cannot resolve data directory: set `XDG_DATA_HOME` or `HOME`"))
}

pub(crate) fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot resolve home directory: set `HOME`"))
}

pub(crate) fn jetbrains_agent_shim_bin_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".local/bin"))
}

pub(crate) fn legacy_jetbrains_agent_shim_bin_dir() -> Result<PathBuf> {
    Ok(data_home()?.join("za/shims/jetbrains/bin"))
}
