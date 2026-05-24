use super::latest::{
    LatestQuerySource, LatestRecord, LatestStatus, LatestSuggestionKind, LatestSummary,
    render_empty_latest,
};
use super::render::{render_latest_lines, render_latest_toml, render_report_lines};
use super::{
    ActionAuditRecord, ActionLocation, ActionUpdatePlan, AuditSummary, CargoDependency,
    CargoMetadata, CargoPackage, CargoResolve, CargoResolveDepKind, CargoResolveNode,
    CargoResolveNodeDep, DepAuditRecord, DependencyUpdatePlan, RiskLevel, WorkflowActionSpec,
    build_action_audit_record, collect_dependency_inventory, collect_dependency_specs,
    derive_auto_jobs, latest_stable_action_tag, parse_action_tag_version, parse_workflow_uses_line,
};
use std::path::Path;

#[test]
fn auto_jobs_is_bounded() {
    assert_eq!(derive_auto_jobs(1), 4);
    assert_eq!(derive_auto_jobs(2), 4);
    assert_eq!(derive_auto_jobs(4), 8);
    assert_eq!(derive_auto_jobs(8), 16);
    assert_eq!(derive_auto_jobs(32), 16);
}

#[test]
fn collect_dependency_specs_uses_resolved_active_direct_dependencies() {
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
fn collect_dependency_specs_discards_partial_resolve_before_declared_fallback() {
    let mut metadata = workspace_metadata();
    metadata.packages[0].dependencies.push(CargoDependency {
        name: "hyper".to_string(),
        source: Some(registry_source()),
        req: "^1".to_string(),
        kind: None,
        optional: true,
    });
    metadata.packages.push(CargoPackage {
        id: "pkg-hyper".to_string(),
        name: "hyper".to_string(),
        source: Some(registry_source()),
        dependencies: Vec::new(),
    });
    let resolve = metadata.resolve.as_mut().unwrap();
    resolve.nodes[0].deps.push(CargoResolveNodeDep {
        pkg: "pkg-hyper".to_string(),
        dep_kinds: vec![CargoResolveDepKind { kind: None }],
    });
    resolve.nodes.retain(|node| node.id != "pkg-workspace-core");

    let specs = collect_dependency_specs(&metadata, false, false, false).unwrap();
    let names = specs
        .iter()
        .map(|spec| spec.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(names, vec!["serde", "tokio"]);
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
    assert!(output.contains("HIGH   Cargo.toml  3 deps  1 high · 1 unknown · 1 low"));
    assert!(output.contains("\nattention\n"));
    assert!(output.contains("openssl-probe"));
    assert!(output.contains("mystery-crate"));
    assert!(!output.contains("\nbaseline\n"));
    assert!(output.contains("1 baseline entry hidden; use `--verbose` to show all"));
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
    assert!(output.contains("OK     Cargo.toml  2 deps  2 low · 1 update"));
    assert!(output.contains("\nattention\n"));
    assert!(output.contains("reqx"));
    assert!(output.contains("bump"));
    assert!(output.contains("same-line"));
    assert!(!output.contains("same release line; refresh manifest requirement"));
    assert!(!output.contains("bytes"));
    assert!(output.contains("1 baseline entry hidden; use `--verbose` to show all"));
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
    assert!(output.contains("MED    Cargo.toml  2 deps  1 medium · 1 low"));
    assert!(output.contains("\nattention\n"));
    assert!(output.contains("\nbaseline\n"));
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

    assert!(output.contains("OK     Cargo.toml  1 deps  1 low · 2 internal skipped"));
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
            "Swatinem/rust-cache",
            "v2",
            Some("v2"),
            ActionUpdatePlan::Keep,
            "current ref is up to date",
        ),
    ];

    let lines = render_report_lines(manifest, &summary, &records, &actions, false);
    let output = lines.join("\n");

    assert!(output.contains("1 action update"));
    assert!(output.contains("\nactions\n"));
    assert!(output.contains("actions/checkout"));
    assert!(output.contains("newer-tag"));
    assert!(output.contains("1 action entry hidden; use `--verbose` to show all"));
    assert!(!output.contains("Swatinem/rust-cache"));
}

