use super::{
    LatestCheck, ToolHome, ToolRef, ToolScope, ToolSpec, canonical_tool_name, command_candidates,
    extract_version_from_text, find_tool_policy, list_update_status, load_sync_specs_from_manifest,
    normalize_version, prune_non_active_versions, source, supported_tool_names_csv,
};
use std::{fs, time::Duration};

#[test]
fn parse_tool_ref_ok() {
    let tool = ToolRef::parse("codex-cli:0.20.0").expect("valid ref");
    assert_eq!(tool.name, "codex-cli");
    assert_eq!(tool.version, "0.20.0");
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
    assert!(!source::is_tar_gz_asset("a.zip"));
    assert!(!source::is_tar_gz_asset("codex"));
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
  "docker-compose:v5.1.0",
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
    assert!(line.contains("Downloaded"));
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
    let dust = find_tool_policy("dust").expect("canonical policy");
    assert_eq!(dust.canonical_name, "dust");
    assert!(find_tool_policy("unknown-tool").is_none());
}

#[test]
fn canonical_tool_name_resolves_aliases() {
    assert_eq!(canonical_tool_name("codex-cli"), "codex");
    assert_eq!(canonical_tool_name("ripgrep"), "rg");
    assert_eq!(canonical_tool_name("fdfind"), "fd");
    assert_eq!(canonical_tool_name("tcping-rs"), "tcping");
    assert_eq!(canonical_tool_name("docker-compose"), "docker-compose");
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
    assert!(csv.contains("dust"));
    assert!(csv.contains("just"));
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
