use anyhow::{Result, bail};
use std::env;

const CODEX_GITHUB_OWNER: &str = "openai";
const CODEX_GITHUB_REPO: &str = "codex";
const CODEX_GITHUB_TAG_PREFIX: &str = "rust-v";
const ZA_GITHUB_OWNER: &str = "lvillis";
const ZA_GITHUB_REPO: &str = "za";
const ZA_GITHUB_TAG_PREFIX: &str = "";
const DOCKER_COMPOSE_GITHUB_OWNER: &str = "docker";
const DOCKER_COMPOSE_GITHUB_REPO: &str = "compose";
const DOCKER_COMPOSE_GITHUB_TAG_PREFIX: &str = "v";
const RIPGREP_GITHUB_OWNER: &str = "BurntSushi";
const RIPGREP_GITHUB_REPO: &str = "ripgrep";
const RIPGREP_GITHUB_TAG_PREFIX: &str = "";
const FD_GITHUB_OWNER: &str = "sharkdp";
const FD_GITHUB_REPO: &str = "fd";
const FD_GITHUB_TAG_PREFIX: &str = "v";
const TCPING_GITHUB_OWNER: &str = "lvillis";
const TCPING_GITHUB_REPO: &str = "tcping-rs";
const TCPING_GITHUB_TAG_PREFIX: &str = "";
const DUST_GITHUB_OWNER: &str = "bootandy";
const DUST_GITHUB_REPO: &str = "dust";
const DUST_GITHUB_TAG_PREFIX: &str = "v";
const JUST_GITHUB_OWNER: &str = "casey";
const JUST_GITHUB_REPO: &str = "just";
const JUST_GITHUB_TAG_PREFIX: &str = "";

#[derive(Debug, Clone, Copy)]
pub(super) struct GithubReleasePolicy {
    pub(super) project_label: &'static str,
    pub(super) owner: &'static str,
    pub(super) repo: &'static str,
    pub(super) tag_prefix: &'static str,
    pub(super) expected_asset_name: fn(&str) -> Result<String>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ToolPolicy {
    pub(super) canonical_name: &'static str,
    pub(super) aliases: &'static [&'static str],
    pub(super) source_label: &'static str,
    pub(super) github_release: Option<GithubReleasePolicy>,
    pub(super) cargo_fallback_package: Option<&'static str>,
}

impl ToolPolicy {
    pub(super) fn matches(self, name: &str) -> bool {
        self.canonical_name == name || self.aliases.contains(&name)
    }

    pub(super) fn supported_names(self) -> Vec<&'static str> {
        let mut out = vec![self.canonical_name];
        out.extend(self.aliases.iter().copied());
        out
    }
}

