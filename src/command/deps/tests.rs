use super::{
    CargoDependency, CargoMetadata, CargoPackage, CargoResolve, CargoResolveDepKind,
    CargoResolveNode, CargoResolveNodeDep, collect_dependency_specs, derive_auto_jobs,
};

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
