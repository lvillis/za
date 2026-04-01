use super::{
    AuditSummary, CargoDependency, CargoMetadata, CargoPackage, CargoResolve, CargoResolveDepKind,
    CargoResolveNode, CargoResolveNodeDep, DepAuditRecord, RiskLevel, collect_dependency_specs,
    derive_auto_jobs, render_report_lines,
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
fn render_report_lines_default_focuses_on_attention() {
    let manifest = Path::new("/tmp/work/Cargo.toml");
    let summary = AuditSummary {
        high: 1,
        medium: 0,
        low: 1,
        unknown: 1,
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

    let lines = render_report_lines(manifest, &summary, &records, false);
    let output = lines.join("\n");
    assert!(output.contains("HIGH   Cargo.toml  3 deps  1 high · 1 unknown · 1 low"));
    assert!(output.contains("\nattention\n"));
    assert!(output.contains("openssl-probe"));
    assert!(output.contains("mystery-crate"));
    assert!(!output.contains("\nbaseline\n"));
    assert!(
        output.contains("low-risk entry is hidden")
            || output.contains("low-risk entries are hidden")
    );
    assert!(!output.contains("\nLOW"));
}

#[test]
fn render_report_lines_verbose_includes_baseline_and_manifest() {
    let manifest = Path::new("/tmp/work/Cargo.toml");
    let summary = AuditSummary {
        high: 0,
        medium: 1,
        low: 1,
        unknown: 0,
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

    let lines = render_report_lines(manifest, &summary, &records, true);
    let output = lines.join("\n");
    assert!(output.contains("MED    Cargo.toml  2 deps  1 medium · 1 low"));
    assert!(output.contains("\nattention\n"));
    assert!(output.contains("\nbaseline\n"));
    assert!(output.contains("manifest  /tmp/work/Cargo.toml"));
    assert!(output.contains("reqwest"));
    assert!(output.contains("bytes"));
}

fn sample_record(name: &str, risk: RiskLevel, notes: &[&str]) -> DepAuditRecord {
    DepAuditRecord {
        name: name.to_string(),
        requirement: "^1".to_string(),
        kinds: "normal".to_string(),
        optional: false,
        latest_version: Some("1.0.0".to_string()),
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

fn sample_metadata() -> CargoMetadata {
    CargoMetadata {
        packages: vec![
            CargoPackage {
                id: "pkg-root".to_string(),
                name: "sample".to_string(),
                dependencies: vec![
                    CargoDependency {
                        name: "bytes".to_string(),
                        req: "^1".to_string(),
                        kind: None,
                        optional: false,
                    },
                    CargoDependency {
                        name: "futures-core".to_string(),
                        req: "^0.3".to_string(),
                        kind: None,
                        optional: true,
                    },
                    CargoDependency {
                        name: "hyper".to_string(),
                        req: "^1".to_string(),
                        kind: None,
                        optional: true,
                    },
                    CargoDependency {
                        name: "criterion".to_string(),
                        req: "^0.5".to_string(),
                        kind: Some("dev".to_string()),
                        optional: false,
                    },
                ],
            },
            CargoPackage {
                id: "pkg-bytes".to_string(),
                name: "bytes".to_string(),
                dependencies: Vec::new(),
            },
            CargoPackage {
                id: "pkg-futures-core".to_string(),
                name: "futures-core".to_string(),
                dependencies: Vec::new(),
            },
            CargoPackage {
                id: "pkg-hyper".to_string(),
                name: "hyper".to_string(),
                dependencies: Vec::new(),
            },
            CargoPackage {
                id: "pkg-criterion".to_string(),
                name: "criterion".to_string(),
                dependencies: Vec::new(),
            },
        ],
        workspace_members: vec!["pkg-root".to_string()],
        root: None,
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
