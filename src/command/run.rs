//! Run managed tools with normalized proxy environment variables.

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

    let status = cmd
        .status()
        .with_context(|| format!("failed to start `{}`", executable.display()))?;

    Ok(status.code().unwrap_or(130))
}

fn resolve_executable_path(name: &str) -> Result<PathBuf> {
    if let Some(path) = resolve_user_managed_active(name)? {
        return Ok(path);
    }
    if let Some(path) = resolve_global_managed_active(name)? {
        return Ok(path);
    }
    if let Some(path) = find_in_path(name) {
        return Ok(path);
    }

    bail!("tool `{name}` is not installed or active. install with `za tool install {name}` first")
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
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let candidate_exe = dir.join(format!("{name}.exe"));
            if is_executable_file(&candidate_exe) {
                return Some(candidate_exe);
            }
        }
    }
    None
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

fn normalized_proxy_env_from_system() -> Result<Vec<(String, String)>> {
    let vars: HashMap<String, String> = env::vars().collect();
    let overrides = crate::command::za_config::load_run_proxy_overrides()?;
    Ok(normalized_proxy_env(&vars, &overrides))
}

fn normalized_proxy_env(
    vars: &HashMap<String, String>,
    overrides: &crate::command::za_config::RunProxyOverrides,
) -> Vec<(String, String)> {
    let env_https = first_non_empty(
        vars,
        &[
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ],
    );
    let env_http = first_non_empty(
        vars,
        &[
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
            "HTTPS_PROXY",
            "https_proxy",
        ],
    );
    let env_all = first_non_empty(
        vars,
        &[
            "ALL_PROXY",
            "all_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ],
    );
    let env_no_proxy = first_non_empty(vars, &["NO_PROXY", "no_proxy"]);

    // Global config overrides shell environment when provided.
    let https = overrides
        .https_proxy
        .clone()
        .or_else(|| overrides.all_proxy.clone())
        .or_else(|| overrides.http_proxy.clone())
        .or(env_https);
    let http = overrides
        .http_proxy
        .clone()
        .or_else(|| overrides.all_proxy.clone())
        .or_else(|| overrides.https_proxy.clone())
        .or(env_http);
    let all = overrides
        .all_proxy
        .clone()
        .or_else(|| overrides.https_proxy.clone())
        .or_else(|| overrides.http_proxy.clone())
        .or(env_all);
    let no_proxy = overrides.no_proxy.clone().or(env_no_proxy);

    let mut out = Vec::new();
    if let Some(value) = https {
        out.push(("HTTPS_PROXY".to_string(), value.clone()));
        out.push(("https_proxy".to_string(), value));
    }
    if let Some(value) = http {
        out.push(("HTTP_PROXY".to_string(), value.clone()));
        out.push(("http_proxy".to_string(), value));
    }
    if let Some(value) = all {
        out.push(("ALL_PROXY".to_string(), value.clone()));
        out.push(("all_proxy".to_string(), value));
    }
    if let Some(value) = no_proxy {
        out.push(("NO_PROXY".to_string(), value.clone()));
        out.push(("no_proxy".to_string(), value));
    }
    out
}

fn first_non_empty(vars: &HashMap<String, String>, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(value) = vars.get(*key) else {
            continue;
        };
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::normalized_proxy_env;
    use crate::command::za_config::RunProxyOverrides;
    use std::collections::HashMap;

    fn as_map(values: Vec<(String, String)>) -> HashMap<String, String> {
        values.into_iter().collect()
    }

    #[test]
    fn normalize_proxy_from_http_only_sets_all_common_keys() {
        let mut vars = HashMap::new();
        vars.insert(
            "HTTP_PROXY".to_string(),
            "http://127.0.0.1:7890".to_string(),
        );

        let out = as_map(normalized_proxy_env(&vars, &RunProxyOverrides::default()));
        assert_eq!(
            out.get("HTTP_PROXY").map(String::as_str),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            out.get("http_proxy").map(String::as_str),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            out.get("HTTPS_PROXY").map(String::as_str),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            out.get("https_proxy").map(String::as_str),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            out.get("ALL_PROXY").map(String::as_str),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            out.get("all_proxy").map(String::as_str),
            Some("http://127.0.0.1:7890")
        );
    }

    #[test]
    fn normalize_proxy_preserves_explicit_http_and_https_values() {
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
        assert_eq!(
            out.get("ALL_PROXY").map(String::as_str),
            Some("http://https.proxy")
        );
    }

    #[test]
    fn normalize_proxy_sets_no_proxy_case_variants() {
        let mut vars = HashMap::new();
        vars.insert("no_proxy".to_string(), "localhost,127.0.0.1".to_string());

        let out = as_map(normalized_proxy_env(&vars, &RunProxyOverrides::default()));
        assert_eq!(
            out.get("NO_PROXY").map(String::as_str),
            Some("localhost,127.0.0.1")
        );
        assert_eq!(
            out.get("no_proxy").map(String::as_str),
            Some("localhost,127.0.0.1")
        );
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
            out.get("HTTPS_PROXY").map(String::as_str),
            Some("http://cfg-https.proxy")
        );
        assert_eq!(
            out.get("NO_PROXY").map(String::as_str),
            Some("localhost,127.0.0.1")
        );
    }
}
