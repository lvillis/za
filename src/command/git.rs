//! Git authentication integration utilities.

use crate::{cli::GitAuthCommands, command::za_config};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{
    io::{self, Read},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const GITHUB_HOST: &str = "github.com";
const GITHUB_HELPER_KEY: &str = "credential.https://github.com.helper";
const GITHUB_USERNAME_KEY: &str = "credential.https://github.com.username";
const GITHUB_USERNAME_VALUE: &str = "x-access-token";
const CREDENTIAL_USE_HTTP_PATH_KEY: &str = "credential.useHttpPath";
const ZA_HELPER_COMMAND: &str = "!za gh credential";

#[derive(Debug, Clone, Default)]
struct CredentialRequest {
    protocol: Option<String>,
    host: Option<String>,
    path: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Serialize)]
struct GitAuthStatus {
    git_available: bool,
    git_version: Option<String>,
    helper_configured: bool,
    helper_command: &'static str,
    helper_order: Vec<String>,
    username: Option<String>,
    use_http_path: Option<bool>,
    token_available: bool,
    token_source: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: &'static str,
    ok: bool,
    detail: String,
    hint: Option<String>,
}

#[derive(Debug, Serialize)]
struct GitAuthDoctorReport {
    ok: bool,
    checks: Vec<DoctorCheck>,
}

#[derive(Debug, Serialize)]
struct GitAuthTestReport {
    ok: bool,
    auth_verified: bool,
    anonymous_readable: bool,
    target_url: String,
    remote: Option<String>,
    timeout_secs: u64,
    elapsed_ms: u128,
    timed_out: bool,
    exit_code: Option<i32>,
    reason: String,
    hint: Option<String>,
}

#[derive(Debug, Serialize)]
struct GitAuthRepairReport {
    ok: bool,
    git_version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    actions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
    remote: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_after: Option<String>,
    remote_updated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification: Option<GitAuthTestReport>,
}

#[derive(Debug, Clone)]
struct GitProbeResult {
    success: bool,
    timed_out: bool,
    elapsed_ms: u128,
    exit_code: Option<i32>,
    stderr: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeFailureKind {
    Authentication,
    RepositoryNotFound,
    Proxy,
    Tls,
    Network,
    Unknown,
}

pub fn run_auth(cmd: GitAuthCommands) -> Result<i32> {
    match cmd {
        GitAuthCommands::Enable => run_auth_enable(),
        GitAuthCommands::Status { json } => run_auth_status(json),
        GitAuthCommands::Doctor { json } => run_auth_doctor(json),
        GitAuthCommands::Repair {
            remote,
            timeout_secs,
            json,
        } => run_auth_repair(remote, timeout_secs, json),
        GitAuthCommands::Test {
            repo,
            remote,
            timeout_secs,
            json,
        } => run_auth_test(repo, remote, timeout_secs, json),
        GitAuthCommands::Disable => run_auth_disable(),
    }
}

fn run_auth_enable() -> Result<i32> {
    let git_version = git_version()?;
    let existing = git_config_get_all_global(GITHUB_HELPER_KEY)?;
    let mut helpers = vec![ZA_HELPER_COMMAND.to_string()];
    helpers.extend(
        existing
            .into_iter()
            .filter(|helper| !helper.eq_ignore_ascii_case(ZA_HELPER_COMMAND)),
    );

    rewrite_github_helper_list(&helpers)?;
    git_config_set_global(GITHUB_USERNAME_KEY, GITHUB_USERNAME_VALUE)?;
    git_config_set_global(CREDENTIAL_USE_HTTP_PATH_KEY, "true")?;

    println!("Enabled GitHub credential helper via za.");
    println!("Git: {git_version}");
    println!("Helper order for {GITHUB_HOST}:");
    for helper in helpers {
        println!("- {helper}");
    }
    println!();
    println!("Run `za gh auth doctor` to verify the setup.");
    Ok(0)
}

fn run_auth_disable() -> Result<i32> {
    let git_version = git_version()?;
    let existing = git_config_get_all_global(GITHUB_HELPER_KEY)?;
    let remaining = existing
        .into_iter()
        .filter(|helper| !helper.eq_ignore_ascii_case(ZA_HELPER_COMMAND))
        .collect::<Vec<_>>();

    rewrite_github_helper_list(&remaining)?;

    if git_config_get_global(GITHUB_USERNAME_KEY)?
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case(GITHUB_USERNAME_VALUE))
    {
        git_config_unset_global(GITHUB_USERNAME_KEY)?;
    }

    println!("Disabled za GitHub credential helper wiring.");
    println!("Git: {git_version}");
    if remaining.is_empty() {
        println!("No remaining host-specific helper for {GITHUB_HOST}.");
    } else {
        println!("Remaining helper order for {GITHUB_HOST}:");
        for helper in remaining {
            println!("- {helper}");
        }
    }
    println!("Note: `{CREDENTIAL_USE_HTTP_PATH_KEY}` was left unchanged.");
    Ok(0)
}

