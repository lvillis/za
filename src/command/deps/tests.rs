use super::api::{cache_entry_is_fresh, merge_cache_entries};
use super::latest::{
    LatestQuerySource, LatestRecord, LatestStatus, LatestSuggestionKind, LatestSummary,
    render_empty_latest,
};
use super::render::{render_latest_lines, render_latest_toml, render_report_lines};
use super::{
    ActionAuditRecord, ActionLocation, ActionUpdatePlan, AuditSummary, DepAuditRecord,
    DependencySource, DependencyUpdatePlan, DepsRunOptions, GitHubTagSnapshot, RiskLevel,
    WorkflowActionSpec, WorkspaceDependency, WorkspaceManifest, WorkspacePackage,
    build_action_audit_record, collect_dependency_inventory, collect_dependency_specs,
    derive_auto_jobs, latest_stable_action_tag, parse_action_tag_version, parse_workflow_uses_line,
    read_manifest_metadata, run,
};
use std::{collections::BTreeMap, fs, path::Path};

#[test]
fn auto_jobs_is_bounded() {
    assert_eq!(derive_auto_jobs(1), 4);
    assert_eq!(derive_auto_jobs(2), 4);
    assert_eq!(derive_auto_jobs(4), 8);
    assert_eq!(derive_auto_jobs(8), 16);
    assert_eq!(derive_auto_jobs(32), 16);
}

#[test]
fn cache_freshness_rejects_future_and_expired_entries() {
    assert!(cache_entry_is_fresh(1_000, 950, 60));
    assert!(!cache_entry_is_fresh(1_000, 900, 60));
    assert!(!cache_entry_is_fresh(1_000, 1_001, 60));
}

#[test]
fn cache_merge_keeps_newest_entries_from_concurrent_writers() {
    let mut persisted = BTreeMap::from([
        ("shared".to_string(), (20_u64, "persisted")),
        ("persisted-only".to_string(), (10_u64, "persisted-only")),
    ]);
    let local = BTreeMap::from([
        ("shared".to_string(), (15_u64, "stale-local")),
        ("local-only".to_string(), (30_u64, "local-only")),
    ]);

    merge_cache_entries(&mut persisted, local, |entry| entry.0);

    assert_eq!(persisted["shared"].1, "persisted");
    assert_eq!(persisted["persisted-only"].1, "persisted-only");
    assert_eq!(persisted["local-only"].1, "local-only");
}

