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
const MOTDYN_GITHUB_OWNER: &str = "lvillis";
const MOTDYN_GITHUB_REPO: &str = "motdyn";
const MOTDYN_GITHUB_TAG_PREFIX: &str = "";
const BOTTOM_GITHUB_OWNER: &str = "ClementTsang";
const BOTTOM_GITHUB_REPO: &str = "bottom";
const BOTTOM_GITHUB_TAG_PREFIX: &str = "";
const BPFTOP_GITHUB_OWNER: &str = "Netflix";
const BPFTOP_GITHUB_REPO: &str = "bpftop";
const BPFTOP_GITHUB_TAG_PREFIX: &str = "v";
const HYPERFINE_GITHUB_OWNER: &str = "sharkdp";
const HYPERFINE_GITHUB_REPO: &str = "hyperfine";
const HYPERFINE_GITHUB_TAG_PREFIX: &str = "v";
const DUST_GITHUB_OWNER: &str = "bootandy";
const DUST_GITHUB_REPO: &str = "dust";
const DUST_GITHUB_TAG_PREFIX: &str = "v";
const JUST_GITHUB_OWNER: &str = "casey";
const JUST_GITHUB_REPO: &str = "just";
const JUST_GITHUB_TAG_PREFIX: &str = "";
const OHA_GITHUB_OWNER: &str = "hatoo";
const OHA_GITHUB_REPO: &str = "oha";
const OHA_GITHUB_TAG_PREFIX: &str = "v";
const STARSHIP_GITHUB_OWNER: &str = "starship";
const STARSHIP_GITHUB_REPO: &str = "starship";
const STARSHIP_GITHUB_TAG_PREFIX: &str = "v";
const GIT_CLIFF_GITHUB_OWNER: &str = "orhun";
const GIT_CLIFF_GITHUB_REPO: &str = "git-cliff";
const GIT_CLIFF_GITHUB_TAG_PREFIX: &str = "v";
const CARGO_RELEASE_GITHUB_OWNER: &str = "crate-ci";
const CARGO_RELEASE_GITHUB_REPO: &str = "cargo-release";
const CARGO_RELEASE_GITHUB_TAG_PREFIX: &str = "v";
const NEXTEST_GITHUB_OWNER: &str = "nextest-rs";
const NEXTEST_GITHUB_REPO: &str = "nextest";
const NEXTEST_GITHUB_TAG_PREFIX: &str = "cargo-nextest-";
const CARGO_FUZZ_GITHUB_OWNER: &str = "rust-fuzz";
const CARGO_FUZZ_GITHUB_REPO: &str = "cargo-fuzz";
const CARGO_FUZZ_GITHUB_TAG_PREFIX: &str = "";
const CROSS_GITHUB_OWNER: &str = "cross-rs";
const CROSS_GITHUB_REPO: &str = "cross";
const CROSS_GITHUB_TAG_PREFIX: &str = "v";
const BLESH_GITHUB_OWNER: &str = "akinomyoga";
const BLESH_GITHUB_REPO: &str = "ble.sh";
const BLESH_NIGHTLY_TAG: &str = "nightly";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolLayout {
    Binary,
    Package,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PackagePolicy {
    pub(super) entry_relpath: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GithubReleaseTrack {
    VersionedTags,
    RollingTagAssets {
        tag: &'static str,
        asset_prefix: &'static str,
        asset_suffix: &'static str,
        version_prefix: &'static str,
    },
}

#[derive(Debug, Clone, Copy)]
pub(super) struct GithubReleasePolicy {
    pub(super) project_label: &'static str,
    pub(super) owner: &'static str,
    pub(super) repo: &'static str,
    pub(super) tag_prefix: &'static str,
    pub(super) expected_asset_name: Option<fn(&str) -> Result<String>>,
    pub(super) verification: GithubReleaseVerification,
    pub(super) track: GithubReleaseTrack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GithubReleaseVerification {
    RequiredSha256Digest,
    NoSha256Digest,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ToolPolicy {
    pub(super) canonical_name: &'static str,
    pub(super) aliases: &'static [&'static str],
    pub(super) source_label: &'static str,
    pub(super) layout: ToolLayout,
    pub(super) package: Option<PackagePolicy>,
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

const TOOL_POLICIES: [ToolPolicy; 20] = [
    ToolPolicy {
        canonical_name: "za",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "za",
            owner: ZA_GITHUB_OWNER,
            repo: ZA_GITHUB_REPO,
            tag_prefix: ZA_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(za_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "codex",
        aliases: &["codex-cli"],
        source_label: "GitHub Release (SHA-256 verified), cargo install fallback",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "codex",
            owner: CODEX_GITHUB_OWNER,
            repo: CODEX_GITHUB_REPO,
            tag_prefix: CODEX_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(codex_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: Some("codex-cli"),
    },
    ToolPolicy {
        canonical_name: "docker-compose",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "docker-compose",
            owner: DOCKER_COMPOSE_GITHUB_OWNER,
            repo: DOCKER_COMPOSE_GITHUB_REPO,
            tag_prefix: DOCKER_COMPOSE_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(docker_compose_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "rg",
        aliases: &["ripgrep"],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "ripgrep",
            owner: RIPGREP_GITHUB_OWNER,
            repo: RIPGREP_GITHUB_REPO,
            tag_prefix: RIPGREP_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(ripgrep_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "fd",
        aliases: &["fdfind"],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "fd",
            owner: FD_GITHUB_OWNER,
            repo: FD_GITHUB_REPO,
            tag_prefix: FD_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(fd_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "tcping",
        aliases: &["tcping-rs"],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "tcping-rs",
            owner: TCPING_GITHUB_OWNER,
            repo: TCPING_GITHUB_REPO,
            tag_prefix: TCPING_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(tcping_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "motdyn",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "motdyn",
            owner: MOTDYN_GITHUB_OWNER,
            repo: MOTDYN_GITHUB_REPO,
            tag_prefix: MOTDYN_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(motdyn_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "btm",
        aliases: &["bottom"],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "bottom",
            owner: BOTTOM_GITHUB_OWNER,
            repo: BOTTOM_GITHUB_REPO,
            tag_prefix: BOTTOM_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(bottom_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "bpftop",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "bpftop",
            owner: BPFTOP_GITHUB_OWNER,
            repo: BPFTOP_GITHUB_REPO,
            tag_prefix: BPFTOP_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(bpftop_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "hyperfine",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "hyperfine",
            owner: HYPERFINE_GITHUB_OWNER,
            repo: HYPERFINE_GITHUB_REPO,
            tag_prefix: HYPERFINE_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(hyperfine_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "dust",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "dust",
            owner: DUST_GITHUB_OWNER,
            repo: DUST_GITHUB_REPO,
            tag_prefix: DUST_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(dust_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "just",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "just",
            owner: JUST_GITHUB_OWNER,
            repo: JUST_GITHUB_REPO,
            tag_prefix: JUST_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(just_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "oha",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "oha",
            owner: OHA_GITHUB_OWNER,
            repo: OHA_GITHUB_REPO,
            tag_prefix: OHA_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(oha_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "starship",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "starship",
            owner: STARSHIP_GITHUB_OWNER,
            repo: STARSHIP_GITHUB_REPO,
            tag_prefix: STARSHIP_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(starship_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "git-cliff",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "git-cliff",
            owner: GIT_CLIFF_GITHUB_OWNER,
            repo: GIT_CLIFF_GITHUB_REPO,
            tag_prefix: GIT_CLIFF_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(git_cliff_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "cargo-release",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "cargo-release",
            owner: CARGO_RELEASE_GITHUB_OWNER,
            repo: CARGO_RELEASE_GITHUB_REPO,
            tag_prefix: CARGO_RELEASE_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(cargo_release_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "cargo-nextest",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "cargo-nextest",
            owner: NEXTEST_GITHUB_OWNER,
            repo: NEXTEST_GITHUB_REPO,
            tag_prefix: NEXTEST_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(nextest_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "cargo-fuzz",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 verified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "cargo-fuzz",
            owner: CARGO_FUZZ_GITHUB_OWNER,
            repo: CARGO_FUZZ_GITHUB_REPO,
            tag_prefix: CARGO_FUZZ_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(cargo_fuzz_expected_asset_name),
            verification: GithubReleaseVerification::RequiredSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "cross",
        aliases: &[],
        source_label: "GitHub Release (SHA-256 unavailable; unverified)",
        layout: ToolLayout::Binary,
        package: None,
        github_release: Some(GithubReleasePolicy {
            project_label: "cross",
            owner: CROSS_GITHUB_OWNER,
            repo: CROSS_GITHUB_REPO,
            tag_prefix: CROSS_GITHUB_TAG_PREFIX,
            expected_asset_name: Some(cross_expected_asset_name),
            verification: GithubReleaseVerification::NoSha256Digest,
            track: GithubReleaseTrack::VersionedTags,
        }),
        cargo_fallback_package: None,
    },
    ToolPolicy {
        canonical_name: "ble.sh",
        aliases: &["blesh"],
        source_label: "GitHub nightly rolling release (commit-tracked; SHA-256 unavailable)",
        layout: ToolLayout::Package,
        package: Some(PackagePolicy {
            entry_relpath: "ble.sh",
        }),
        github_release: Some(GithubReleasePolicy {
            project_label: "ble.sh",
            owner: BLESH_GITHUB_OWNER,
            repo: BLESH_GITHUB_REPO,
            tag_prefix: "",
            expected_asset_name: None,
            verification: GithubReleaseVerification::NoSha256Digest,
            track: GithubReleaseTrack::RollingTagAssets {
                tag: BLESH_NIGHTLY_TAG,
                asset_prefix: "ble-nightly-",
                asset_suffix: ".tar.xz",
                version_prefix: "nightly-",
            },
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

fn motdyn_expected_asset_name(version: &str) -> Result<String> {
    Ok(format!(
        "motdyn-{version}-{}.tar.gz",
        motdyn_target_triple()?
    ))
}

fn bottom_expected_asset_name(_version: &str) -> Result<String> {
    Ok(format!("bottom_{}.tar.gz", bottom_target_triple()?))
}

fn bpftop_expected_asset_name(_version: &str) -> Result<String> {
    Ok(format!("bpftop-{}", bpftop_target_triple()?))
}

fn hyperfine_expected_asset_name(version: &str) -> Result<String> {
    Ok(format!(
        "hyperfine-v{version}-{}.tar.gz",
        hyperfine_target_triple()?
    ))
}

fn dust_expected_asset_name(version: &str) -> Result<String> {
    Ok(format!("dust-v{version}-{}.tar.gz", dust_target_triple()?))
}

fn just_expected_asset_name(version: &str) -> Result<String> {
    Ok(format!("just-{version}-{}.tar.gz", just_target_triple()?))
}

fn oha_expected_asset_name(_version: &str) -> Result<String> {
    Ok(format!("oha-{}", oha_target()?))
}

fn starship_expected_asset_name(_version: &str) -> Result<String> {
    Ok(format!("starship-{}.tar.gz", starship_target_triple()?))
}

fn git_cliff_expected_asset_name(version: &str) -> Result<String> {
    Ok(format!(
        "git-cliff-{version}-{}.tar.gz",
        git_cliff_target_triple()?
    ))
}

fn cargo_release_expected_asset_name(version: &str) -> Result<String> {
    Ok(format!(
        "cargo-release-v{version}-{}.tar.gz",
        cargo_release_target_triple()?
    ))
}

fn nextest_expected_asset_name(version: &str) -> Result<String> {
    Ok(format!(
        "cargo-nextest-{version}-{}.tar.gz",
        nextest_target_triple()?
    ))
}

fn cargo_fuzz_expected_asset_name(version: &str) -> Result<String> {
    Ok(format!(
        "cargo-fuzz-{version}-{}.tar.gz",
        cargo_fuzz_target_triple()?
    ))
}

fn cross_expected_asset_name(_version: &str) -> Result<String> {
    Ok(format!("cross-{}.tar.gz", cross_target_triple()?))
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

fn motdyn_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        _ => bail!(
            "unsupported platform for motdyn release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn bottom_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!(
            "unsupported platform for bottom release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn bpftop_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        _ => bail!(
            "unsupported platform for bpftop release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn hyperfine_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!(
            "unsupported platform for hyperfine release asset: {}-{}",
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

fn oha_target() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("linux-amd64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        ("macos", "x86_64") => Ok("macos-amd64"),
        ("macos", "aarch64") => Ok("macos-arm64"),
        _ => bail!(
            "unsupported platform for oha release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn starship_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!(
            "unsupported platform for starship release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn git_cliff_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!(
            "unsupported platform for git-cliff release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn cargo_release_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!(
            "unsupported platform for cargo-release release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn nextest_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => Ok("aarch64-pc-windows-msvc"),
        _ => bail!(
            "unsupported platform for cargo-nextest release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn cargo_fuzz_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        _ => bail!(
            "unsupported platform for cargo-fuzz release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}

fn cross_target_triple() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        _ => bail!(
            "unsupported platform for cross release asset: {}-{}",
            env::consts::ARCH,
            env::consts::OS
        ),
    }
}
