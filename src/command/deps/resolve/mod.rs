//! Resolve dependency references without modifying project files.

mod action;
mod crate_registry;
mod npm;
use super::is_full_commit_sha;
use action::resolve_action;
use crate_registry::resolve_crate;
use npm::resolve_npm;

use crate::{
    cli::DepsResolveCommands,
    command::{
        http::build_client, print_json as print_json_envelope, render as text_render, za_config,
    },
};
use anyhow::{Context, Result, anyhow, bail};
use reqx::{advanced::ClientProfile, blocking::Client};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

const HTTP_TIMEOUT_SECS: u64 = 30;
const HTTP_USER_AGENT: &str = "za-deps-resolve/0.1";
const NPM_REGISTRY_BASE: &str = "https://registry.npmjs.org";
const DEFAULT_NPM_TAG: &str = "latest";

pub fn run(cmd: DepsResolveCommands) -> Result<()> {
    match cmd {
        DepsResolveCommands::Npm {
            package,
            tag,
            json,
            emit,
        } => {
            let query = parse_npm_package_query(&package, tag.as_deref())?;
            let record = resolve_npm(&query)?;
            if json {
                print_json(&record)?;
            } else {
                println!(
                    "{}",
                    if emit.is_some() {
                        record.package_json.clone()
                    } else {
                        format!(
                            "{}@{} -> {}",
                            record.package, record.requested_tag, record.version
                        )
                    }
                );
            }
        }
        DepsResolveCommands::Crate { name, json, emit } => {
            let name = normalize_crate_name(&name)?;
            let record = resolve_crate(&name)?;
            if json {
                print_json(&record)?;
            } else {
                println!(
                    "{}",
                    if emit.is_some() {
                        record.cargo_toml_exact.clone()
                    } else {
                        format!("{}@latest -> {}", record.name, record.version)
                    }
                );
            }
        }
        DepsResolveCommands::Action { spec, json, emit } => {
            let spec = ActionSpec::parse(&spec)?;
            let record = resolve_action(&spec)?;
            if json {
                print_json(&record)?;
            } else {
                println!(
                    "{}",
                    if emit.is_some() {
                        action_yaml(&record)
                    } else {
                        format!("{} -> {}", spec.input(), record.sha)
                    }
                );
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NpmPackageQuery {
    package: String,
    tag: String,
}

#[derive(Debug, Serialize)]
struct NpmResolution {
    kind: &'static str,
    package: String,
    requested_tag: String,
    version: String,
    npm_spec: String,
    package_json: String,
}

#[derive(Debug, Serialize)]
struct CrateResolution {
    kind: &'static str,
    name: String,
    version: String,
    cargo_toml_exact: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ActionSpec {
    pub(super) owner: String,
    pub(super) repo: String,
    pub(super) path: Option<String>,
    pub(super) ref_name: String,
}

#[derive(Debug, Serialize)]
struct ActionResolution {
    kind: &'static str,
    owner: String,
    repo: String,
    path: Option<String>,
    input_ref: String,
    sha: String,
    uses: String,
    source: &'static str,
}

#[derive(Debug, Deserialize)]
struct NpmPackageResponse {
    name: String,
    #[serde(rename = "dist-tags")]
    dist_tags: BTreeMap<String, String>,
    #[serde(default)]
    versions: BTreeMap<String, serde_json::Value>,
}

fn build_npm_record(package: String, requested_tag: String, version: String) -> NpmResolution {
    let npm_spec = format!("{package}@{version}");
    NpmResolution {
        kind: "npm",
        package: package.clone(),
        requested_tag,
        version: version.clone(),
        npm_spec,
        package_json: format!(
            "{}: {}",
            serde_json::to_string(&package).expect("serialize package"),
            serde_json::to_string(&version).expect("serialize version")
        ),
    }
}

fn build_crate_record(name: String, version: String) -> CrateResolution {
    CrateResolution {
        kind: "crate",
        name: name.clone(),
        version: version.clone(),
        cargo_toml_exact: format!("{name} = \"={version}\""),
    }
}

fn build_action_record(spec: &ActionSpec, sha: String, source: &'static str) -> ActionResolution {
    let action = spec.action_without_ref();
    ActionResolution {
        kind: "action",
        owner: spec.owner.clone(),
        repo: spec.repo.clone(),
        path: spec.path.clone(),
        input_ref: spec.ref_name.clone(),
        uses: format!("{action}@{sha}"),
        sha,
        source,
    }
}

fn action_yaml(record: &ActionResolution) -> String {
    // JSON strings are valid YAML scalars, including refs containing YAML punctuation.
    format!(
        "uses: {} # {}",
        serde_json::to_string(&record.uses).expect("serialize string"),
        record.input_ref
    )
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    print_json_envelope(value, "serialize dependency resolution output")
}

fn parse_npm_package_query(input: &str, tag: Option<&str>) -> Result<NpmPackageQuery> {
    let input = input.trim();
    if input.is_empty() {
        bail!("npm package must not be empty");
    }
    let (package, inline_tag) = split_npm_package_and_tag(input)?;
    if inline_tag.is_some() && tag.is_some() {
        bail!("use either PACKAGE@TAG or --tag, not both");
    }
    validate_npm_package_name(&package)?;
    let tag = inline_tag
        .or_else(|| tag.map(str::to_string))
        .unwrap_or_else(|| DEFAULT_NPM_TAG.to_string());
    validate_npm_tag(&tag)?;
    Ok(NpmPackageQuery { package, tag })
}

fn split_npm_package_and_tag(input: &str) -> Result<(String, Option<String>)> {
    if let Some(rest) = input.strip_prefix('@') {
        let slash = rest
            .find('/')
            .ok_or_else(|| anyhow!("scoped npm package must be in @scope/name form"))?;
        let tag_start = slash + 2;
        if let Some(rel_at) = input[tag_start..].rfind('@') {
            let at = tag_start + rel_at;
            let package = input[..at].to_string();
            let tag = input[at + 1..].to_string();
            return Ok((package, Some(tag)));
        }
        return Ok((input.to_string(), None));
    }

    if let Some((package, tag)) = input.rsplit_once('@') {
        return Ok((package.to_string(), Some(tag.to_string())));
    }
    Ok((input.to_string(), None))
}

fn validate_npm_package_name(package: &str) -> Result<()> {
    if package.trim() != package || package.is_empty() {
        bail!("npm package must not be empty or contain surrounding whitespace");
    }
    if package.chars().any(char::is_whitespace) {
        bail!("npm package `{package}` must not contain whitespace");
    }
    if package.starts_with('@') {
        let Some((scope, name)) = package.split_once('/') else {
            bail!("scoped npm package must be in @scope/name form");
        };
        if scope.len() <= 1 || name.is_empty() || name.contains('/') {
            bail!("scoped npm package must be in @scope/name form");
        }
    } else if package.contains('/') {
        bail!("unscoped npm package `{package}` must not contain `/`");
    }
    Ok(())
}

fn validate_npm_tag(tag: &str) -> Result<()> {
    if tag.trim() != tag || tag.is_empty() {
        bail!("npm tag/version must not be empty or contain surrounding whitespace");
    }
    if tag.chars().any(char::is_whitespace) {
        bail!("npm tag/version `{tag}` must not contain whitespace");
    }
    Ok(())
}

fn normalize_crate_name(input: &str) -> Result<String> {
    let name = input.trim();
    if name.is_empty() {
        bail!("crate name must not be empty");
    }
    if name != input {
        bail!("crate name must not contain surrounding whitespace");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("crate name `{name}` contains unsupported characters");
    }
    Ok(name.to_string())
}

impl ActionSpec {
    pub(super) fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("action spec must not be empty");
        }
        if input.chars().any(char::is_whitespace) {
            bail!("action spec `{input}` must not contain whitespace");
        }
        let Some((action, ref_name)) = input.rsplit_once('@') else {
            bail!("action spec must be OWNER/REPO[/PATH]@REF");
        };
        if ref_name.is_empty() {
            bail!("action ref must not be empty");
        }
        let mut parts = action.split('/');
        let owner = parts
            .next()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| anyhow!("action owner must not be empty"))?;
        let repo = parts
            .next()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| anyhow!("action repo must not be empty"))?;
        let rest: Vec<_> = parts.collect();
        if [owner, repo]
            .into_iter()
            .chain(rest.iter().copied())
            .any(|part| super::valid_action_segment(part).is_none() || matches!(part, "." | ".."))
        {
            bail!("action owner, repo and path must contain valid non-empty segments");
        }
        Ok(Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            path: (!rest.is_empty()).then(|| rest.join("/")),
            ref_name: ref_name.to_string(),
        })
    }

    fn input(&self) -> String {
        format!("{}@{}", self.action_without_ref(), self.ref_name)
    }

    pub(super) fn action_without_ref(&self) -> String {
        match self.path.as_deref() {
            Some(path) => format!("{}/{}/{}", self.owner, self.repo, path),
            None => format!("{}/{}", self.owner, self.repo),
        }
    }
}