fn run_auth_status(json: bool) -> Result<i32> {
    let git_version = git_version().ok();

    if git_version.is_none() {
        let status = GitAuthStatus {
            git_available: false,
            git_version: None,
            helper_configured: false,
            helper_command: ZA_HELPER_COMMAND,
            helper_order: Vec::new(),
            username: None,
            use_http_path: None,
            token_available: false,
            token_source: None,
        };
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&status).context("serialize git auth status")?
            );
            return Ok(0);
        }
        println!("Git available: no");
        println!("Git is not installed or not in PATH.");
        return Ok(1);
    }

    let helper_order = git_config_get_all_global(GITHUB_HELPER_KEY)?;
    let helper_configured = helper_order
        .iter()
        .any(|helper| helper.eq_ignore_ascii_case(ZA_HELPER_COMMAND));
    let username = git_config_get_global(GITHUB_USERNAME_KEY)?;
    let use_http_path = git_config_get_global(CREDENTIAL_USE_HTTP_PATH_KEY)?
        .as_deref()
        .and_then(parse_bool_value);
    let token = resolve_github_token()?;

    let status = GitAuthStatus {
        git_available: true,
        git_version,
        helper_configured,
        helper_command: ZA_HELPER_COMMAND,
        helper_order,
        username,
        use_http_path,
        token_available: token.is_some(),
        token_source: token.map(|(_, source)| source),
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status).context("serialize git auth status")?
        );
        return Ok(0);
    }

    println!(
        "Git available: yes ({})",
        status.git_version.as_deref().unwrap_or("-")
    );
    println!(
        "za helper configured: {}",
        if status.helper_configured {
            "yes"
        } else {
            "no"
        }
    );
    if status.helper_order.is_empty() {
        println!("GitHub helper order: (none)");
    } else {
        println!("GitHub helper order:");
        for helper in status.helper_order {
            println!("- {helper}");
        }
    }
    println!(
        "GitHub username: {}",
        status.username.as_deref().unwrap_or("(unset)")
    );
    println!(
        "credential.useHttpPath: {}",
        status
            .use_http_path
            .map(|value| if value { "true" } else { "false" })
            .unwrap_or("(unset)")
    );
    if let Some(source) = status.token_source {
        println!("GitHub token: available ({source})");
    } else {
        println!("GitHub token: missing");
    }

    Ok(0)
}

fn run_auth_doctor(json: bool) -> Result<i32> {
    let mut checks = Vec::new();

    let git_version = git_version();
    let git_ok = git_version.is_ok();
    checks.push(DoctorCheck {
        name: "git-available",
        ok: git_ok,
        detail: git_version
            .as_ref()
            .map_or_else(|err| err.to_string(), |version| version.clone()),
        hint: if git_ok {
            None
        } else {
            Some("Install Git and ensure `git` is in PATH.".to_string())
        },
    });

    let helper_order = if git_ok {
        git_config_get_all_global(GITHUB_HELPER_KEY)?
    } else {
        Vec::new()
    };
    let helper_configured = helper_order
        .iter()
        .any(|helper| helper.eq_ignore_ascii_case(ZA_HELPER_COMMAND));
    checks.push(DoctorCheck {
        name: "github-helper-configured",
        ok: helper_configured,
        detail: if helper_order.is_empty() {
            "no host-specific helper configured".to_string()
        } else {
            helper_order.join(" | ")
        },
        hint: if helper_configured {
            None
        } else {
            Some("Run `za gh auth enable`.".to_string())
        },
    });

    let helper_first = helper_order
        .first()
        .is_some_and(|helper| helper.eq_ignore_ascii_case(ZA_HELPER_COMMAND));
    checks.push(DoctorCheck {
        name: "za-helper-priority",
        ok: helper_first,
        detail: if helper_order.is_empty() {
            "no host-specific helper configured".to_string()
        } else {
            format!("first helper: {}", helper_order[0])
        },
        hint: if helper_first {
            None
        } else {
            Some("Run `za gh auth enable` to move za helper to the first position.".to_string())
        },
    });

    let username = if git_ok {
        git_config_get_global(GITHUB_USERNAME_KEY)?
    } else {
        None
    };
    let username_ok = username
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case(GITHUB_USERNAME_VALUE));
    checks.push(DoctorCheck {
        name: "github-username",
        ok: username_ok,
        detail: username.unwrap_or_else(|| "(unset)".to_string()),
        hint: if username_ok {
            None
        } else {
            Some(format!(
                "Set `{GITHUB_USERNAME_KEY}` to `{GITHUB_USERNAME_VALUE}` via `za gh auth enable`."
            ))
        },
    });

    let use_http_path = if git_ok {
        git_config_get_global(CREDENTIAL_USE_HTTP_PATH_KEY)?
            .as_deref()
            .and_then(parse_bool_value)
    } else {
        None
    };
    checks.push(DoctorCheck {
        name: "use-http-path",
        ok: use_http_path == Some(true),
        detail: use_http_path
            .map(|value| value.to_string())
            .unwrap_or_else(|| "(unset)".to_string()),
        hint: if use_http_path == Some(true) {
            None
        } else {
            Some(format!(
                "Set `{CREDENTIAL_USE_HTTP_PATH_KEY}` to `true` via `za gh auth enable`."
            ))
        },
    });

    let token = resolve_github_token()?;
    let token_available = token.is_some();
    checks.push(DoctorCheck {
        name: "github-token",
        ok: token_available,
        detail: token
            .as_ref()
            .map(|(_, source)| format!("available from {source}"))
            .unwrap_or_else(|| "missing".to_string()),
        hint: if token_available {
            None
        } else {
            Some(
                "Set `ZA_GITHUB_TOKEN`/`GITHUB_TOKEN`/`GH_TOKEN`, or `za config set github-token <TOKEN>`."
                    .to_string(),
            )
        },
    });

    let ok = checks.iter().all(|check| check.ok);
    let report = GitAuthDoctorReport { ok, checks };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialize git auth doctor report")?
        );
    } else {
        println!(
            "Git auth doctor: {}",
            if ok { "ok" } else { "issues found" }
        );
        for check in report.checks {
            println!(
                "- [{}] {}: {}",
                if check.ok { "ok" } else { "x" },
                check.name,
                check.detail
            );
            if let Some(hint) = check.hint {
                println!("  hint: {hint}");
            }
        }
    }

    Ok(if ok { 0 } else { 1 })
}