#[test]
fn collect_dependency_specs_uses_default_enabled_optional_dependencies() {
    let metadata = sample_metadata();

    let specs = collect_dependency_specs(&metadata, false, false, false).unwrap();
    let names = specs
        .iter()
        .map(|spec| spec.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(names, vec!["bytes", "futures-core"]);
    let futures_core = specs
        .iter()
        .find(|spec| spec.name == "futures-core")
        .unwrap();
    assert_eq!(futures_core.requirement, "^0.3");
    assert_eq!(futures_core.kinds, "normal");
    assert!(futures_core.optional);
}

#[test]
fn collect_dependency_specs_include_optional_adds_inactive_optional_declarations() {
    let metadata = sample_metadata();

    let specs = collect_dependency_specs(&metadata, false, false, true).unwrap();
    let names = specs
        .iter()
        .map(|spec| spec.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(names, vec!["bytes", "futures-core", "hyper"]);
    let hyper = specs.iter().find(|spec| spec.name == "hyper").unwrap();
    assert_eq!(hyper.requirement, "^1");
    assert_eq!(hyper.kinds, "normal");
    assert!(hyper.optional);
}

#[test]
fn collect_dependency_specs_scans_all_workspace_members_even_when_root_is_set() {
    let metadata = workspace_metadata();

    let specs = collect_dependency_specs(&metadata, false, false, false).unwrap();
    let names = specs
        .iter()
        .map(|spec| spec.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(names, vec!["serde", "tokio"]);
}

#[test]
fn collect_dependency_specs_preserves_distinct_workspace_requirements() {
    let mut metadata = workspace_metadata();
    metadata.packages[1].dependencies.push(workspace_dependency(
        "serde",
        DependencySource::CratesIo,
        "^2",
        false,
        true,
    ));

    let specs = collect_dependency_specs(&metadata, false, false, false).unwrap();
    let serde = specs
        .iter()
        .find(|spec| spec.name == "serde")
        .expect("serde dependency");

    assert_eq!(serde.requirement, "^1 | ^2");
    assert_eq!(serde.project_rust_version.as_deref(), Some("1.75"));
}

#[test]
fn collect_dependency_inventory_skips_workspace_and_local_path_crates() {
    let metadata = workspace_metadata();

    let inventory = collect_dependency_inventory(&metadata, false, false, false).unwrap();
    let names = inventory
        .specs
        .iter()
        .map(|spec| spec.name.as_str())
        .collect::<Vec<_>>();
    let skipped = inventory
        .skipped_local
        .iter()
        .map(|spec| spec.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(names, vec!["serde", "tokio"]);
    assert_eq!(skipped, vec!["local-helper", "workspace-core"]);
}

#[test]
fn read_manifest_metadata_scans_workspace_manifests_without_resolving_deps() {
    let root = temp_root("deps-manifest-workspace");
    fs::create_dir_all(root.join("crates/app")).expect("create app crate");
    fs::create_dir_all(root.join("crates/local-helper")).expect("create local crate");

    fs::write(
        root.join("Cargo.toml"),
        r#"
            [workspace]
            members = ["crates/*"]

            [workspace.package]
            rust-version = "1.75"

            [workspace.dependencies]
            anyhow = "1"
            serde = "1"
            local-helper = { path = "crates/local-helper" }
        "#,
    )
    .expect("write workspace manifest");
    fs::write(
        root.join("crates/app/Cargo.toml"),
        r#"
            [package]
            name = "app"
            version = "0.1.0"
            rust-version.workspace = true

            [features]
            default = ["network"]
            network = ["dep:reqx"]

            [dependencies]
            anyhow = { workspace = true }
            serde = { workspace = true }
            local-helper = { workspace = true }
            runtime = { package = "tokio", version = "1" }
            reqx = { version = "0.1", optional = true }
            git-tool = { git = "https://github.com/example/git-tool", rev = "abc123" }
            private-api = { version = "2", registry = "company" }

            [target.'cfg(unix)'.dependencies]
            regex = "1"
        "#,
    )
    .expect("write app manifest");
    fs::write(
        root.join("crates/local-helper/Cargo.toml"),
        r#"
            [package]
            name = "local-helper"
            version = "0.1.0"
        "#,
    )
    .expect("write local manifest");

    let metadata = read_manifest_metadata(&root.join("Cargo.toml")).expect("read metadata");
    let inventory =
        collect_dependency_inventory(&metadata, false, false, false).expect("collect deps");
    let names = inventory
        .specs
        .iter()
        .map(|spec| spec.name.as_str())
        .collect::<Vec<_>>();
    let skipped = inventory
        .skipped_local
        .iter()
        .map(|spec| spec.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec![
            "anyhow",
            "git-tool",
            "private-api",
            "regex",
            "reqx",
            "serde",
            "tokio"
        ]
    );
    assert_eq!(skipped, vec!["local-helper"]);
    let git = inventory
        .specs
        .iter()
        .find(|spec| spec.name == "git-tool")
        .expect("git dependency");
    assert!(matches!(git.source, DependencySource::Git(_)));
    assert_eq!(git.requirement, "rev:abc123");
    let private = inventory
        .specs
        .iter()
        .find(|spec| spec.name == "private-api")
        .expect("registry dependency");
    assert_eq!(
        private.source,
        DependencySource::Registry("company".to_string())
    );
    let reqx = inventory
        .specs
        .iter()
        .find(|spec| spec.name == "reqx")
        .expect("default-enabled optional dependency");
    assert_eq!(reqx.project_rust_version.as_deref(), Some("1.75"));
}

#[test]
fn workspace_discovery_supports_question_globs_and_implicit_path_members() {
    let root = temp_root("deps-workspace-discovery");
    fs::create_dir_all(root.join("crates/app")).expect("create app crate");
    fs::create_dir_all(root.join("crates/helper")).expect("create helper crate");
    fs::write(
        root.join("Cargo.toml"),
        r#"
            [workspace]
            members = ["crates/a?p"]
        "#,
    )
    .expect("write workspace manifest");
    fs::write(
        root.join("crates/app/Cargo.toml"),
        r#"
            [package]
            name = "app"
            version = "0.1.0"

            [dependencies]
            helper = { path = "../helper" }
        "#,
    )
    .expect("write app manifest");
    fs::write(
        root.join("crates/helper/Cargo.toml"),
        r#"
            [package]
            name = "helper"
            version = "0.1.0"

            [dependencies]
            serde = "1"
        "#,
    )
    .expect("write helper manifest");

    let metadata = read_manifest_metadata(&root.join("Cargo.toml")).expect("read metadata");
    assert_eq!(metadata.packages.len(), 2);
    let inventory =
        collect_dependency_inventory(&metadata, false, false, false).expect("collect deps");
    assert_eq!(
        inventory
            .specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>(),
        vec!["serde"]
    );
    assert_eq!(
        inventory
            .skipped_local
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>(),
        vec!["helper"]
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_discovery_resolves_inherited_paths_from_workspace_root() {
    let root = temp_root("deps-workspace-inherited-path");
    fs::create_dir_all(root.join("apps/cli")).expect("create app crate");
    fs::create_dir_all(root.join("crates/nested/helper")).expect("create helper crate");
    fs::write(
        root.join("Cargo.toml"),
        r#"
            [workspace]
            members = ["apps/cli"]

            [workspace.dependencies]
            helper = { path = "crates/nested/helper" }
        "#,
    )
    .expect("write workspace manifest");
    fs::write(
        root.join("apps/cli/Cargo.toml"),
        r#"
            [package]
            name = "cli"
            version = "0.1.0"

            [dependencies]
            helper.workspace = true
        "#,
    )
    .expect("write app manifest");
    fs::write(
        root.join("crates/nested/helper/Cargo.toml"),
        r#"
            [package]
            name = "helper"
            version = "0.1.0"

            [dependencies]
            serde = "1"
        "#,
    )
    .expect("write helper manifest");

    let metadata = read_manifest_metadata(&root.join("Cargo.toml")).expect("read metadata");
    assert_eq!(metadata.packages.len(), 2);
    let inventory =
        collect_dependency_inventory(&metadata, false, false, false).expect("collect deps");
    assert_eq!(
        inventory
            .specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>(),
        vec!["serde"]
    );
    assert_eq!(
        inventory
            .skipped_local
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>(),
        vec!["helper"]
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_discovery_supports_recursive_member_globs() {
    let root = temp_root("deps-workspace-recursive-glob");
    fs::create_dir_all(root.join("crates/group/member")).expect("create nested member");
    fs::write(
        root.join("Cargo.toml"),
        r#"
            [workspace]
            members = ["crates/**"]
        "#,
    )
    .expect("write workspace manifest");
    fs::write(
        root.join("crates/group/member/Cargo.toml"),
        r#"
            [package]
            name = "member"
            version = "0.1.0"

            [dependencies]
            anyhow = "1"
        "#,
    )
    .expect("write member manifest");

    let metadata = read_manifest_metadata(&root.join("Cargo.toml")).expect("read metadata");
    assert_eq!(metadata.packages.len(), 1);
    let inventory =
        collect_dependency_inventory(&metadata, false, false, false).expect("collect deps");
    assert_eq!(inventory.specs[0].name, "anyhow");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn empty_audit_writes_requested_json_report() {
    let root = temp_root("deps-empty-json");
    fs::write(
        root.join("Cargo.toml"),
        r#"
            [package]
            name = "empty"
            version = "0.1.0"
        "#,
    )
    .expect("write manifest");
    let report = root.join("deps.json");

    run(DepsRunOptions {
        manifest_path: Some(root.join("Cargo.toml")),
        project_path: None,
        jobs: Some(1),
        include_dev: false,
        include_build: false,
        include_optional: false,
        refresh: false,
        json_out: Some(report.clone()),
        fail_on_high: false,
        verbose: false,
    })
    .expect("run empty audit");

    let output = fs::read_to_string(&report).expect("read report");
    let json: serde_json::Value = serde_json::from_str(&output).expect("parse report");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["data"]["dependencies"], serde_json::json!([]));
    assert_eq!(json["data"]["actions"], serde_json::Value::Null);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn render_report_lines_default_focuses_on_attention() {
    let manifest = Path::new("/tmp/work/Cargo.toml");
    let summary = AuditSummary {
        high: 1,
        medium: 0,
        low: 1,
        unknown: 1,
        skipped_local: 0,
    };
    let records = vec![
        sample_record(
            "openssl-probe",
            RiskLevel::High,
            &["latest published crate version is yanked"],
        ),
        sample_record(
            "mystery-crate",
            RiskLevel::Unknown,
            &["GitHub signals unavailable (set GITHUB_TOKEN for stable quota)"],
        ),
        sample_record(
            "bytes",
            RiskLevel::Low,
            &["small community size (stars=120)"],
        ),
    ];

    let lines = render_report_lines(manifest, &summary, &records, &[], false);
    let output = lines.join("\n");
    assert!(output.contains("HIGH   deps     Cargo.toml  3 deps  1 high · 1 unknown · 1 baseline"));
    assert!(output.contains("\nattention\n"));
    assert!(output.contains("openssl-probe"));
    assert!(output.contains("mystery-crate"));
    assert!(!output.contains("\ndeps baseline\n"));
    assert!(output.contains("hidden  1 baseline dep; use `--verbose` to show all"));
    assert!(!output.contains("plan"));
    assert!(!output.contains("\nLOW"));
}

#[test]
fn render_report_lines_default_surfaces_low_risk_version_updates() {
    let manifest = Path::new("/tmp/work/Cargo.toml");
    let summary = AuditSummary {
        high: 0,
        medium: 0,
        low: 2,
        unknown: 0,
        skipped_local: 0,
    };
    let mut bumped = sample_record("reqx", RiskLevel::Low, &[]);
    bumped.latest_version = Some("0.1.31".to_string());
    bumped.update_plan = Some(DependencyUpdatePlan::Bump);
    bumped.suggested_requirement = Some("0.1.31".to_string());
    bumped.update_note = Some("same release line; refresh manifest requirement".to_string());
    let records = vec![
        bumped,
        sample_record(
            "bytes",
            RiskLevel::Low,
            &["small community size (stars=120)"],
        ),
    ];

    let lines = render_report_lines(manifest, &summary, &records, &[], false);
    let output = lines.join("\n");
    assert!(output.contains("OK     deps     Cargo.toml  2 deps  1 update · 1 baseline"));
    assert!(output.contains("\nupdates\n"));
    assert!(output.contains("reqx"));
    assert!(output.contains("0.1.31"));
    assert!(output.contains("same-line"));
    assert!(!output.contains("same release line; refresh manifest requirement"));
    assert!(!output.contains("bytes"));
    assert!(!output.contains("\nattention\n"));
    assert!(output.contains("hidden  1 baseline dep; use `--verbose` to show all"));
}

#[test]
fn render_report_lines_verbose_includes_baseline_and_manifest() {
    let manifest = Path::new("/tmp/work/Cargo.toml");
    let summary = AuditSummary {
        high: 0,
        medium: 1,
        low: 1,
        unknown: 0,
        skipped_local: 0,
    };
    let records = vec![
        sample_record(
            "reqwest",
            RiskLevel::Medium,
            &["crate release not recent (800 days)"],
        ),
        sample_record(
            "bytes",
            RiskLevel::Low,
            &["small community size (stars=120)"],
        ),
    ];

    let lines = render_report_lines(manifest, &summary, &records, &[], true);
    let output = lines.join("\n");
    assert!(output.contains("MED    deps     Cargo.toml  2 deps  1 medium · 1 baseline"));
    assert!(output.contains("\nattention\n"));
    assert!(output.contains("\ndeps baseline\n"));
    assert!(output.contains("manifest  /tmp/work/Cargo.toml"));
    assert!(output.contains("reqwest"));
    assert!(output.contains("bytes"));
}

#[test]
fn render_report_lines_summarizes_skipped_internal_dependencies() {
    let manifest = Path::new("/tmp/work/Cargo.toml");
    let summary = AuditSummary {
        high: 0,
        medium: 0,
        low: 1,
        unknown: 0,
        skipped_local: 2,
    };
    let records = vec![sample_record("bytes", RiskLevel::Low, &[])];

    let lines = render_report_lines(manifest, &summary, &records, &[], false);
    let output = lines.join("\n");

    assert!(output.contains("OK     deps     Cargo.toml  1 deps  1 baseline · 2 internal skipped"));
}

#[test]
fn render_report_lines_surfaces_workflow_action_updates() {
    let manifest = Path::new("/tmp/work/Cargo.toml");
    let summary = AuditSummary {
        high: 0,
        medium: 0,
        low: 1,
        unknown: 0,
        skipped_local: 0,
    };
    let records = vec![sample_record("bytes", RiskLevel::Low, &[])];
    let actions = vec![
        sample_action_record(
            "actions/checkout",
            "v4",
            Some("v6"),
            ActionUpdatePlan::Bump,
            "newer action tag available",
        ),
        sample_action_record(
            "actions/cache",
            "v5.0.5",
            Some("v5.0.5"),
            ActionUpdatePlan::Keep,
            "current ref is up to date",
        ),
    ];

    let lines = render_report_lines(manifest, &summary, &records, &actions, false);
    let output = lines.join("\n");

    assert!(output.contains("OK     actions  workflows  2 actions  1 update · 1 baseline"));
    assert!(output.contains("\nupdates\n"));
    assert!(output.contains("actions/checkout"));
    assert!(output.contains("newer-tag"));
    assert!(
        output.contains("hidden  1 baseline dep, 1 baseline action; use `--verbose` to show all")
    );
    assert!(!output.contains("actions/cache"));
}

#[test]
fn render_report_lines_reviews_floating_action_refs_without_fake_latest() {
    let manifest = Path::new("/tmp/work/Cargo.toml");
    let summary = AuditSummary {
        high: 0,
        medium: 0,
        low: 0,
        unknown: 0,
        skipped_local: 0,
    };
    let actions = vec![sample_action_record(
        "dtolnay/rust-toolchain",
        "stable",
        Some("v1"),
        ActionUpdatePlan::Review,
        "floating or non-semver ref; review manually",
    )];

    let lines = render_report_lines(manifest, &summary, &[], &actions, false);
    let output = lines.join("\n");

    assert!(output.contains("WARN   actions  workflows  1 actions  1 review"));
    assert!(output.contains("\naction review\n"));
    assert!(output.contains("dtolnay/rust-toolchain"));
    assert!(output.contains("stable"));
    assert!(output.contains("floating-ref"));
    assert!(!output.contains("v1"));
}

#[test]
fn workflow_action_version_selection_uses_highest_stable_tag() {
    let tags = vec![
        action_tag("v4", "4444444444444444444444444444444444444444"),
        action_tag("v6", "6666666666666666666666666666666666666666"),
        action_tag("v5.1.0", "5555555555555555555555555555555555555555"),
    ];

    assert_eq!(
        latest_stable_action_tag(&tags).map(|tag| tag.tag),
        Some("v6".to_string())
    );
    assert_eq!(
        parse_action_tag_version("v4").map(|tag| tag.version.to_string()),
        Some("4.0.0".to_string())
    );
}

#[test]
fn build_action_audit_record_classifies_version_refs() {
    let spec = sample_workflow_action_spec("actions/checkout", "v4");
    let record = build_action_audit_record(
        spec,
        Ok(vec![
            action_tag("v4", "4444444444444444444444444444444444444444"),
            action_tag("v6", "6666666666666666666666666666666666666666"),
        ]),
    );

    assert_eq!(record.latest_ref.as_deref(), Some("v6"));
    assert_eq!(record.update_plan, ActionUpdatePlan::Bump);
    assert_eq!(record.note.as_deref(), Some("newer action tag available"));
}

#[test]
fn build_action_audit_record_keeps_major_refs_on_same_major() {
    let spec = sample_workflow_action_spec("actions/checkout", "v6");
    let record = build_action_audit_record(
        spec,
        Ok(vec![
            action_tag("v6", "6666666666666666666666666666666666666666"),
            action_tag("v6.0.2", "6666666666666666666666666666666666666666"),
        ]),
    );

    assert_eq!(record.latest_ref.as_deref(), Some("v6.0.2"));
    assert_eq!(record.update_plan, ActionUpdatePlan::Keep);
    assert_eq!(record.note.as_deref(), Some("current ref is up to date"));
}

#[test]
fn build_action_audit_record_updates_outdated_sha_pins() {
    let spec = sample_workflow_action_spec(
        "actions/checkout",
        "0123456789abcdef0123456789abcdef01234567",
    );
    let record = build_action_audit_record(
        spec,
        Ok(vec![
            action_tag("v4.2.2", "0123456789abcdef0123456789abcdef01234567"),
            action_tag("v6.0.2", "abcdef0123456789abcdef0123456789abcdef01"),
        ]),
    );

    assert_eq!(record.latest_ref.as_deref(), Some("v6.0.2"));
    assert_eq!(
        record.latest_sha.as_deref(),
        Some("abcdef0123456789abcdef0123456789abcdef01")
    );
    assert_eq!(record.update_plan, ActionUpdatePlan::Bump);
    assert_eq!(record.note.as_deref(), Some("newer action tag available"));
}

#[test]
fn build_action_audit_record_reviews_sha_with_ambiguous_version_hint() {
    let mut spec = sample_workflow_action_spec(
        "actions/checkout",
        "0123456789abcdef0123456789abcdef01234567",
    );
    spec.version_hint = Some("v6".to_string());

    let record = build_action_audit_record(
        spec,
        Ok(vec![action_tag(
            "v6.0.2",
            "abcdef0123456789abcdef0123456789abcdef01",
        )]),
    );

    assert_eq!(record.update_plan, ActionUpdatePlan::Review);
    assert_eq!(
        record.note.as_deref(),
        Some("sha pin needs an exact version hint; review manually")
    );
}

#[test]
fn build_action_audit_record_reviews_mismatched_sha_hint() {
    let mut spec = sample_workflow_action_spec(
        "actions/checkout",
        "0123456789abcdef0123456789abcdef01234567",
    );
    spec.version_hint = Some("v6.0.2".to_string());

    let record = build_action_audit_record(
        spec,
        Ok(vec![action_tag(
            "v6.0.2",
            "abcdef0123456789abcdef0123456789abcdef01",
        )]),
    );

    assert_eq!(record.update_plan, ActionUpdatePlan::Review);
    assert_eq!(
        record.note.as_deref(),
        Some("sha pin does not match the hinted release tag")
    );
}

#[test]
fn parse_workflow_uses_line_reads_quoted_remote_actions() {
    let spec = parse_workflow_uses_line("  - uses: 'actions/checkout@v6' # pinned major")
        .expect("must parse action");

    assert_eq!(spec.action, "actions/checkout");
    assert_eq!(spec.owner, "actions");
    assert_eq!(spec.repo, "checkout");
    assert_eq!(spec.ref_name, "v6");
}

#[test]
fn parse_workflow_uses_line_reads_sha_version_hint() {
    let spec = parse_workflow_uses_line(
        "- uses: actions/checkout@0123456789abcdef0123456789abcdef01234567 # v4.2.2",
    )
    .expect("must parse action");

    assert_eq!(spec.version_hint.as_deref(), Some("v4.2.2"));
}

#[test]
fn render_latest_lines_show_summary_and_failure_note() {
    let summary = LatestSummary {
        total: 3,
        resolved: 1,
        review: 1,
        failed: 1,
    };
    let records = vec![
        LatestRecord {
            name: "serde".to_string(),
            dependency_source: DependencySource::CratesIo,
            requirement: Some("^1".to_string()),
            kinds: Some("normal".to_string()),
            source: LatestQuerySource::Manifest,
            status: LatestStatus::Resolved,
            latest_version: Some("1.0.228".to_string()),
            project_rust_version: Some("1.70".to_string()),
            msrv_compatible: Some(true),
            suggestion_kind: None,
            suggested_requirement: None,
            note: None,
            suggestion_note: None,
        },
        LatestRecord {
            name: "internal-sdk".to_string(),
            dependency_source: DependencySource::Registry("company".to_string()),
            requirement: Some("1".to_string()),
            kinds: Some("normal".to_string()),
            source: LatestQuerySource::Manifest,
            status: LatestStatus::Review,
            latest_version: None,
            project_rust_version: None,
            msrv_compatible: None,
            suggestion_kind: Some(LatestSuggestionKind::Review),
            suggested_requirement: None,
            note: Some("registry dependency requires source-specific resolution".to_string()),
            suggestion_note: None,
        },
        LatestRecord {
            name: "mystery".to_string(),
            dependency_source: DependencySource::CratesIo,
            requirement: None,
            kinds: None,
            source: LatestQuerySource::Args,
            status: LatestStatus::Failed,
            latest_version: None,
            project_rust_version: None,
            msrv_compatible: None,
            suggestion_kind: None,
            suggested_requirement: None,
            note: Some("crates.io query failed: timeout".to_string()),
            suggestion_note: None,
        },
    ];

    let lines = render_latest_lines(
        Some(Path::new("/tmp/work/Cargo.toml")),
        &summary,
        &records,
        false,
    );
    let output = lines.join("\n");
    assert!(output.contains("latest"));
    assert!(output.contains("1 resolved"));
    assert!(output.contains("1 review"));
    assert!(output.contains("1 failed"));
    assert!(output.contains("serde"));
    assert!(output.contains("1.0.228"));
    assert!(output.contains("mystery"));
    assert!(output.contains("timeout"));
    assert!(output.contains("internal-sdk"));
    assert!(output.contains("source-specific"));
    assert!(output.contains("manifest  /tmp/work/Cargo.toml"));
}

#[test]
fn render_empty_latest_rejects_missing_manifest_source() {
    let err = render_empty_latest(None, false, false).unwrap_err();
    assert!(
        err.to_string()
            .contains("provide crate names or `--manifest-path <Cargo.toml>` or `--path <DIR>`")
    );
}

#[test]
fn render_latest_toml_comments_failed_entries() {
    let rendered = render_latest_toml(&[
        LatestRecord {
            name: "serde".to_string(),
            dependency_source: DependencySource::CratesIo,
            requirement: None,
            kinds: None,
            source: LatestQuerySource::Args,
            status: LatestStatus::Resolved,
            latest_version: Some("1.0.228".to_string()),
            project_rust_version: None,
            msrv_compatible: None,
            suggestion_kind: None,
            suggested_requirement: None,
            note: None,
            suggestion_note: None,
        },
        LatestRecord {
            name: "broken".to_string(),
            dependency_source: DependencySource::CratesIo,
            requirement: None,
            kinds: None,
            source: LatestQuerySource::Args,
            status: LatestStatus::Failed,
            latest_version: None,
            project_rust_version: None,
            msrv_compatible: None,
            suggestion_kind: None,
            suggested_requirement: None,
            note: Some("crates.io query failed: eof".to_string()),
            suggestion_note: None,
        },
    ]);

    assert!(rendered.contains("serde = \"1.0.228\""));
    assert!(rendered.contains("# broken: crates.io query failed: eof"));
}

#[test]
fn render_latest_lines_suggest_mode_surfaces_plan_and_suggestion() {
    let summary = LatestSummary {
        total: 2,
        resolved: 2,
        review: 0,
        failed: 0,
    };
    let records = vec![
        LatestRecord {
            name: "serde".to_string(),
            dependency_source: DependencySource::CratesIo,
            requirement: Some("^1".to_string()),
            kinds: Some("normal".to_string()),
            source: LatestQuerySource::Manifest,
            status: LatestStatus::Resolved,
            latest_version: Some("1.0.228".to_string()),
            project_rust_version: Some("1.70".to_string()),
            msrv_compatible: Some(true),
            suggestion_kind: Some(LatestSuggestionKind::Keep),
            suggested_requirement: None,
            note: None,
            suggestion_note: Some("current requirement already accepts latest".to_string()),
        },
        LatestRecord {
            name: "reqx".to_string(),
            dependency_source: DependencySource::CratesIo,
            requirement: Some("0.1.29".to_string()),
            kinds: Some("normal".to_string()),
            source: LatestQuerySource::Manifest,
            status: LatestStatus::Resolved,
            latest_version: Some("0.1.31".to_string()),
            project_rust_version: Some("1.70".to_string()),
            msrv_compatible: Some(true),
            suggestion_kind: Some(LatestSuggestionKind::Bump),
            suggested_requirement: Some("0.1.31".to_string()),
            note: None,
            suggestion_note: Some("same release line; refresh manifest requirement".to_string()),
        },
    ];

    let lines = render_latest_lines(
        Some(Path::new("/tmp/work/Cargo.toml")),
        &summary,
        &records,
        true,
    );
    let output = lines.join("\n");
    assert!(output.contains("plan"));
    assert!(output.contains("suggest"));
    assert!(output.contains("keep"));
    assert!(output.contains("bump"));
    assert!(output.contains("0.1.31"));
    assert!(output.contains("current requirement already accepts latest"));
}

fn sample_record(name: &str, risk: RiskLevel, notes: &[&str]) -> DepAuditRecord {
    DepAuditRecord {
        name: name.to_string(),
        source: DependencySource::CratesIo,
        requirement: "^1".to_string(),
        kinds: "normal".to_string(),
        project_rust_version: Some("1.70".to_string()),
        optional: false,
        latest_version: Some("1.0.0".to_string()),
        update_plan: Some(DependencyUpdatePlan::Keep),
        suggested_requirement: None,
        update_note: Some("current requirement already accepts latest".to_string()),
        latest_version_license: Some("MIT".to_string()),
        latest_version_rust_version: Some("1.70".to_string()),
        msrv_compatible: Some(true),
        latest_version_yanked: Some(false),
        crate_updated_at: Some("2026-03-01T00:00:00Z".to_string()),
        latest_release_at: Some("2026-03-01T00:00:00Z".to_string()),
        latest_release_age_days: Some(30),
        repository: Some("https://github.com/example/example".to_string()),
        github_stars: Some(120),
        github_archived: Some(false),
        github_pushed_at: Some("2026-03-01T00:00:00Z".to_string()),
        github_push_age_days: Some(30),
        std_alternative: None,
        risk,
        notes: notes.iter().map(|note| (*note).to_string()).collect(),
    }
}

fn sample_action_record(
    action: &str,
    current_ref: &str,
    latest_ref: Option<&str>,
    update_plan: ActionUpdatePlan,
    note: &str,
) -> ActionAuditRecord {
    let mut parts = action.split('/');
    let owner = parts.next().unwrap_or_default().to_string();
    let repo = parts.next().unwrap_or_default().to_string();
    let rest = parts.collect::<Vec<_>>();
    ActionAuditRecord {
        action: action.to_string(),
        owner,
        repo,
        path: (!rest.is_empty()).then(|| rest.join("/")),
        current_ref: current_ref.to_string(),
        latest_ref: latest_ref.map(ToOwned::to_owned),
        latest_sha: None,
        update_plan,
        note: Some(note.to_string()),
        locations: vec![ActionLocation {
            file: ".github/workflows/ci.yaml".to_string(),
            line: 12,
        }],
    }
}

fn sample_workflow_action_spec(action: &str, ref_name: &str) -> WorkflowActionSpec {
    let mut parts = action.split('/');
    let owner = parts.next().unwrap_or_default().to_string();
    let repo = parts.next().unwrap_or_default().to_string();
    let rest = parts.collect::<Vec<_>>();
    WorkflowActionSpec {
        action: action.to_string(),
        owner,
        repo,
        path: (!rest.is_empty()).then(|| rest.join("/")),
        ref_name: ref_name.to_string(),
        version_hint: None,
        locations: vec![ActionLocation {
            file: ".github/workflows/ci.yaml".to_string(),
            line: 12,
        }],
    }
}

fn action_tag(name: &str, sha: &str) -> GitHubTagSnapshot {
    GitHubTagSnapshot {
        name: name.to_string(),
        sha: sha.to_string(),
    }
}

fn sample_metadata() -> WorkspaceManifest {
    WorkspaceManifest {
        packages: vec![WorkspacePackage {
            rust_version: Some("1.70".to_string()),
            dependencies: vec![
                workspace_dependency("bytes", DependencySource::CratesIo, "^1", false, true),
                workspace_dependency(
                    "futures-core",
                    DependencySource::CratesIo,
                    "^0.3",
                    true,
                    true,
                ),
                workspace_dependency("hyper", DependencySource::CratesIo, "^1", true, false),
                WorkspaceDependency {
                    kind: Some("dev".to_string()),
                    ..workspace_dependency(
                        "criterion",
                        DependencySource::CratesIo,
                        "^0.5",
                        false,
                        true,
                    )
                },
            ],
        }],
        workspace_root: "/tmp/work".into(),
    }
}

fn workspace_metadata() -> WorkspaceManifest {
    WorkspaceManifest {
        packages: vec![
            WorkspacePackage {
                rust_version: Some("1.75".to_string()),
                dependencies: vec![
                    workspace_dependency("serde", DependencySource::CratesIo, "^1", false, true),
                    workspace_dependency(
                        "workspace-core",
                        DependencySource::Path("crates/workspace-core".to_string()),
                        "^0.1",
                        false,
                        true,
                    ),
                    workspace_dependency(
                        "local-helper",
                        DependencySource::Path("crates/local-helper".to_string()),
                        "^0.1",
                        false,
                        true,
                    ),
                ],
            },
            WorkspacePackage {
                rust_version: Some("1.80".to_string()),
                dependencies: vec![workspace_dependency(
                    "tokio",
                    DependencySource::CratesIo,
                    "^1",
                    false,
                    true,
                )],
            },
        ],
        workspace_root: "/tmp/work".into(),
    }
}

fn workspace_dependency(
    name: &str,
    source: DependencySource,
    requirement: &str,
    optional: bool,
    enabled_by_default: bool,
) -> WorkspaceDependency {
    WorkspaceDependency {
        name: name.to_string(),
        source,
        req: requirement.to_string(),
        kind: None,
        optional,
        enabled_by_default,
    }
}

fn temp_root(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "za-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("create temp root");
    root
}
