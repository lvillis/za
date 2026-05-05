//! Run tools with preserved proxy environment variables.

use anyhow::{Context, Result, bail};
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const GLOBAL_STORE_DIR: &str = "/var/lib/za/tools/store";
const GLOBAL_CURRENT_DIR: &str = "/var/lib/za/tools/current";
const IDE_AGENT_SHIM_MANAGED_MARKER_PREFIX: &str = "# za-managed: ide-agent-shim";

pub fn run(tool: &str, args: &[String]) -> Result<i32> {
    let canonical = crate::command::tool::canonical_tool_name(tool);
    let executable = resolve_executable_path(&canonical)?;

    let mut cmd = Command::new(&executable);
    cmd.args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    for (key, value) in normalized_proxy_env_from_system()? {
        cmd.env(key, value);
    }
    if canonical == "codex" {
        let workspace = env::current_dir().context("read current working directory")?;
        for (key, value) in crate::command::ai::codex_env_overrides(&workspace)? {
            cmd.env(key, value);
        }
    }

    let status = cmd
        .status()
        .with_context(|| format!("failed to start `{}`", executable.display()))?;

    Ok(status.code().unwrap_or(130))
}

pub(crate) fn resolve_executable_path(name: &str) -> Result<PathBuf> {
    if has_path_component(name) {
        if let Some(path) = find_in_path(name) {
            return Ok(path);
        }
        bail!("`za run` expected an executable path or tool name, but `{name}` is not executable");
    }

    if let Some(path) = resolve_user_managed_active(name)? {
        return Ok(path);
    }
    if let Some(path) = resolve_global_managed_active(name)? {
        return Ok(path);
    }
    if let Some(path) = find_in_path(name) {
        return Ok(path);
    }

    bail!(
        "tool `{name}` is not installed or active. install it with `za tool install {name}` first"
    )
}

fn has_path_component(name: &str) -> bool {
    Path::new(name).components().count() > 1
}

fn resolve_user_managed_active(name: &str) -> Result<Option<PathBuf>> {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return Ok(None);
    };
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let state_home = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));

    resolve_managed_active(
        name,
        &data_home.join("za/tools/store"),
        &state_home.join("za/tools/current"),
    )
}

fn resolve_global_managed_active(name: &str) -> Result<Option<PathBuf>> {
    resolve_managed_active(
        name,
        Path::new(GLOBAL_STORE_DIR),
        Path::new(GLOBAL_CURRENT_DIR),
    )
}

fn resolve_managed_active(
    name: &str,
    store_dir: &Path,
    current_dir: &Path,
) -> Result<Option<PathBuf>> {
    let current_file = current_dir.join(name);
    if !current_file.exists() {
        return Ok(None);
    }

    let version = fs::read_to_string(&current_file)
        .with_context(|| format!("read active version file {}", current_file.display()))?;
    let version = version.trim();
    if version.is_empty() {
        return Ok(None);
    }

    let executable = store_dir.join(name).join(version).join(name);
    if !is_executable_file(&executable) {
        bail!(
            "active `{name}` version `{version}` points to missing executable `{}`; repair with `za tool update {name}`",
            executable.display()
        );
    }
    Ok(Some(executable))
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 && is_executable_file(path) {
        return Some(path.to_path_buf());
    }

    let path_env = env::var_os("PATH")?;
    for dir in env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) && !is_ide_agent_shim(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let candidate_exe = dir.join(format!("{name}.exe"));
            if is_executable_file(&candidate_exe) && !is_ide_agent_shim(&candidate_exe) {
                return Some(candidate_exe);
            }
        }
    }
    None
}

fn is_ide_agent_shim(path: &Path) -> bool {
    fs::read_to_string(path)
        .is_ok_and(|content| content.contains(IDE_AGENT_SHIM_MANAGED_MARKER_PREFIX))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub(crate) fn normalized_proxy_env_from_system() -> Result<Vec<(String, String)>> {
    let vars: HashMap<String, String> = env::vars().collect();
    let overrides = crate::command::za_config::load_run_proxy_overrides()?;
    Ok(normalized_proxy_env(&vars, &overrides))
}

fn normalized_proxy_env(
    vars: &HashMap<String, String>,
    overrides: &crate::command::za_config::RunProxyOverrides,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    preserve_existing_proxy_env(vars, &mut out);

    if let Some(value) = overrides.https_proxy.as_deref() {
        set_proxy_pair(&mut out, "HTTPS_PROXY", "https_proxy", value);
    }
    if let Some(value) = overrides.http_proxy.as_deref() {
        set_proxy_pair(&mut out, "HTTP_PROXY", "http_proxy", value);
    }
    if let Some(value) = overrides.all_proxy.as_deref() {
        set_proxy_pair(&mut out, "ALL_PROXY", "all_proxy", value);
    }
    if let Some(value) = overrides.no_proxy.as_deref() {
        set_proxy_pair(&mut out, "NO_PROXY", "no_proxy", value);
    }

    out
}

fn preserve_existing_proxy_env(vars: &HashMap<String, String>, out: &mut Vec<(String, String)>) {
    for key in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        if let Some(value) = non_empty_env_value(vars, key) {
            out.push((key.to_string(), value.to_string()));
        }
    }
}

fn set_proxy_pair(out: &mut Vec<(String, String)>, upper: &str, lower: &str, value: &str) {
    set_proxy_value(out, upper, value);
    set_proxy_value(out, lower, value);
}

fn set_proxy_value(out: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, current)) = out.iter_mut().find(|(existing, _)| existing == key) {
        *current = value.to_string();
    } else {
        out.push((key.to_string(), value.to_string()));
    }
}

