//! Shared filesystem path helpers for za-managed state.

use anyhow::{Result, anyhow};
use std::{env, path::PathBuf};

pub(crate) const GLOBAL_TOOL_STORE_DIR: &str = "/var/lib/za/tools/store";
pub(crate) const GLOBAL_TOOL_CURRENT_DIR: &str = "/var/lib/za/tools/current";
pub(crate) const GLOBAL_BIN_DIR: &str = "/usr/local/bin";

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

pub(crate) fn state_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
        return Ok(path);
    }
    Ok(home_dir()?.join(".local/state"))
}

pub(crate) fn user_tool_store_dir() -> Result<PathBuf> {
    Ok(data_home()?.join("za/tools/store"))
}

pub(crate) fn user_tool_current_dir() -> Result<PathBuf> {
    Ok(state_home()?.join("za/tools/current"))
}

pub(crate) fn user_bin_dir() -> Result<PathBuf> {
    user_bin_dir_from_env(
        env::var_os("HOME").map(PathBuf::from),
        env::var_os("ZA_BIN_DIR").map(PathBuf::from),
    )
}

fn user_bin_dir_from_env(home: Option<PathBuf>, za_bin_dir: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = za_bin_dir {
        return Ok(path);
    }
    home.map(|home| home.join(".local/bin"))
        .ok_or_else(|| anyhow!("cannot resolve user bin directory: set `HOME` or `ZA_BIN_DIR`"))
}

pub(crate) fn jetbrains_agent_shim_bin_dir() -> Result<PathBuf> {
    user_bin_dir()
}

pub(crate) fn legacy_jetbrains_agent_shim_bin_dir() -> Result<PathBuf> {
    Ok(data_home()?.join("za/shims/jetbrains/bin"))
}

#[cfg(test)]
mod tests {
    use super::user_bin_dir_from_env;
    use std::path::PathBuf;

    #[test]
    fn user_bin_dir_defaults_to_local_bin() {
        let resolved = user_bin_dir_from_env(Some(PathBuf::from("/home/alice")), None)
            .expect("resolve user bin");

        assert_eq!(resolved, PathBuf::from("/home/alice/.local/bin"));
    }

    #[test]
    fn user_bin_dir_uses_explicit_za_override() {
        let resolved = user_bin_dir_from_env(
            Some(PathBuf::from("/home/alice")),
            Some(PathBuf::from("/opt/za/bin")),
        )
        .expect("resolve user bin");

        assert_eq!(resolved, PathBuf::from("/opt/za/bin"));
    }

    #[test]
    fn user_bin_dir_can_be_resolved_from_za_override_without_home() {
        let resolved = user_bin_dir_from_env(None, Some(PathBuf::from("/opt/za/bin")))
            .expect("resolve user bin");

        assert_eq!(resolved, PathBuf::from("/opt/za/bin"));
    }
}