#[test]
fn workflow_action_version_selection_uses_highest_stable_tag() {
    let tags = vec![
        "v4".to_string(),
        "v6".to_string(),
        "v5.1.0".to_string(),
        "v7.0.0-beta.1".to_string(),
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
    let record = build_action_audit_record(spec, Ok(vec!["v4".to_string(), "v6".to_string()]));

    assert_eq!(record.latest_ref.as_deref(), Some("v6"));
    assert_eq!(record.update_plan, ActionUpdatePlan::Bump);
    assert_eq!(record.note.as_deref(), Some("newer action tag available"));
}

#[test]
fn build_action_audit_record_keeps_major_refs_on_same_major() {
    let spec = sample_workflow_action_spec("actions/checkout", "v6");
    let record = build_action_audit_record(spec, Ok(vec!["v6".to_string(), "v6.0.2".to_string()]));

    assert_eq!(record.latest_ref.as_deref(), Some("v6.0.2"));
    assert_eq!(record.update_plan, ActionUpdatePlan::Keep);
    assert_eq!(record.note.as_deref(), Some("current ref is up to date"));
}

#[test]
fn build_action_audit_record_keeps_sha_pinned_refs() {
    let spec = sample_workflow_action_spec(
        "actions/checkout",
        "0123456789abcdef0123456789abcdef01234567",
    );
    let record = build_action_audit_record(spec, Ok(vec!["v6".to_string()]));

    assert_eq!(record.latest_ref, None);
    assert_eq!(record.update_plan, ActionUpdatePlan::Keep);
    assert_eq!(record.note.as_deref(), Some("sha-pinned"));
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
fn render_latest_lines_show_summary_and_failure_note() {
    let summary = LatestSummary {
        total: 2,
        resolved: 1,
        failed: 1,
    };
    let records = vec![
        LatestRecord {
            name: "serde".to_string(),
            requirement: Some("^1".to_string()),
            kinds: Some("normal".to_string()),
            source: LatestQuerySource::Manifest,
            status: LatestStatus::Resolved,
            latest_version: Some("1.0.228".to_string()),
            suggestion_kind: None,
            suggested_requirement: None,
            note: None,
            suggestion_note: None,
        },
        LatestRecord {
            name: "mystery".to_string(),
            requirement: None,
            kinds: None,
            source: LatestQuerySource::Args,
            status: LatestStatus::Failed,
            latest_version: None,
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
    assert!(output.contains("1 failed"));
    assert!(output.contains("serde"));
    assert!(output.contains("1.0.228"));
    assert!(output.contains("mystery"));
    assert!(output.contains("timeout"));
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
            requirement: None,
            kinds: None,
            source: LatestQuerySource::Args,
            status: LatestStatus::Resolved,
            latest_version: Some("1.0.228".to_string()),
            suggestion_kind: None,
            suggested_requirement: None,
            note: None,
            suggestion_note: None,
        },
        LatestRecord {
            name: "broken".to_string(),
            requirement: None,
            kinds: None,
            source: LatestQuerySource::Args,
            status: LatestStatus::Failed,
            latest_version: None,
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
        failed: 0,
    };
    let records = vec![
        LatestRecord {
            name: "serde".to_string(),
            requirement: Some("^1".to_string()),
            kinds: Some("normal".to_string()),
            source: LatestQuerySource::Manifest,
            status: LatestStatus::Resolved,
            latest_version: Some("1.0.228".to_string()),
            suggestion_kind: Some(LatestSuggestionKind::Keep),
            suggested_requirement: None,
            note: None,
            suggestion_note: Some("current requirement already accepts latest".to_string()),
        },
        LatestRecord {
            name: "reqx".to_string(),
            requirement: Some("0.1.29".to_string()),
            kinds: Some("normal".to_string()),
            source: LatestQuerySource::Manifest,
            status: LatestStatus::Resolved,
            latest_version: Some("0.1.31".to_string()),
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
        requirement: "^1".to_string(),
        kinds: "normal".to_string(),
        optional: false,
        latest_version: Some("1.0.0".to_string()),
        update_plan: Some(DependencyUpdatePlan::Keep),
        suggested_requirement: None,
        update_note: Some("current requirement already accepts latest".to_string()),
        latest_version_license: Some("MIT".to_string()),
        latest_version_rust_version: Some("1.70".to_string()),
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
        locations: vec![ActionLocation {
            file: ".github/workflows/ci.yaml".to_string(),
            line: 12,
        }],
    }
}

fn sample_metadata() -> CargoMetadata {
    CargoMetadata {
        packages: vec![
            CargoPackage {
                id: "pkg-root".to_string(),
                name: "sample".to_string(),
                source: None,
                dependencies: vec![
                    CargoDependency {
                        name: "bytes".to_string(),
                        source: Some(registry_source()),
                        req: "^1".to_string(),
                        kind: None,
                        optional: false,
                    },
                    CargoDependency {
                        name: "futures-core".to_string(),
                        source: Some(registry_source()),
                        req: "^0.3".to_string(),
                        kind: None,
                        optional: true,
                    },
                    CargoDependency {
                        name: "hyper".to_string(),
                        source: Some(registry_source()),
                        req: "^1".to_string(),
                        kind: None,
                        optional: true,
                    },
                    CargoDependency {
                        name: "criterion".to_string(),
                        source: Some(registry_source()),
                        req: "^0.5".to_string(),
                        kind: Some("dev".to_string()),
                        optional: false,
                    },
                ],
            },
            CargoPackage {
                id: "pkg-bytes".to_string(),
                name: "bytes".to_string(),
                source: Some(registry_source()),
                dependencies: Vec::new(),
            },
            CargoPackage {
                id: "pkg-futures-core".to_string(),
                name: "futures-core".to_string(),
                source: Some(registry_source()),
                dependencies: Vec::new(),
            },
            CargoPackage {
                id: "pkg-hyper".to_string(),
                name: "hyper".to_string(),
                source: Some(registry_source()),
                dependencies: Vec::new(),
            },
            CargoPackage {
                id: "pkg-criterion".to_string(),
                name: "criterion".to_string(),
                source: Some(registry_source()),
                dependencies: Vec::new(),
            },
        ],
        workspace_members: vec!["pkg-root".to_string()],
        root: None,
        workspace_root: Some("/tmp/work".into()),
        resolve: Some(CargoResolve {
            nodes: vec![CargoResolveNode {
                id: "pkg-root".to_string(),
                deps: vec![
                    CargoResolveNodeDep {
                        pkg: "pkg-bytes".to_string(),
                        dep_kinds: vec![CargoResolveDepKind { kind: None }],
                    },
                    CargoResolveNodeDep {
                        pkg: "pkg-futures-core".to_string(),
                        dep_kinds: vec![CargoResolveDepKind { kind: None }],
                    },
                    CargoResolveNodeDep {
                        pkg: "pkg-criterion".to_string(),
                        dep_kinds: vec![CargoResolveDepKind {
                            kind: Some("dev".to_string()),
                        }],
                    },
                ],
            }],
        }),
    }
}

fn workspace_metadata() -> CargoMetadata {
    CargoMetadata {
        packages: vec![
            CargoPackage {
                id: "pkg-app".to_string(),
                name: "app".to_string(),
                source: None,
                dependencies: vec![
                    CargoDependency {
                        name: "serde".to_string(),
                        source: Some(registry_source()),
                        req: "^1".to_string(),
                        kind: None,
                        optional: false,
                    },
                    CargoDependency {
                        name: "workspace-core".to_string(),
                        source: None,
                        req: "^0.1".to_string(),
                        kind: None,
                        optional: false,
                    },
                    CargoDependency {
                        name: "local-helper".to_string(),
                        source: None,
                        req: "^0.1".to_string(),
                        kind: None,
                        optional: false,
                    },
                ],
            },
            CargoPackage {
                id: "pkg-workspace-core".to_string(),
                name: "workspace-core".to_string(),
                source: None,
                dependencies: vec![CargoDependency {
                    name: "tokio".to_string(),
                    source: Some(registry_source()),
                    req: "^1".to_string(),
                    kind: None,
                    optional: false,
                }],
            },
            CargoPackage {
                id: "pkg-local-helper".to_string(),
                name: "local-helper".to_string(),
                source: None,
                dependencies: Vec::new(),
            },
            CargoPackage {
                id: "pkg-serde".to_string(),
                name: "serde".to_string(),
                source: Some(registry_source()),
                dependencies: Vec::new(),
            },
            CargoPackage {
                id: "pkg-tokio".to_string(),
                name: "tokio".to_string(),
                source: Some(registry_source()),
                dependencies: Vec::new(),
            },
        ],
        workspace_members: vec!["pkg-app".to_string(), "pkg-workspace-core".to_string()],
        root: Some("pkg-app".to_string()),
        workspace_root: Some("/tmp/work".into()),
        resolve: Some(CargoResolve {
            nodes: vec![
                CargoResolveNode {
                    id: "pkg-app".to_string(),
                    deps: vec![
                        CargoResolveNodeDep {
                            pkg: "pkg-serde".to_string(),
                            dep_kinds: vec![CargoResolveDepKind { kind: None }],
                        },
                        CargoResolveNodeDep {
                            pkg: "pkg-workspace-core".to_string(),
                            dep_kinds: vec![CargoResolveDepKind { kind: None }],
                        },
                        CargoResolveNodeDep {
                            pkg: "pkg-local-helper".to_string(),
                            dep_kinds: vec![CargoResolveDepKind { kind: None }],
                        },
                    ],
                },
                CargoResolveNode {
                    id: "pkg-workspace-core".to_string(),
                    deps: vec![CargoResolveNodeDep {
                        pkg: "pkg-tokio".to_string(),
                        dep_kinds: vec![CargoResolveDepKind { kind: None }],
                    }],
                },
            ],
        }),
    }
}

fn registry_source() -> String {
    "registry+https://github.com/rust-lang/crates.io-index".to_string()
}