const TOOL_POLICIES: [ToolPolicy; 8] = [
    ToolPolicy {
        canonical_name: "za",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        github_release: Some(GithubReleasePolicy {
            project_label: "za",
            owner: ZA_GITHUB_OWNER,
            repo: ZA_GITHUB_REPO,
            tag_prefix: ZA_GITHUB_TAG_PREFIX,
            expected_asset_name: za_expected_asset_name,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "codex",
        aliases: &["codex-cli"],
        source_label: "GitHub Release (SHA-256 verified), cargo install fallback",
        github_release: Some(GithubReleasePolicy {
            project_label: "codex",
            owner: CODEX_GITHUB_OWNER,
            repo: CODEX_GITHUB_REPO,
            tag_prefix: CODEX_GITHUB_TAG_PREFIX,
            expected_asset_name: codex_expected_asset_name,
        }),
        cargo_fallback_package: Some("codex-cli"),
    },
    ToolPolicy {
        canonical_name: "docker-compose",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        github_release: Some(GithubReleasePolicy {
            project_label: "docker-compose",
            owner: DOCKER_COMPOSE_GITHUB_OWNER,
            repo: DOCKER_COMPOSE_GITHUB_REPO,
            tag_prefix: DOCKER_COMPOSE_GITHUB_TAG_PREFIX,
            expected_asset_name: docker_compose_expected_asset_name,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "rg",
        aliases: &["ripgrep"],
        source_label: "GitHub Release (SHA-256 verified)",
        github_release: Some(GithubReleasePolicy {
            project_label: "ripgrep",
            owner: RIPGREP_GITHUB_OWNER,
            repo: RIPGREP_GITHUB_REPO,
            tag_prefix: RIPGREP_GITHUB_TAG_PREFIX,
            expected_asset_name: ripgrep_expected_asset_name,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "fd",
        aliases: &["fdfind"],
        source_label: "GitHub Release (SHA-256 verified)",
        github_release: Some(GithubReleasePolicy {
            project_label: "fd",
            owner: FD_GITHUB_OWNER,
            repo: FD_GITHUB_REPO,
            tag_prefix: FD_GITHUB_TAG_PREFIX,
            expected_asset_name: fd_expected_asset_name,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "tcping",
        aliases: &["tcping-rs"],
        source_label: "GitHub Release (SHA-256 verified)",
        github_release: Some(GithubReleasePolicy {
            project_label: "tcping-rs",
            owner: TCPING_GITHUB_OWNER,
            repo: TCPING_GITHUB_REPO,
            tag_prefix: TCPING_GITHUB_TAG_PREFIX,
            expected_asset_name: tcping_expected_asset_name,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "dust",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        github_release: Some(GithubReleasePolicy {
            project_label: "dust",
            owner: DUST_GITHUB_OWNER,
            repo: DUST_GITHUB_REPO,
            tag_prefix: DUST_GITHUB_TAG_PREFIX,
            expected_asset_name: dust_expected_asset_name,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "just",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        github_release: Some(GithubReleasePolicy {
            project_label: "just",
            owner: JUST_GITHUB_OWNER,
            repo: JUST_GITHUB_REPO,
            tag_prefix: JUST_GITHUB_TAG_PREFIX,
            expected_asset_name: just_expected_asset_name,
        }),
        cargo_fallback_package: None,
    },
];

pub(super) fn tool_policies() -> &'static [ToolPolicy] {
    &TOOL_POLICIES
}

pub(super) fn find_tool_policy(name: &str) -> Option<ToolPolicy> {
    tool_policies()
        .iter()
        .copied()
        .find(|policy| policy.matches(name))
}

pub(super) fn supported_tool_names_csv() -> String {
    let mut names = Vec::new();
    for policy in tool_policies() {
        names.extend(policy.supported_names());
    }
    names.join(", ")
}

pub(super) fn canonical_tool_name(name: &str) -> String {
    find_tool_policy(name)
        .map(|policy| policy.canonical_name.to_string())
        .unwrap_or_else(|| name.to_string())
}

fn codex_expected_asset_name(_version: &str) -> Result<String> {
    Ok(format!("codex-{}.tar.gz", codex_target_triple()?))
}

fn za_expected_asset_name(version: &str) -> Result<String> {
    Ok(format!("za-{version}-{}.tar.gz", za_target_triple()?))
}

fn docker_compose_expected_asset_name(_version: &str) -> Result<String> {
    Ok(format!("docker-compose-{}", docker_compose_target()?))
}

fn ripgrep_expected_asset_name(version: &str) -> Result<String> {
    Ok(format!(
        "ripgrep-{version}-{}.tar.gz",
        ripgrep_target_triple()?
    ))
}

fn fd_expected_asset_name(version: &str) -> Result<String> {
    Ok(format!("fd-v{version}-{}.tar.gz", fd_target_triple()?))
}

fn tcping_expected_asset_name(version: &str) -> Result<String> {
    Ok(format!(
        "tcping-{version}-{}.tar.gz",
        tcping_target_triple()?
    ))
}

fn dust_expected_asset_name(version: &str) -> Result<String> {
    Ok(format!("dust-v{version}-{}.tar.gz", dust_target_triple()?))
}

fn just_expected_asset_name(version: &str) -> Result<String> {
    Ok(format!("just-{version}-{}.tar.gz", just_target_triple()?))
}

fn codex_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!(
            "unsupported platform for codex release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn za_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!(
            "unsupported platform for za release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn docker_compose_target() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("linux-x86_64"),
        ("linux", "aarch64") => Ok("linux-aarch64"),
        ("macos", "x86_64") => Ok("darwin-x86_64"),
        ("macos", "aarch64") => Ok("darwin-aarch64"),
        ("windows", "x86_64") => Ok("windows-x86_64.exe"),
        ("windows", "aarch64") => Ok("windows-aarch64.exe"),
        _ => bail!(
            "unsupported platform for docker-compose release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn ripgrep_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!(
            "unsupported platform for ripgrep release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn fd_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!(
            "unsupported platform for fd release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn tcping_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!(
            "unsupported platform for tcping-rs release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn dust_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        _ => bail!(
            "unsupported platform for dust release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn just_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!(
            "unsupported platform for just release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}