fn run_auth_repair(remote: String, timeout_secs: u64, json: bool) -> Result<i32> {
    let git_version = git_version()?;
    let mut actions = Vec::new();
    let mut notes = Vec::new();

    let existing_helpers = git_config_get_all_global(GITHUB_HELPER_KEY)?;
    let mut helpers = vec![ZA_HELPER_COMMAND.to_string()];
    helpers.extend(
        existing_helpers
            .iter()
            .filter(|helper| !helper.eq_ignore_ascii_case(ZA_HELPER_COMMAND))
            .cloned(),
    );
    if existing_helpers != helpers {
        rewrite_github_helper_list(&helpers)?;
        actions.push(format!("set {GITHUB_HOST} helper order with za first"));
    } else {
        notes.push("GitHub helper order already preferred za first".to_string());
    }

    let username = git_config_get_global(GITHUB_USERNAME_KEY)?;
    if username.as_deref() != Some(GITHUB_USERNAME_VALUE) {
        git_config_set_global(GITHUB_USERNAME_KEY, GITHUB_USERNAME_VALUE)?;
        actions.push(format!(
            "set {GITHUB_USERNAME_KEY} = {GITHUB_USERNAME_VALUE}"
        ));
    } else {
        notes.push(format!("{GITHUB_USERNAME_KEY} already set correctly"));
    }

    let use_http_path = git_config_get_global(CREDENTIAL_USE_HTTP_PATH_KEY)?
        .as_deref()
        .and_then(parse_bool_value);
    if use_http_path != Some(true) {
        git_config_set_global(CREDENTIAL_USE_HTTP_PATH_KEY, "true")?;
        actions.push(format!("set {CREDENTIAL_USE_HTTP_PATH_KEY} = true"));
    } else {
        notes.push(format!("{CREDENTIAL_USE_HTTP_PATH_KEY} already true"));
    }

    let remote_before = match git_remote_get_url(&remote) {
        Ok(url) => Some(url),
        Err(err) => {
            notes.push(format!("remote `{remote}` not repaired: {err}"));
            None
        }
    };
    let mut remote_after = remote_before.clone();
    let mut remote_updated = false;
    if let Some(before) = remote_before.as_deref() {
        if let Some(normalized) = normalize_github_remote_url(before) {
            if normalized != before {
                git_remote_set_url(&remote, &normalized)?;
                actions.push(format!("rewrote `{remote}` remote to {normalized}"));
                remote_after = Some(normalized);
                remote_updated = true;
            } else {
                notes.push(format!(
                    "remote `{remote}` already uses clean HTTPS GitHub URL"
                ));
            }
        } else {
            notes.push(format!(
                "remote `{remote}` is not a GitHub URL; left unchanged"
            ));
        }
    }

    let verification_target = remote_after.clone().filter(|target_url| {
        request_targets_github_https(&CredentialRequest {
            protocol: None,
            host: None,
            path: None,
            url: Some(target_url.clone()),
        })
    });
    if remote_after.is_some() && verification_target.is_none() {
        notes.push(
            "verification skipped because the selected remote is not an HTTPS GitHub repo"
                .to_string(),
        );
    }
    let verification = if let Some(target_url) = verification_target {
        match build_auth_test_report(Some(target_url), remote.clone(), timeout_secs) {
            Ok((_git_version, report)) => Some(report),
            Err(err) => {
                notes.push(format!("verification probe failed: {err}"));
                None
            }
        }
    } else {
        None
    };

    let ok = verification
        .as_ref()
        .is_none_or(|report| report.ok || report.anonymous_readable);
    let report = GitAuthRepairReport {
        ok,
        git_version,
        actions,
        notes,
        remote,
        remote_before,
        remote_after,
        remote_updated,
        verification,
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialize git auth repair report")?
        );
    } else {
        println!(
            "Git auth repair: {}",
            if report.ok { "ok" } else { "issues remain" }
        );
        println!("Git: {}", report.git_version);
        for action in &report.actions {
            println!("- {action}");
        }
        if report.actions.is_empty() {
            println!("- no config changes were required");
        }
        if let Some(before) = report.remote_before.as_deref() {
            println!(
                "remote {} before: {}",
                report.remote,
                sanitize_url_for_log(before)
            );
        }
        if let Some(after) = report.remote_after.as_deref() {
            println!(
                "remote {} after:  {}",
                report.remote,
                sanitize_url_for_log(after)
            );
        }
        if let Some(verification) = report.verification.as_ref() {
            println!(
                "verification: {}",
                if verification.ok {
                    format!("passed for {}", verification.target_url)
                } else if verification.anonymous_readable {
                    format!(
                        "inconclusive for {} (repository is anonymously readable)",
                        verification.target_url
                    )
                } else {
                    format!(
                        "failed for {} ({})",
                        verification.target_url, verification.reason
                    )
                }
            );
            if let Some(hint) = verification.hint.as_deref() {
                println!("hint: {hint}");
            }
        } else {
            println!("verification: skipped");
        }
        for note in &report.notes {
            println!("note: {note}");
        }
    }

    Ok(if report.ok { 0 } else { 1 })
}