fn encode_npm_package_for_url(package: &str) -> String {
    package.replace('/', "%2f")
}

fn build_http_client(base_url: &str) -> Result<Client> {
    build_client(
        base_url,
        "za-deps-resolve",
        ClientProfile::StandardSdk,
        false,
        Duration::from_secs(HTTP_TIMEOUT_SECS),
        za_config::ProxyScope::Deps,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::deps::api::percent_encode_path_segment;

    #[test]
    fn action_yaml_preserves_nested_path_and_original_ref() {
        let spec = ActionSpec::parse("github/codeql-action/init@release/v3").unwrap();
        let record = build_action_record(&spec, "a".repeat(40), "github");
        assert_eq!(
            action_yaml(&record),
            format!(
                "uses: \"github/codeql-action/init@{}\" # release/v3",
                "a".repeat(40)
            )
        );
    }

    #[test]
    fn full_sha_resolution_does_not_need_network() {
        let spec = ActionSpec::parse(&format!("actions/checkout@{}", "a".repeat(40))).unwrap();
        let record = resolve_action(&spec).unwrap();
        assert_eq!(record.source, "input");
        assert_eq!(record.sha, "a".repeat(40));
    }

    #[test]
    fn rejects_invalid_action_paths() {
        for input in [
            "actions/checkout/../init@v4",
            "actions//init@v4",
            "actions/checkout@",
            "actions/checkout@two refs",
        ] {
            assert!(ActionSpec::parse(input).is_err(), "{input}");
        }
    }

    #[test]
    fn parses_unscoped_npm_package_with_inline_tag() {
        let query = parse_npm_package_query("react@next", None).expect("must parse");
        assert_eq!(
            query,
            NpmPackageQuery {
                package: "react".to_string(),
                tag: "next".to_string(),
            }
        );
    }

    #[test]
    fn parses_scoped_npm_package_with_inline_tag() {
        let query = parse_npm_package_query("@scope/pkg@beta", None).expect("must parse");
        assert_eq!(query.package, "@scope/pkg");
        assert_eq!(query.tag, "beta");
    }

    #[test]
    fn rejects_ambiguous_npm_tag_sources() {
        let err = parse_npm_package_query("react@next", Some("latest")).unwrap_err();
        assert!(err.to_string().contains("PACKAGE@TAG or --tag"));
    }

    #[test]
    fn encodes_scoped_npm_package_for_registry_url() {
        assert_eq!(encode_npm_package_for_url("@scope/pkg"), "@scope%2fpkg");
    }

    #[test]
    fn parses_action_with_nested_path() {
        let spec = ActionSpec::parse("github/codeql-action/init@v3").expect("must parse");
        assert_eq!(spec.owner, "github");
        assert_eq!(spec.repo, "codeql-action");
        assert_eq!(spec.path.as_deref(), Some("init"));
        assert_eq!(spec.ref_name, "v3");
        assert_eq!(spec.action_without_ref(), "github/codeql-action/init");
    }

    #[test]
    fn percent_encodes_refs_with_slashes() {
        assert_eq!(percent_encode_path_segment("release/v1"), "release%2Fv1");
    }

    #[test]
    fn builds_action_record_with_sha_pin() {
        let spec = ActionSpec::parse("actions/checkout@v4").expect("must parse");
        let record = build_action_record(
            &spec,
            "0123456789abcdef0123456789abcdef01234567".to_string(),
            "github",
        );
        assert_eq!(
            record.uses,
            "actions/checkout@0123456789abcdef0123456789abcdef01234567"
        );
    }

    #[test]
    fn builds_copy_pastable_dependency_records() {
        let npm = build_npm_record(
            "react".to_string(),
            "latest".to_string(),
            "19.2.0".to_string(),
        );
        assert_eq!(npm.npm_spec, "react@19.2.0");
        assert_eq!(npm.package_json, "\"react\": \"19.2.0\"");

        let krate = build_crate_record("serde".to_string(), "1.0.228".to_string());
        assert_eq!(krate.cargo_toml_exact, "serde = \"=1.0.228\"");
    }
}
