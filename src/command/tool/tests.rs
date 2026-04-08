use super::policy::GithubReleaseVerification;
use super::{
    InstallOutcome, InstallResult, LatestCheck, ManagedBlockPosition, ManagedFileChange,
    STARSHIP_BASH_INIT_END_MARKER, STARSHIP_BASH_INIT_START_MARKER, ToolBatchKind,
    ToolBatchSummary, ToolHome, ToolRef, ToolScope, ToolSpec, canonical_tool_name,
    cleanup_legacy_current_dir_artifacts, collect_managed_tool_names, command_candidates,
    extract_version_from_text, find_tool_policy, latest_check_progress_message, list_update_status,
    load_sync_specs_from_manifest, normalize_version, prune_non_active_versions,
    render_batch_summary, render_compact_batch_result, source, starship_bash_init_block,
    supported_tool_names_csv, upsert_managed_block,
};
use std::{fs, time::Duration};

#[test]
fn parse_tool_ref_ok() {
    let tool = ToolRef::parse("codex-cli:0.20.0").expect("valid ref");
    assert_eq!(tool.name, "codex-cli");
    assert_eq!(tool.version, "0.20.0");

    let tool = ToolRef::parse("codex-cli@0.21.0").expect("valid ref");
    assert_eq!(tool.name, "codex-cli");
    assert_eq!(tool.version, "0.21.0");
}

#[test]
fn parse_tool_ref_rejects_invalid() {
    assert!(ToolRef::parse("codex-cli").is_err());
    assert!(ToolRef::parse("codex/cli:0.1.0").is_err());
    assert!(ToolRef::parse("codex-cli:").is_err());
}

#[test]
fn command_candidates_include_cli_stripped_name() {
    let cands = command_candidates("codex-cli");
    assert!(cands.iter().any(|c| c == "codex-cli"));
    assert!(cands.iter().any(|c| c == "codex"));
}

#[test]
fn download_filename_reads_url_basename() {
    let url = "https://github.com/openai/codex/releases/download/rust-v0.104.0/codex-x86_64-unknown-linux-musl.tar.gz?x=1";
    let name = source::download_filename(url).expect("valid URL");
    assert_eq!(name, "codex-x86_64-unknown-linux-musl.tar.gz");
}

#[test]
fn tar_asset_detection_works() {
    assert!(source::is_tar_gz_asset("a.tar.gz"));
    assert!(source::is_tar_gz_asset("A.TGZ"));
    assert!(source::is_tar_xz_asset("a.tar.xz"));
    assert!(source::is_tar_xz_asset("A.TXZ"));
    assert!(!source::is_tar_gz_asset("a.zip"));
    assert!(!source::is_tar_gz_asset("codex"));
    assert!(!source::is_tar_xz_asset("a.zip"));
    assert!(!source::is_tar_xz_asset("codex"));
}

#[test]
fn parse_rolling_asset_version_extracts_blesh_nightly_version() {
    assert_eq!(
        source::parse_rolling_asset_version(
            "ble-nightly-20260310+8f3c1ab.tar.xz",
            "ble-nightly-",
            ".tar.xz",
            "nightly-"
        ),
        Some("nightly-20260310+8f3c1ab".to_string())
    );
    assert_eq!(
        source::parse_rolling_asset_version(
            "ble-nightly.tar.xz",
            "ble-nightly-",
            ".tar.xz",
            "nightly-"
        ),
        None
    );
}

#[test]
fn github_sha256_digest_parser_works() {
    assert_eq!(
        source::parse_github_sha256_digest(
            "sha256:74204b12a87031f8fa3ed4218e88d6b9b6879efec99e7ddac79e00a4205bbb28"
        ),
        Some("74204b12a87031f8fa3ed4218e88d6b9b6879efec99e7ddac79e00a4205bbb28".to_string())
    );
    assert!(source::parse_github_sha256_digest("sha512:abcd").is_none());
    assert!(source::parse_github_sha256_digest("sha256:xyz").is_none());
}

#[test]
fn parse_tool_spec_supports_optional_version() {
    let s1 = ToolSpec::parse("codex").expect("valid spec");
    assert_eq!(s1.name, "codex");
    assert!(s1.version.is_none());

    let s2 = ToolSpec::parse("codex:0.104.0").expect("valid spec");
    assert_eq!(s2.name, "codex");
    assert_eq!(s2.version.as_deref(), Some("0.104.0"));

    let s3 = ToolSpec::parse("codex@0.105.0").expect("valid spec");
    assert_eq!(s3.name, "codex");
    assert_eq!(s3.version.as_deref(), Some("0.105.0"));

    assert!(ToolSpec::parse("codex:").is_err());
    assert!(ToolSpec::parse("codex@").is_err());
}