fn run_auth_test(
    repo: Option<String>,
    remote: String,
    timeout_secs: u64,
    json: bool,
) -> Result<i32> {
    let (git_version, report) = build_auth_test_report(repo, remote, timeout_secs)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialize git auth test report")?
        );
    } else if report.ok {
        println!(
            "Git auth test passed for {} ({} ms).",
            report.target_url, report.elapsed_ms
        );
        println!("Git: {git_version}");
    } else {
        println!("Git auth test failed for {}.", report.target_url);
        println!("Git: {git_version}");
        println!("reason: {}", report.reason);
        if let Some(hint) = report.hint {
            println!("hint: {hint}");
        }
    }

    Ok(if report.ok { 0 } else { 1 })
}

fn build_auth_test_report(
    repo: Option<String>,
    remote: String,
    timeout_secs: u64,
) -> Result<(String, GitAuthTestReport)> {
    let git_version = git_version()?;
    let timeout_secs = timeout_secs.max(1);
    let timeout = Duration::from_secs(timeout_secs);

    let (target_url, remote_used) = if let Some(url) = repo {
        (url, None)
    } else {
        let url = git_remote_get_url(&remote)?;
        (url, Some(remote))
    };
    let target_display = sanitize_url_for_log(&target_url);
    let anonymous_target_url = strip_url_userinfo(&target_url);

    let is_github_https = request_targets_github_https(&CredentialRequest {
        protocol: None,
        host: None,
        path: None,
        url: Some(target_url.clone()),
    });
    if !is_github_https {
        return Ok((
            git_version,
            GitAuthTestReport {
                ok: false,
                auth_verified: false,
                anonymous_readable: false,
                target_url: target_display,
                remote: remote_used,
                timeout_secs,
                elapsed_ms: 0,
                timed_out: false,
                exit_code: None,
                reason: "target URL is not an HTTPS GitHub repository".to_string(),
                hint: Some(
                    "Use an HTTPS GitHub remote, for example `https://github.com/org/repo.git`."
                        .to_string(),
                ),
            },
        ));
    }

    let auth_probe = run_git_ls_remote_probe(&target_url, timeout, false)?;
    if auth_probe.timed_out || !auth_probe.success {
        return Ok((
            git_version,
            build_auth_probe_failure_report(
                &target_display,
                remote_used,
                timeout_secs,
                &auth_probe,
            ),
        ));
    }

    let anon_probe = run_git_ls_remote_probe(&anonymous_target_url, timeout, true)?;
    let report = build_auth_verification_report(
        &target_display,
        remote_used,
        timeout_secs,
        &auth_probe,
        &anon_probe,
    );
    Ok((git_version, report))
}