fn non_empty_env_value<'a>(vars: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    let value = vars.get(key)?.trim();
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::{normalized_proxy_env, resolve_executable_path};
    use crate::command::za_config::RunProxyOverrides;
    use anyhow::Result;
    use std::{
        collections::HashMap,
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn as_map(values: Vec<(String, String)>) -> HashMap<String, String> {
        values.into_iter().collect()
    }

    #[test]
    fn normalize_proxy_preserves_existing_env_without_expansion() {
        let mut vars = HashMap::new();
        vars.insert(
            "http_proxy".to_string(),
            "http://127.0.0.1:7890".to_string(),
        );
        vars.insert(
            "https_proxy".to_string(),
            "http://127.0.0.1:7890".to_string(),
        );

        let out = as_map(normalized_proxy_env(&vars, &RunProxyOverrides::default()));
        assert_eq!(out.len(), 2);
        assert_eq!(
            out.get("http_proxy").map(String::as_str),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            out.get("https_proxy").map(String::as_str),
            Some("http://127.0.0.1:7890")
        );
        assert!(!out.contains_key("HTTP_PROXY"));
        assert!(!out.contains_key("HTTPS_PROXY"));
        assert!(!out.contains_key("ALL_PROXY"));
        assert!(!out.contains_key("all_proxy"));
    }

    #[test]
    fn normalize_proxy_does_not_derive_all_proxy_from_scheme_values() {
        let mut vars = HashMap::new();
        vars.insert("HTTP_PROXY".to_string(), "http://http.proxy".to_string());
        vars.insert("HTTPS_PROXY".to_string(), "http://https.proxy".to_string());

        let out = as_map(normalized_proxy_env(&vars, &RunProxyOverrides::default()));
        assert_eq!(
            out.get("HTTP_PROXY").map(String::as_str),
            Some("http://http.proxy")
        );
        assert_eq!(
            out.get("HTTPS_PROXY").map(String::as_str),
            Some("http://https.proxy")
        );
        assert!(!out.contains_key("ALL_PROXY"));
        assert!(!out.contains_key("all_proxy"));
    }

    #[test]
    fn normalize_proxy_preserves_no_proxy_exact_case() {
        let mut vars = HashMap::new();
        vars.insert("no_proxy".to_string(), "localhost,127.0.0.1".to_string());

        let out = as_map(normalized_proxy_env(&vars, &RunProxyOverrides::default()));
        assert_eq!(out.len(), 1);
        assert_eq!(
            out.get("no_proxy").map(String::as_str),
            Some("localhost,127.0.0.1")
        );
        assert!(!out.contains_key("NO_PROXY"));
    }

    #[test]
    fn global_run_proxy_overrides_env_proxy_values() {
        let mut vars = HashMap::new();
        vars.insert(
            "HTTP_PROXY".to_string(),
            "http://env-http.proxy".to_string(),
        );
        vars.insert(
            "HTTPS_PROXY".to_string(),
            "http://env-https.proxy".to_string(),
        );

        let overrides = RunProxyOverrides {
            http_proxy: Some("http://cfg-http.proxy".to_string()),
            https_proxy: Some("http://cfg-https.proxy".to_string()),
            all_proxy: None,
            no_proxy: Some("localhost,127.0.0.1".to_string()),
        };

        let out = as_map(normalized_proxy_env(&vars, &overrides));
        assert_eq!(
            out.get("HTTP_PROXY").map(String::as_str),
            Some("http://cfg-http.proxy")
        );
        assert_eq!(
            out.get("http_proxy").map(String::as_str),
            Some("http://cfg-http.proxy")
        );
        assert_eq!(
            out.get("HTTPS_PROXY").map(String::as_str),
            Some("http://cfg-https.proxy")
        );
        assert_eq!(
            out.get("https_proxy").map(String::as_str),
            Some("http://cfg-https.proxy")
        );
        assert_eq!(
            out.get("NO_PROXY").map(String::as_str),
            Some("localhost,127.0.0.1")
        );
        assert_eq!(
            out.get("no_proxy").map(String::as_str),
            Some("localhost,127.0.0.1")
        );
        assert!(!out.contains_key("ALL_PROXY"));
        assert!(!out.contains_key("all_proxy"));
    }

    #[test]
    fn explicit_all_proxy_override_sets_only_all_proxy_pair() {
        let vars = HashMap::new();
        let overrides = RunProxyOverrides {
            http_proxy: None,
            https_proxy: None,
            all_proxy: Some("socks5://cfg-all.proxy".to_string()),
            no_proxy: None,
        };

        let out = as_map(normalized_proxy_env(&vars, &overrides));
        assert_eq!(
            out.get("ALL_PROXY").map(String::as_str),
            Some("socks5://cfg-all.proxy")
        );
        assert_eq!(
            out.get("all_proxy").map(String::as_str),
            Some("socks5://cfg-all.proxy")
        );
        assert!(!out.contains_key("HTTP_PROXY"));
        assert!(!out.contains_key("HTTPS_PROXY"));
    }

    #[test]
    fn resolve_executable_path_accepts_direct_paths_without_managed_lookup() -> Result<()> {
        let path = temp_executable_path("direct-path-tool");
        fs::write(&path, "#!/bin/sh\nexit 0\n")?;
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms)?;
        }

        let resolved = resolve_executable_path(path.to_str().expect("utf-8 temp path"))?;

        assert_eq!(resolved, path);
        let _ = fs::remove_file(&resolved);
        Ok(())
    }

    fn temp_executable_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time must be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "za-run-test-{name}-{}-{unique}",
            std::process::id()
        ))
    }
}