#[test]
fn load_sync_specs_normalizes_and_deduplicates() {
    let root = std::env::temp_dir().join(format!(
        "za-test-sync-manifest-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("create temp root");
    let manifest_path = root.join("za.tools.toml");
    fs::write(
        &manifest_path,
        r#"
tools = [
  "codex-cli",
  "codex",
  "ripgrep",
  "rg",
  "docker-compose@v5.1.0",
]
"#,
    )
    .expect("write manifest");

    let specs = load_sync_specs_from_manifest(&manifest_path).expect("parse manifest");
    assert_eq!(specs, vec!["codex", "rg", "docker-compose:5.1.0"]);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn load_sync_specs_rejects_empty_tools() {
    let root = std::env::temp_dir().join(format!(
        "za-test-sync-manifest-empty-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("create temp root");
    let manifest_path = root.join("za.tools.toml");
    fs::write(&manifest_path, "tools = []\n").expect("write manifest");

    let err = load_sync_specs_from_manifest(&manifest_path).expect_err("must fail");
    assert!(err.to_string().contains("has no tools"));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn normalize_version_strips_leading_v() {
    assert_eq!(normalize_version("v0.104.0"), "0.104.0");
    assert_eq!(normalize_version("0.104.0"), "0.104.0");
}

#[test]
fn parse_release_version_handles_prefixes() {
    assert_eq!(
        source::parse_release_version("rust-v0.104.0", "rust-v").expect("valid tag"),
        "0.104.0"
    );
    assert_eq!(
        source::parse_release_version("v5.1.0", "v").expect("valid tag"),
        "5.1.0"
    );
    assert_eq!(
        source::parse_release_version("5.1.0", "v").expect("valid tag"),
        "5.1.0"
    );
}

#[test]
fn extract_version_from_tool_output() {
    assert_eq!(
        extract_version_from_text("codex-cli 0.104.0"),
        Some("0.104.0".to_string())
    );
    assert_eq!(
        extract_version_from_text("Codex version v0.105.1-beta.2"),
        Some("0.105.1-beta.2".to_string())
    );
}

#[test]
fn extract_version_returns_none_without_semver() {
    assert_eq!(extract_version_from_text("codex unknown"), None);
}

#[test]
fn list_update_status_marks_latest_and_outdated() {
    assert_eq!(
        list_update_status("0.104.0", &LatestCheck::Latest("0.104.0".to_string())),
        "latest"
    );
    assert_eq!(
        list_update_status("0.104.0", &LatestCheck::Latest("0.105.0".to_string())),
        "update -> 0.105.0"
    );
    assert_eq!(
        list_update_status("0.104.0", &LatestCheck::Unsupported),
        "n/a"
    );
    assert_eq!(
        list_update_status("0.104.0", &LatestCheck::Error("boom".to_string())),
        "check-failed"
    );
}

#[test]
fn latest_check_progress_message_summarizes_result() {
    let policy = find_tool_policy("codex").expect("policy");
    assert_eq!(
        latest_check_progress_message(policy, &LatestCheck::Latest("0.114.0".to_string())),
        "codex 0.114.0"
    );
    assert_eq!(
        latest_check_progress_message(policy, &LatestCheck::Unsupported),
        "codex n/a"
    );
    assert_eq!(
        latest_check_progress_message(policy, &LatestCheck::Error("boom".to_string())),
        "codex failed"
    );
}

#[test]
fn render_batch_summary_mentions_updates_repairs_and_failures() {
    let summary = ToolBatchSummary {
        installed: 0,
        updated: 2,
        repaired: 1,
        unchanged: 3,
        failed: 1,
    };
    assert_eq!(
        render_batch_summary(ToolBatchKind::Update, summary, false),
        "2 updated, 1 repaired, 3 already latest, 1 failed"
    );
}

#[test]
fn render_batch_summary_uses_dry_run_wording() {
    let summary = ToolBatchSummary {
        installed: 0,
        updated: 2,
        repaired: 1,
        unchanged: 3,
        failed: 0,
    };
    assert_eq!(
        render_batch_summary(ToolBatchKind::Update, summary, true),
        "2 would update, 1 would repair, 3 already latest"
    );
}

#[test]
fn render_compact_batch_result_shows_updated_versions() {
    let result = InstallResult {
        tool: ToolRef {
            name: "codex".to_string(),
            version: "0.118.0".to_string(),
        },
        outcome: InstallOutcome::Updated,
        previous_active: Some("0.117.0".to_string()),
    };
    assert_eq!(
        render_compact_batch_result(ToolBatchKind::Update, &result, false),
        ("update", "`codex` 0.117.0 -> 0.118.0".to_string())
    );
}

#[test]
fn render_compact_batch_result_shows_repair_concisely() {
    let result = InstallResult {
        tool: ToolRef {
            name: "ble.sh".to_string(),
            version: "nightly-20260310+b99cadb".to_string(),
        },
        outcome: InstallOutcome::Repaired,
        previous_active: Some("nightly-20260310+b99cadb".to_string()),
    };
    assert_eq!(
        render_compact_batch_result(ToolBatchKind::Update, &result, false),
        ("repair", "`ble.sh` nightly-20260310+b99cadb".to_string())
    );
}

#[test]
fn proxy_env_keys_order_matches_scheme() {
    assert_eq!(
        source::proxy_env_keys_for_scheme("https"),
        &[
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ]
    );
    assert_eq!(
        source::proxy_env_keys_for_scheme("http"),
        &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    );
}

#[test]
fn split_no_proxy_rules_ignores_empty_entries() {
    let rules = source::split_no_proxy_rules("localhost, 127.0.0.1, ,.corp.local ,,");
    assert_eq!(rules, vec!["localhost", "127.0.0.1", ".corp.local"]);
}

#[test]
fn render_download_progress_with_total_includes_percentage() {
    let line = source::render_download_progress(512, Some(1024), Duration::from_secs(1));
    assert!(line.contains("50.0%"));
    assert!(line.contains("/ 1.0 KiB"));
}

#[test]
fn render_download_progress_without_total_omits_percentage() {
    let line = source::render_download_progress(512, None, Duration::from_secs(1));
    assert!(!line.contains('%'));
    assert!(line.contains("download:"));
}

#[test]
fn tool_policy_matches_alias_and_canonical() {
    let za = find_tool_policy("za").expect("canonical policy");
    assert_eq!(za.canonical_name, "za");
    let codex_alias = find_tool_policy("codex-cli").expect("alias policy");
    let codex = find_tool_policy("codex").expect("canonical policy");
    assert_eq!(codex_alias.canonical_name, "codex");
    assert_eq!(codex.canonical_name, "codex");
    let rg_alias = find_tool_policy("ripgrep").expect("alias policy");
    let rg = find_tool_policy("rg").expect("canonical policy");
    assert_eq!(rg_alias.canonical_name, "rg");
    assert_eq!(rg.canonical_name, "rg");
    let fd_alias = find_tool_policy("fdfind").expect("alias policy");
    let fd = find_tool_policy("fd").expect("canonical policy");
    assert_eq!(fd_alias.canonical_name, "fd");
    assert_eq!(fd.canonical_name, "fd");
    let tcping_alias = find_tool_policy("tcping-rs").expect("alias policy");
    let tcping = find_tool_policy("tcping").expect("canonical policy");
    assert_eq!(tcping_alias.canonical_name, "tcping");
    assert_eq!(tcping.canonical_name, "tcping");
    let motdyn = find_tool_policy("motdyn").expect("canonical policy");
    assert_eq!(motdyn.canonical_name, "motdyn");
    assert_eq!(motdyn.cargo_fallback_package, None);
    assert_eq!(motdyn.source_label, "GitHub Release (SHA-256 verified)");
    assert_eq!(
        motdyn.github_release.expect("github policy").verification,
        GithubReleaseVerification::RequiredSha256Digest
    );
    let bpftop = find_tool_policy("bpftop").expect("canonical policy");
    assert_eq!(bpftop.canonical_name, "bpftop");
    assert_eq!(bpftop.cargo_fallback_package, None);
    assert_eq!(bpftop.source_label, "GitHub Release (SHA-256 verified)");
    assert_eq!(
        bpftop.github_release.expect("github policy").verification,
        GithubReleaseVerification::RequiredSha256Digest
    );
    let dust = find_tool_policy("dust").expect("canonical policy");
    assert_eq!(dust.canonical_name, "dust");
    let oha = find_tool_policy("oha").expect("canonical policy");
    assert_eq!(oha.canonical_name, "oha");
    let starship = find_tool_policy("starship").expect("canonical policy");
    assert_eq!(starship.canonical_name, "starship");
    assert_eq!(starship.cargo_fallback_package, None);
    assert_eq!(starship.source_label, "GitHub Release (SHA-256 verified)");
    assert_eq!(
        starship.github_release.expect("github policy").verification,
        GithubReleaseVerification::RequiredSha256Digest
    );
    let git_cliff = find_tool_policy("git-cliff").expect("canonical policy");
    assert_eq!(git_cliff.canonical_name, "git-cliff");
    assert_eq!(git_cliff.cargo_fallback_package, None);
    assert_eq!(git_cliff.source_label, "GitHub Release (SHA-256 verified)");
    assert_eq!(
        git_cliff
            .github_release
            .expect("github policy")
            .verification,
        GithubReleaseVerification::RequiredSha256Digest
    );
    let cargo_release = find_tool_policy("cargo-release").expect("canonical policy");
    assert_eq!(cargo_release.canonical_name, "cargo-release");
    assert_eq!(cargo_release.cargo_fallback_package, None);
    assert_eq!(
        cargo_release.source_label,
        "GitHub Release (SHA-256 verified)"
    );
    assert_eq!(
        cargo_release
            .github_release
            .expect("github policy")
            .verification,
        GithubReleaseVerification::RequiredSha256Digest
    );
    let nextest = find_tool_policy("cargo-nextest").expect("canonical policy");
    assert_eq!(nextest.canonical_name, "cargo-nextest");
    assert_eq!(nextest.cargo_fallback_package, None);
    assert_eq!(nextest.source_label, "GitHub Release (SHA-256 verified)");
    assert_eq!(
        nextest.github_release.expect("github policy").verification,
        GithubReleaseVerification::RequiredSha256Digest
    );
    let cross = find_tool_policy("cross").expect("canonical policy");
    assert_eq!(cross.canonical_name, "cross");
    assert_eq!(cross.cargo_fallback_package, None);
    assert_eq!(
        cross.source_label,
        "GitHub Release (SHA-256 unavailable; unverified)"
    );
    assert_eq!(
        cross.github_release.expect("github policy").verification,
        GithubReleaseVerification::NoSha256Digest
    );
    let blesh_alias = find_tool_policy("blesh").expect("alias policy");
    let blesh = find_tool_policy("ble.sh").expect("canonical policy");
    assert_eq!(blesh_alias.canonical_name, "ble.sh");
    assert_eq!(blesh.canonical_name, "ble.sh");
    assert_eq!(blesh.cargo_fallback_package, None);
    assert_eq!(
        blesh.source_label,
        "GitHub nightly rolling release (commit-tracked; SHA-256 unavailable)"
    );
    assert_eq!(
        blesh.github_release.expect("github policy").verification,
        GithubReleaseVerification::NoSha256Digest
    );
    assert!(find_tool_policy("unknown-tool").is_none());
}

#[test]
fn canonical_tool_name_resolves_aliases() {
    assert_eq!(canonical_tool_name("codex-cli"), "codex");
    assert_eq!(canonical_tool_name("ripgrep"), "rg");
    assert_eq!(canonical_tool_name("fdfind"), "fd");
    assert_eq!(canonical_tool_name("tcping-rs"), "tcping");
    assert_eq!(canonical_tool_name("motdyn"), "motdyn");
    assert_eq!(canonical_tool_name("bpftop"), "bpftop");
    assert_eq!(canonical_tool_name("docker-compose"), "docker-compose");
    assert_eq!(canonical_tool_name("oha"), "oha");
    assert_eq!(canonical_tool_name("starship"), "starship");
    assert_eq!(canonical_tool_name("git-cliff"), "git-cliff");
    assert_eq!(canonical_tool_name("cargo-release"), "cargo-release");
    assert_eq!(canonical_tool_name("cargo-nextest"), "cargo-nextest");
    assert_eq!(canonical_tool_name("cross"), "cross");
    assert_eq!(canonical_tool_name("blesh"), "ble.sh");
}

#[test]
fn supported_tool_names_csv_contains_all_aliases() {
    let csv = supported_tool_names_csv();
    assert!(csv.contains("za"));
    assert!(csv.contains("codex"));
    assert!(csv.contains("codex-cli"));
    assert!(csv.contains("docker-compose"));
    assert!(csv.contains("rg"));
    assert!(csv.contains("ripgrep"));
    assert!(csv.contains("fd"));
    assert!(csv.contains("fdfind"));
    assert!(csv.contains("tcping"));
    assert!(csv.contains("tcping-rs"));
    assert!(csv.contains("motdyn"));
    assert!(csv.contains("bpftop"));
    assert!(csv.contains("dust"));
    assert!(csv.contains("just"));
    assert!(csv.contains("oha"));
    assert!(csv.contains("starship"));
    assert!(csv.contains("git-cliff"));
    assert!(csv.contains("cargo-release"));
    assert!(csv.contains("cargo-nextest"));
    assert!(csv.contains("cross"));
    assert!(csv.contains("ble.sh"));
    assert!(csv.contains("blesh"));
}

#[test]
fn starship_bash_init_block_uses_jeditem_guard() {
    let block = starship_bash_init_block();
    assert!(block.contains(r#"if [ "${TERMINAL_EMULATOR-}" = "JetBrains-JediTerm" ]; then"#));
    assert!(block.contains(r#"eval "$(starship init bash)""#));
}

#[test]
fn starship_bash_init_managed_block_is_idempotent() {
    let root = std::env::temp_dir().join(format!(
        "za-test-starship-bashrc-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("create temp root");
    let rc_path = root.join(".bashrc");
    fs::write(&rc_path, "export PATH=/tmp\n").expect("write rc");

    let first = upsert_managed_block(
        &rc_path,
        STARSHIP_BASH_INIT_START_MARKER,
        STARSHIP_BASH_INIT_END_MARKER,
        ManagedBlockPosition::Bottom,
        starship_bash_init_block(),
    )
    .expect("insert block");
    let second = upsert_managed_block(
        &rc_path,
        STARSHIP_BASH_INIT_START_MARKER,
        STARSHIP_BASH_INIT_END_MARKER,
        ManagedBlockPosition::Bottom,
        starship_bash_init_block(),
    )
    .expect("update block");

    let content = fs::read_to_string(&rc_path).expect("read rc");
    assert_eq!(first, ManagedFileChange::Created);
    assert_eq!(second, ManagedFileChange::Unchanged);
    assert!(content.contains("export PATH=/tmp"));
    assert!(content.contains("JetBrains-JediTerm"));
    assert_eq!(content.matches(STARSHIP_BASH_INIT_START_MARKER).count(), 1);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn starship_policy_expected_asset_name_matches_supported_tarball() {
    let policy = find_tool_policy("starship")
        .expect("policy")
        .github_release
        .expect("github policy");
    let asset_name =
        (policy.expected_asset_name.expect("asset resolver"))("1.24.2").expect("asset name");
    let expected_target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        other => panic!("unsupported local test platform: {other:?}"),
    };
    assert_eq!(asset_name, format!("starship-{expected_target}.tar.gz"));
}

#[test]
fn git_cliff_policy_expected_asset_name_matches_supported_tarball() {
    let policy = find_tool_policy("git-cliff")
        .expect("policy")
        .github_release
        .expect("github policy");
    let asset_name =
        (policy.expected_asset_name.expect("asset resolver"))("2.12.0").expect("asset name");
    let expected_target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        other => panic!("unsupported local test platform: {other:?}"),
    };
    assert_eq!(
        asset_name,
        format!("git-cliff-2.12.0-{expected_target}.tar.gz")
    );
}

#[test]
fn cargo_release_policy_expected_asset_name_matches_supported_tarball() {
    let policy = find_tool_policy("cargo-release")
        .expect("policy")
        .github_release
        .expect("github policy");
    let asset_name =
        (policy.expected_asset_name.expect("asset resolver"))("1.1.1").expect("asset name");
    let expected_target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        other => panic!("unsupported local test platform: {other:?}"),
    };
    assert_eq!(
        asset_name,
        format!("cargo-release-v1.1.1-{expected_target}.tar.gz")
    );
}

#[test]
fn nextest_policy_expected_asset_name_matches_supported_tarball() {
    let policy = find_tool_policy("cargo-nextest")
        .expect("policy")
        .github_release
        .expect("github policy");
    let asset_name =
        (policy.expected_asset_name.expect("asset resolver"))("0.9.132").expect("asset name");
    let expected_target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        other => panic!("unsupported local test platform: {other:?}"),
    };
    assert_eq!(
        asset_name,
        format!("cargo-nextest-0.9.132-{expected_target}.tar.gz")
    );
}

#[test]
fn motdyn_policy_expected_asset_name_matches_supported_tarball() {
    let policy = find_tool_policy("motdyn")
        .expect("policy")
        .github_release
        .expect("github policy");
    let asset_name =
        (policy.expected_asset_name.expect("asset resolver"))("1.0.8").expect("asset name");
    let expected_target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        other => panic!("unsupported local test platform: {other:?}"),
    };
    assert_eq!(asset_name, format!("motdyn-1.0.8-{expected_target}.tar.gz"));
}

#[test]
fn bpftop_policy_expected_asset_name_matches_supported_binary() {
    let policy = find_tool_policy("bpftop")
        .expect("policy")
        .github_release
        .expect("github policy");
    let asset_name =
        (policy.expected_asset_name.expect("asset resolver"))("0.7.1").expect("asset name");
    let expected_target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        other => panic!("unsupported local test platform: {other:?}"),
    };
    assert_eq!(asset_name, format!("bpftop-{expected_target}"));
}

#[test]
fn prune_non_active_versions_keeps_only_target_version() {
    let root = std::env::temp_dir().join(format!(
        "za-test-prune-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    let home = ToolHome {
        scope: ToolScope::User,
        store_dir: root.join("store"),
        current_dir: root.join("current"),
        bin_dir: root.join("bin"),
    };

    let old = ToolRef {
        name: "codex".to_string(),
        version: "0.104.0".to_string(),
    };
    let active = ToolRef {
        name: "codex".to_string(),
        version: "0.105.0".to_string(),
    };

    fs::create_dir_all(home.version_dir(&old)).expect("create old version dir");
    fs::create_dir_all(home.version_dir(&active)).expect("create active version dir");

    let removed = prune_non_active_versions(&home, &active).expect("prune versions");
    assert_eq!(removed, vec!["0.104.0".to_string()]);
    assert!(!home.version_dir(&old).exists());
    assert!(home.version_dir(&active).exists());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn collect_managed_tool_names_ignores_legacy_current_artifacts() {
    let root = std::env::temp_dir().join(format!(
        "za-test-current-artifacts-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    let home = ToolHome {
        scope: ToolScope::User,
        store_dir: root.join("store"),
        current_dir: root.join("current"),
        bin_dir: root.join("bin"),
    };

    fs::create_dir_all(&home.current_dir).expect("create current dir");
    fs::create_dir_all(home.name_dir("codex")).expect("create store dir");
    fs::write(home.current_file("rg"), "14.1.0\n").expect("write current version");
    fs::write(home.current_dir.join("za-self-backup-123"), [0_u8, 159]).expect("write backup");
    fs::write(home.current_dir.join("codex.tmp-current-123"), "0.1.0\n").expect("write temp");

    let names = collect_managed_tool_names(&home).expect("collect names");
    assert_eq!(names, vec!["codex".to_string(), "rg".to_string()]);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn cleanup_legacy_current_dir_artifacts_removes_backup_and_temp_files() {
    let root = std::env::temp_dir().join(format!(
        "za-test-cleanup-current-artifacts-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    let home = ToolHome {
        scope: ToolScope::User,
        store_dir: root.join("store"),
        current_dir: root.join("current"),
        bin_dir: root.join("bin"),
    };

    fs::create_dir_all(&home.current_dir).expect("create current dir");
    fs::create_dir_all(home.self_update_backup_dir()).expect("create backup dir");
    fs::write(home.current_dir.join("za-self-backup-123"), [0_u8, 159]).expect("write backup");
    fs::write(home.current_dir.join("codex.tmp-current-123"), "0.1.0\n").expect("write temp");
    fs::write(home.current_file("rg"), "14.1.0\n").expect("write current version");
    fs::write(
        home.self_update_backup_dir().join("za-self-backup-999"),
        [1_u8, 2_u8],
    )
    .expect("write nested backup");

    cleanup_legacy_current_dir_artifacts(&home).expect("cleanup artifacts");

    assert!(home.current_file("rg").exists());
    assert!(!home.current_dir.join("za-self-backup-123").exists());
    assert!(!home.current_dir.join("codex.tmp-current-123").exists());
    assert!(!home.self_update_backup_dir().exists());

    let _ = fs::remove_dir_all(&root);
}