fn git_remote_get_url(remote: &str) -> Result<String> {
    let output = run_git_args(&["remote", "get-url", remote])?;
    if !output.status.success() {
        bail!(
            "resolve current repo remote `{remote}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    normalize_non_empty(String::from_utf8_lossy(&output.stdout).as_ref())
        .ok_or_else(|| anyhow::anyhow!("remote `{remote}` URL is empty"))
}

fn git_remote_set_url(remote: &str, url: &str) -> Result<()> {
    let output = run_git(["remote", "set-url", remote, url])?;
    if !output.status.success() {
        bail!(
            "update remote `{remote}` URL failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn run_git_ls_remote_probe(
    target_url: &str,
    timeout: Duration,
    disable_helper: bool,
) -> Result<GitProbeResult> {
    let mut cmd = Command::new("git");
    if disable_helper {
        cmd.args([
            "-c",
            "credential.helper=",
            "-c",
            "credential.https://github.com.helper=",
            "-c",
            "credential.interactive=false",
            "-c",
            "core.askPass=",
        ]);
        cmd.env_remove("GIT_ASKPASS");
        cmd.env_remove("SSH_ASKPASS");
    }
    let mut child = cmd
        .args(["ls-remote", "--heads", target_url])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "run git ls-remote --heads {}",
                sanitize_url_for_log(target_url)
            )
        })?;

    let start = Instant::now();
    loop {
        if child
            .try_wait()
            .context("poll git ls-remote process")?
            .is_some()
        {
            let output = child
                .wait_with_output()
                .context("collect git ls-remote output")?;
            return Ok(probe_result_from_output(
                output,
                false,
                start.elapsed().as_millis(),
            ));
        }

        if start.elapsed() >= timeout {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .context("collect timed-out git ls-remote output")?;
            return Ok(probe_result_from_output(
                output,
                true,
                start.elapsed().as_millis(),
            ));
        }

        thread::sleep(Duration::from_millis(50));
    }
}

fn probe_result_from_output(output: Output, timed_out: bool, elapsed_ms: u128) -> GitProbeResult {
    GitProbeResult {
        success: output.status.success(),
        timed_out,
        elapsed_ms,
        exit_code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn build_auth_probe_failure_report(
    target_url: &str,
    remote: Option<String>,
    timeout_secs: u64,
    auth_probe: &GitProbeResult,
) -> GitAuthTestReport {
    let (reason, hint) = if auth_probe.timed_out {
        (
            format!("probe timed out after {}s", timeout_secs),
            Some(
                "Check network/proxy reachability to github.com, then retry with a larger `--timeout-secs`."
                    .to_string(),
            ),
        )
    } else {
        summarize_auth_probe_failure(
            classify_probe_failure(&auth_probe.stderr),
            auth_probe.exit_code,
        )
    };

    GitAuthTestReport {
        ok: false,
        auth_verified: false,
        anonymous_readable: false,
        target_url: target_url.to_string(),
        remote,
        timeout_secs,
        elapsed_ms: auth_probe.elapsed_ms,
        timed_out: auth_probe.timed_out,
        exit_code: auth_probe.exit_code,
        reason,
        hint,
    }
}

fn build_auth_verification_report(
    target_url: &str,
    remote: Option<String>,
    timeout_secs: u64,
    auth_probe: &GitProbeResult,
    anon_probe: &GitProbeResult,
) -> GitAuthTestReport {
    let failure_kind = classify_probe_failure(&anon_probe.stderr);
    let (ok, auth_verified, anonymous_readable, timed_out, reason, hint) = if anon_probe.success {
        (
            false,
            false,
            true,
            false,
            "repository is anonymously readable; auth cannot be verified with this target"
                .to_string(),
            Some(
                "Use a private repository (or one requiring authentication) with `za gh auth test --repo <url>`."
                    .to_string(),
            ),
        )
    } else if anon_probe.timed_out {
        (
            false,
            false,
            false,
            true,
            format!(
                "anonymous comparison probe timed out after {}s; auth verification inconclusive",
                timeout_secs
            ),
            Some(
                "Retry with a larger `--timeout-secs` or check network/proxy stability."
                    .to_string(),
            ),
        )
    } else if matches!(
        failure_kind,
        ProbeFailureKind::Authentication | ProbeFailureKind::RepositoryNotFound
    ) {
        (
            true,
            true,
            false,
            false,
            "authentication verified; authenticated probe succeeded and anonymous probe was rejected (auth/access required)"
                .to_string(),
            None,
        )
    } else {
        let (reason, hint) = summarize_anonymous_probe_failure(failure_kind, anon_probe.exit_code);
        (false, false, false, false, reason, hint)
    };

    GitAuthTestReport {
        ok,
        auth_verified,
        anonymous_readable,
        target_url: target_url.to_string(),
        remote,
        timeout_secs,
        elapsed_ms: auth_probe.elapsed_ms.saturating_add(anon_probe.elapsed_ms),
        timed_out,
        exit_code: anon_probe.exit_code.or(auth_probe.exit_code),
        reason,
        hint,
    }
}

fn summarize_auth_probe_failure(
    kind: ProbeFailureKind,
    exit_code: Option<i32>,
) -> (String, Option<String>) {
    match kind {
        ProbeFailureKind::Authentication => (
            "GitHub authentication failed".to_string(),
            Some(
                "Run `za gh auth doctor`, then ensure a valid token is set and has required repo permissions."
                    .to_string(),
            ),
        ),
        ProbeFailureKind::RepositoryNotFound => (
            "repository not found or token lacks access".to_string(),
            Some("Verify repository URL and token access scope.".to_string()),
        ),
        ProbeFailureKind::Proxy => (
            "proxy connectivity to GitHub failed".to_string(),
            Some(
                "Check HTTPS_PROXY/HTTP_PROXY/ALL_PROXY/NO_PROXY settings, proxy reachability, and any required proxy authentication."
                    .to_string(),
            ),
        ),
        ProbeFailureKind::Tls => (
            "TLS connection to GitHub failed".to_string(),
            Some(
                "Check CA trust configuration, intercepting proxies, and TLS settings for github.com."
                    .to_string(),
            ),
        ),
        ProbeFailureKind::Network => (
            "network connectivity to GitHub failed".to_string(),
            Some("Check DNS/proxy/firewall settings for github.com.".to_string()),
        ),
        ProbeFailureKind::Unknown => (
            format!(
                "git ls-remote failed{}",
                exit_code
                    .map(|code| format!(" (exit code {code})"))
                    .unwrap_or_default()
            ),
            Some(
                "Run `za gh auth doctor` and inspect your Git remote and network settings."
                    .to_string(),
            ),
        ),
    }
}

fn summarize_anonymous_probe_failure(
    kind: ProbeFailureKind,
    exit_code: Option<i32>,
) -> (String, Option<String>) {
    match kind {
        ProbeFailureKind::Proxy => (
            "anonymous comparison probe hit a proxy error; auth verification inconclusive"
                .to_string(),
            Some(
                "Retry after checking HTTPS_PROXY/HTTP_PROXY/ALL_PROXY/NO_PROXY settings and proxy authentication."
                    .to_string(),
            ),
        ),
        ProbeFailureKind::Tls => (
            "anonymous comparison probe hit a TLS error; auth verification inconclusive"
                .to_string(),
            Some(
                "Retry after checking CA trust configuration, intercepting proxies, and TLS settings for github.com."
                    .to_string(),
            ),
        ),
        ProbeFailureKind::Network => (
            "anonymous comparison probe hit a network error; auth verification inconclusive"
                .to_string(),
            Some(
                "Retry with a larger `--timeout-secs` after checking DNS/proxy/firewall reachability to github.com."
                    .to_string(),
            ),
        ),
        ProbeFailureKind::Unknown => (
            format!(
                "anonymous comparison probe failed{}; auth verification inconclusive",
                exit_code
                    .map(|code| format!(" (exit code {code})"))
                    .unwrap_or_default()
            ),
            Some(
                "Retry the probe or inspect local Git transport settings that may affect unauthenticated access."
                    .to_string(),
            ),
        ),
        ProbeFailureKind::Authentication | ProbeFailureKind::RepositoryNotFound => (
            "authentication verified; authenticated probe succeeded and anonymous probe was rejected (auth/access required)"
                .to_string(),
            None,
        ),
    }
}

fn classify_probe_failure(stderr: &str) -> ProbeFailureKind {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("repository") && lower.contains("not found") {
        return ProbeFailureKind::RepositoryNotFound;
    }
    if lower.contains("authentication failed")
        || lower.contains("http basic: access denied")
        || lower.contains("invalid username or password")
        || lower.contains("invalid username or token")
        || lower.contains("could not read password")
        || lower.contains("fatal: could not read username")
        || lower.contains("authentication required")
        || lower.contains("terminal prompts disabled")
    {
        return ProbeFailureKind::Authentication;
    }
    if lower.contains("could not resolve proxy")
        || lower.contains("proxy authentication required")
        || lower.contains("received http code 407 from proxy")
        || lower.contains("proxy connect aborted")
        || lower.contains("failed connect to proxy")
    {
        return ProbeFailureKind::Proxy;
    }
    if lower.contains("server certificate verification failed")
        || lower.contains("ssl certificate problem")
        || lower.contains("tlsv")
        || lower.contains("gnutls_handshake() failed")
        || lower.contains("schannel")
        || lower.contains("peer certificate cannot be authenticated")
        || lower.contains("certificate verify failed")
    {
        return ProbeFailureKind::Tls;
    }
    if lower.contains("could not resolve host")
        || lower.contains("operation timed out")
        || lower.contains("connection timed out")
        || lower.contains("connection refused")
        || lower.contains("network is unreachable")
        || lower.contains("no route to host")
        || lower.contains("failed to connect")
    {
        return ProbeFailureKind::Network;
    }
    ProbeFailureKind::Unknown
}

fn strip_url_userinfo(input: &str) -> String {
    let trimmed = input.trim();
    if let Some((scheme, rest)) = trimmed.split_once("://") {
        let mut authority_and_path = rest;
        let mut suffix = "";
        if let Some((authority, path_suffix)) = rest.split_once('/') {
            authority_and_path = authority;
            suffix = &rest[authority.len()..];
            if path_suffix.is_empty() {
                suffix = "/";
            }
        }
        let authority = authority_and_path
            .rsplit_once('@')
            .map(|(_, host)| host)
            .unwrap_or(authority_and_path);
        return format!("{scheme}://{authority}{suffix}");
    }
    trimmed.to_string()
}

fn sanitize_url_for_log(input: &str) -> String {
    strip_url_userinfo(input)
}

fn normalize_github_remote_url(input: &str) -> Option<String> {
    let trimmed = strip_url_userinfo(input).trim().to_string();
    let path = trimmed
        .strip_prefix("git@github.com:")
        .or_else(|| trimmed.strip_prefix("ssh://git@github.com/"))
        .or_else(|| trimmed.strip_prefix("ssh://github.com/"))
        .or_else(|| trimmed.strip_prefix("git://github.com/"))
        .or_else(|| trimmed.strip_prefix("https://github.com/"))?;

    let repo_path = path
        .split('?')
        .next()
        .unwrap_or(path)
        .split('#')
        .next()
        .unwrap_or(path)
        .trim_start_matches('/')
        .trim();
    if repo_path.is_empty() {
        return None;
    }
    Some(format!("https://github.com/{repo_path}"))
}

pub fn run_credential(operation: Option<String>) -> Result<i32> {
    let op = operation.unwrap_or_else(|| "get".to_string());
    match op.as_str() {
        "get" => run_credential_get(),
        "store" | "erase" => Ok(0),
        _ => bail!("unsupported git credential operation: {op}"),
    }
}

fn run_credential_get() -> Result<i32> {
    let request = read_credential_request()?;
    if !request_targets_github_https(&request) {
        return Ok(0);
    }

    let Some((token, _source)) = resolve_github_token()? else {
        return Ok(0);
    };

    println!("username={GITHUB_USERNAME_VALUE}");
    println!("password={token}");
    println!();
    Ok(0)
}

fn read_credential_request() -> Result<CredentialRequest> {
    let mut raw = String::new();
    io::stdin()
        .read_to_string(&mut raw)
        .context("read git credential request from stdin")?;

    let mut req = CredentialRequest::default();
    for line in raw.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "protocol" => req.protocol = normalize_non_empty(value),
            "host" => req.host = normalize_non_empty(value),
            "path" => req.path = normalize_non_empty(value),
            "url" => req.url = normalize_non_empty(value),
            _ => {}
        }
    }
    Ok(req)
}

fn request_targets_github_https(req: &CredentialRequest) -> bool {
    let protocol = req
        .protocol
        .as_deref()
        .map(str::to_ascii_lowercase)
        .or_else(|| {
            req.url
                .as_deref()
                .and_then(extract_url_scheme)
                .map(str::to_ascii_lowercase)
        });

    if protocol.as_deref() != Some("https") {
        return false;
    }

    let host = req
        .host
        .as_deref()
        .and_then(normalize_host)
        .or_else(|| req.url.as_deref().and_then(extract_url_host));

    host.as_deref() == Some(GITHUB_HOST)
}

fn extract_url_scheme(url: &str) -> Option<&str> {
    url.split_once("://").map(|(scheme, _)| scheme.trim())
}

fn extract_url_host(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split('/').next()?;
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    normalize_host(host_port)
}

fn normalize_host(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_brackets = trimmed.trim_start_matches('[').trim_end_matches(']');
    let host = without_brackets
        .split(':')
        .next()
        .unwrap_or(without_brackets)
        .trim();
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

fn resolve_github_token() -> Result<Option<(String, &'static str)>> {
    if let Some(token) = normalize_token(std::env::var("ZA_GITHUB_TOKEN").ok()) {
        return Ok(Some((token, "ZA_GITHUB_TOKEN")));
    }
    if let Some(token) = normalize_token(std::env::var("GITHUB_TOKEN").ok()) {
        return Ok(Some((token, "GITHUB_TOKEN")));
    }
    if let Some(token) = normalize_token(std::env::var("GH_TOKEN").ok()) {
        return Ok(Some((token, "GH_TOKEN")));
    }
    if let Some(token) = za_config::load_github_token()? {
        return Ok(Some((token, "za-config")));
    }
    Ok(None)
}

fn normalize_token(input: Option<String>) -> Option<String> {
    let value = input?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn normalize_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn parse_bool_value(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn rewrite_github_helper_list(helpers: &[String]) -> Result<()> {
    git_config_unset_all_global(GITHUB_HELPER_KEY)?;
    for helper in helpers {
        git_config_add_global(GITHUB_HELPER_KEY, helper)?;
    }
    Ok(())
}

fn git_version() -> Result<String> {
    let output = run_git(["--version"])?;
    if !output.status.success() {
        bail!(
            "git --version failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_config_get_global(key: &str) -> Result<Option<String>> {
    let output = run_git(["config", "--global", "--get", key])?;
    match output.status.code() {
        Some(0) => Ok(normalize_non_empty(
            String::from_utf8_lossy(&output.stdout).as_ref(),
        )),
        Some(1) => Ok(None),
        _ => bail!(
            "git config --global --get {key} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn git_config_get_all_global(key: &str) -> Result<Vec<String>> {
    let output = run_git(["config", "--global", "--get-all", key])?;
    match output.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()),
        Some(1) => Ok(Vec::new()),
        _ => bail!(
            "git config --global --get-all {key} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn git_config_set_global(key: &str, value: &str) -> Result<()> {
    let output = run_git(["config", "--global", key, value])?;
    if !output.status.success() {
        bail!(
            "git config --global {key} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn git_config_add_global(key: &str, value: &str) -> Result<()> {
    let output = run_git(["config", "--global", "--add", key, value])?;
    if !output.status.success() {
        bail!(
            "git config --global --add {key} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn git_config_unset_global(key: &str) -> Result<()> {
    let output = run_git(["config", "--global", "--unset", key])?;
    match output.status.code() {
        Some(0) | Some(5) => Ok(()),
        _ => bail!(
            "git config --global --unset {key} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn git_config_unset_all_global(key: &str) -> Result<()> {
    let output = run_git(["config", "--global", "--unset-all", key])?;
    match output.status.code() {
        Some(0) | Some(5) => Ok(()),
        _ => bail!(
            "git config --global --unset-all {key} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn run_git<const N: usize>(args: [&str; N]) -> Result<Output> {
    run_git_args(&args)
}

fn run_git_args(args: &[&str]) -> Result<Output> {
    Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::{
        CredentialRequest, GitProbeResult, ProbeFailureKind, build_auth_probe_failure_report,
        build_auth_verification_report, classify_probe_failure, extract_url_host,
        extract_url_scheme, normalize_github_remote_url, normalize_host,
        request_targets_github_https, sanitize_url_for_log, strip_url_userinfo,
    };

    fn probe(
        success: bool,
        timed_out: bool,
        exit_code: Option<i32>,
        stderr: &str,
    ) -> GitProbeResult {
        GitProbeResult {
            success,
            timed_out,
            elapsed_ms: 25,
            exit_code,
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn normalize_host_strips_port_and_userinfo_wrappers() {
        assert_eq!(
            normalize_host("github.com:443").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            normalize_host("[github.com]").as_deref(),
            Some("github.com")
        );
    }

    #[test]
    fn extract_url_components_work_for_https_urls() {
        let url = "https://token@github.com/org/repo.git";
        assert_eq!(extract_url_scheme(url), Some("https"));
        assert_eq!(extract_url_host(url).as_deref(), Some("github.com"));
    }

    #[test]
    fn request_target_matches_github_https() {
        let req = CredentialRequest {
            protocol: Some("https".to_string()),
            host: Some("github.com".to_string()),
            path: Some("owner/repo".to_string()),
            url: None,
        };
        assert!(request_targets_github_https(&req));
    }

    #[test]
    fn request_target_rejects_non_github_or_non_https() {
        let req_http = CredentialRequest {
            protocol: Some("http".to_string()),
            host: Some("github.com".to_string()),
            path: None,
            url: None,
        };
        assert!(!request_targets_github_https(&req_http));

        let req_other = CredentialRequest {
            protocol: Some("https".to_string()),
            host: Some("gitlab.com".to_string()),
            path: None,
            url: None,
        };
        assert!(!request_targets_github_https(&req_other));
    }

    #[test]
    fn sanitize_url_for_log_redacts_userinfo() {
        assert_eq!(
            sanitize_url_for_log("https://token@github.com/org/repo.git"),
            "https://github.com/org/repo.git"
        );
        assert_eq!(
            sanitize_url_for_log("https://user:pass@github.com/org/repo.git"),
            "https://github.com/org/repo.git"
        );
    }

    #[test]
    fn strip_url_userinfo_preserves_urls_without_credentials() {
        assert_eq!(
            strip_url_userinfo("https://github.com/org/repo.git"),
            "https://github.com/org/repo.git"
        );
    }

    #[test]
    fn normalize_github_remote_url_converts_ssh_and_strips_userinfo() {
        assert_eq!(
            normalize_github_remote_url("git@github.com:lvillis/za.git").as_deref(),
            Some("https://github.com/lvillis/za.git")
        );
        assert_eq!(
            normalize_github_remote_url("ssh://git@github.com/lvillis/za.git").as_deref(),
            Some("https://github.com/lvillis/za.git")
        );
        assert_eq!(
            normalize_github_remote_url("https://token@github.com/lvillis/za.git").as_deref(),
            Some("https://github.com/lvillis/za.git")
        );
        assert!(normalize_github_remote_url("https://gitlab.com/lvillis/za.git").is_none());
    }

    #[test]
    fn classify_probe_failure_distinguishes_repository_not_found() {
        assert_eq!(
            classify_probe_failure(
                "fatal: repository 'https://github.com/org/repo.git/' not found"
            ),
            ProbeFailureKind::RepositoryNotFound
        );
        assert_eq!(
            classify_probe_failure(
                "fatal: Authentication failed for 'https://github.com/org/repo.git/'"
            ),
            ProbeFailureKind::Authentication
        );
        assert_eq!(
            classify_probe_failure(
                "fatal: unable to access 'https://github.com/org/repo.git/': Could not resolve proxy: corp.proxy"
            ),
            ProbeFailureKind::Proxy
        );
        assert_eq!(
            classify_probe_failure(
                "fatal: unable to access 'https://github.com/org/repo.git/': SSL certificate problem: self-signed certificate"
            ),
            ProbeFailureKind::Tls
        );
    }

    #[test]
    fn auth_probe_repository_not_found_is_not_reported_as_auth_failure() {
        let report = build_auth_probe_failure_report(
            "https://github.com/org/repo.git",
            None,
            5,
            &probe(
                false,
                false,
                Some(128),
                "fatal: repository 'https://github.com/org/repo.git/' not found",
            ),
        );

        assert!(!report.ok);
        assert!(!report.auth_verified);
        assert_eq!(report.reason, "repository not found or token lacks access");
    }

    #[test]
    fn anonymous_network_failure_is_inconclusive_not_success() {
        let report = build_auth_verification_report(
            "https://github.com/org/private.git",
            None,
            5,
            &probe(true, false, Some(0), ""),
            &probe(
                false,
                false,
                Some(128),
                "fatal: unable to access: Could not resolve host: github.com",
            ),
        );

        assert!(!report.ok);
        assert!(!report.auth_verified);
        assert!(!report.anonymous_readable);
        assert_eq!(
            report.reason,
            "anonymous comparison probe hit a network error; auth verification inconclusive"
        );
    }

    #[test]
    fn anonymous_proxy_failure_is_inconclusive_not_success() {
        let report = build_auth_verification_report(
            "https://github.com/org/private.git",
            None,
            5,
            &probe(true, false, Some(0), ""),
            &probe(
                false,
                false,
                Some(128),
                "fatal: unable to access: Could not resolve proxy: corp.proxy",
            ),
        );

        assert!(!report.ok);
        assert!(!report.auth_verified);
        assert_eq!(
            report.reason,
            "anonymous comparison probe hit a proxy error; auth verification inconclusive"
        );
    }

    #[test]
    fn anonymous_access_rejection_verifies_auth() {
        let report = build_auth_verification_report(
            "https://github.com/org/private.git",
            None,
            5,
            &probe(true, false, Some(0), ""),
            &probe(
                false,
                false,
                Some(128),
                "fatal: repository 'https://github.com/org/private.git/' not found",
            ),
        );

        assert!(report.ok);
        assert!(report.auth_verified);
        assert!(!report.anonymous_readable);
    }
}
