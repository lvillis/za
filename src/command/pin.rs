//! Resolve floating dependency/action references into copy-pastable pins.

use crate::{
    cli::PinCommands,
    command::{render as text_render, za_config},
};
use anyhow::{Context, Result, anyhow, bail};
use reqx::{
    advanced::ClientProfile,
    blocking::{Client, ClientBuilder},
    prelude::RetryPolicy,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, env, time::Duration};

const HTTP_TIMEOUT_SECS: u64 = 30;
const HTTP_USER_AGENT: &str = "za-pin/0.1";
const NPM_REGISTRY_BASE: &str = "https://registry.npmjs.org";
const CRATES_API_BASE: &str = "https://crates.io";
const GITHUB_API_BASE: &str = "https://api.github.com";
const GITHUB_API_VERSION: &str = "2022-11-28";
const DEFAULT_NPM_TAG: &str = "latest";

pub fn run(cmd: PinCommands) -> Result<i32> {
    match cmd {
        PinCommands::Npm { package, tag, json } => {
            let query = parse_npm_package_query(&package, tag.as_deref())?;
            let record = resolve_npm_pin(&query)?;
            if json {
                print_json(&record)?;
            } else {
                print_npm_pin(&record);
            }
        }
        PinCommands::Crate { name, json } => {
            let name = normalize_crate_name(&name)?;
            let record = resolve_crate_pin(&name)?;
            if json {
                print_json(&record)?;
            } else {
                print_crate_pin(&record);
            }
        }
        PinCommands::Action {
            spec,
            github_token,
            json,
        } => {
            let spec = ActionSpec::parse(&spec)?;
            let record = resolve_action_pin(&spec, github_token)?;
            if json {
                print_json(&record)?;
            } else {
                print_action_pin(&record);
            }
        }
    }
    Ok(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NpmPackageQuery {
    package: String,
    tag: String,
}

#[derive(Debug, Serialize)]
struct NpmPinRecord {
    kind: &'static str,
    package: String,
    requested_tag: String,
    version: String,
    npm_spec: String,
    npm_install: String,
    package_json: String,
    package_json_caret: String,
}

#[derive(Debug, Serialize)]
struct CratePinRecord {
    kind: &'static str,
    name: String,
    version: String,
    cargo_add: String,
    cargo_toml: String,
    cargo_toml_exact: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActionSpec {
    owner: String,
    repo: String,
    path: Option<String>,
    ref_name: String,
}

#[derive(Debug, Serialize)]
struct ActionPinRecord {
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

#[derive(Debug, Deserialize)]
struct CratesApiResponse {
    #[serde(rename = "crate")]
    krate: CratesCrate,
}

#[derive(Debug, Deserialize)]
struct CratesCrate {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    max_stable_version: Option<String>,
    #[serde(default)]
    max_version: Option<String>,
    #[serde(default)]
    newest_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubCommitResponse {
    sha: String,
}

fn resolve_npm_pin(query: &NpmPackageQuery) -> Result<NpmPinRecord> {
    let client = build_http_client(NPM_REGISTRY_BASE)?;
    let path = format!("/{}", encode_npm_package_for_url(&query.package));
    let mut req = client.get(path.clone());
    req = req
        .try_header("user-agent", HTTP_USER_AGENT)
        .context("set npm registry user-agent")?;

    let response = req
        .send_response()
        .with_context(|| format!("request npm registry `{path}`"))?;
    let status = response.status();
    if !status.is_success() {
        let body = text_render::truncate_end(&response.text_lossy(), 200);
        if status.as_u16() == 404 {
            bail!(
                "npm package `{}` was not found. body: {body}",
                query.package
            );
        }
        bail!(
            "npm registry returned status {} for `{}`. body: {}",
            status,
            query.package,
            body
        );
    }

    let parsed = response
        .json::<NpmPackageResponse>()
        .with_context(|| format!("parse npm registry response for `{}`", query.package))?;
    let version = parsed
        .dist_tags
        .get(&query.tag)
        .cloned()
        .or_else(|| {
            parsed
                .versions
                .contains_key(&query.tag)
                .then(|| query.tag.clone())
        })
        .ok_or_else(|| {
            anyhow!(
                "npm package `{}` has no dist-tag or version `{}`",
                query.package,
                query.tag
            )
        })?;
    let package = parsed.name;
    Ok(build_npm_record(package, query.tag.clone(), version))
}

fn resolve_crate_pin(name: &str) -> Result<CratePinRecord> {
    let client = build_http_client(CRATES_API_BASE)?;
    let path = format!("/api/v1/crates/{name}");
    let mut req = client.get(path.clone());
    req = req
        .try_header("user-agent", HTTP_USER_AGENT)
        .context("set crates.io user-agent")?;

    let response = req
        .send_response()
        .with_context(|| format!("request crates.io API `{path}`"))?;
    let status = response.status();
    if !status.is_success() {
        let body = text_render::truncate_end(&response.text_lossy(), 200);
        if status.as_u16() == 404 {
            bail!("crate `{name}` was not found. body: {body}");
        }
        bail!(
            "crates.io returned status {} for `{}`. body: {}",
            status,
            name,
            body
        );
    }

    let parsed = response
        .json::<CratesApiResponse>()
        .with_context(|| format!("parse crates.io response for `{name}`"))?;
    let version = parsed
        .krate
        .max_stable_version
        .or(parsed.krate.max_version)
        .or(parsed.krate.newest_version)
        .ok_or_else(|| anyhow!("crate `{name}` response did not include a latest version"))?;
    let name = parsed.krate.name.unwrap_or_else(|| name.to_string());
    Ok(build_crate_record(name, version))
}

fn resolve_action_pin(spec: &ActionSpec, github_token: Option<String>) -> Result<ActionPinRecord> {
    if is_full_commit_sha(&spec.ref_name) {
        return Ok(build_action_record(spec, spec.ref_name.clone(), "input"));
    }

    let token = resolve_github_token(github_token)?;
    let client = build_http_client(GITHUB_API_BASE)?;
    let ref_path = percent_encode_path_segment(&spec.ref_name);
    let path = format!(
        "/repos/{}/{}/commits/{}",
        percent_encode_path_segment(&spec.owner),
        percent_encode_path_segment(&spec.repo),
        ref_path
    );
    let mut req = client.get(path.clone());
    req = req
        .try_header("user-agent", HTTP_USER_AGENT)
        .context("set GitHub user-agent")?;
    req = req
        .try_header("accept", "application/vnd.github+json")
        .context("set GitHub accept header")?;
    req = req
        .try_header("x-github-api-version", GITHUB_API_VERSION)
        .context("set GitHub API version header")?;
    if let Some(token) = token.as_deref() {
        req = req
            .try_header("authorization", &format!("Bearer {token}"))
            .context("set GitHub authorization header")?;
    }

    let response = req
        .send_response()
        .with_context(|| format!("request GitHub API `{path}`"))?;
    let status = response.status();
    if !status.is_success() {
        let body = text_render::truncate_end(&response.text_lossy(), 200);
        if status.as_u16() == 403 && token.is_none() {
            bail!(
                "GitHub API returned 403 for `{}`; set GITHUB_TOKEN, GH_TOKEN, or `za config set github-token <token>`. body: {body}",
                spec.input()
            );
        }
        if status.as_u16() == 404 {
            bail!("GitHub ref `{}` was not found. body: {body}", spec.input());
        }
        bail!(
            "GitHub API returned status {} for `{}`. body: {}",
            status,
            spec.input(),
            body
        );
    }

    let parsed = response
        .json::<GitHubCommitResponse>()
        .with_context(|| format!("parse GitHub commit response for `{}`", spec.input()))?;
    if !is_full_commit_sha(&parsed.sha) {
        bail!("GitHub returned invalid commit SHA `{}`", parsed.sha);
    }
    Ok(build_action_record(spec, parsed.sha, "github"))
}

fn build_npm_record(package: String, requested_tag: String, version: String) -> NpmPinRecord {
    let npm_spec = format!("{package}@{version}");
    NpmPinRecord {
        kind: "npm",
        package: package.clone(),
        requested_tag,
        version: version.clone(),
        npm_install: format!("npm install {npm_spec}"),
        npm_spec,
        package_json: format!("\"{package}\": \"{version}\""),
        package_json_caret: format!("\"{package}\": \"^{version}\""),
    }
}

fn build_crate_record(name: String, version: String) -> CratePinRecord {
    CratePinRecord {
        kind: "crate",
        name: name.clone(),
        version: version.clone(),
        cargo_add: format!("cargo add {name}@{version}"),
        cargo_toml: format!("{name} = \"{version}\""),
        cargo_toml_exact: format!("{name} = \"={version}\""),
    }
}

fn build_action_record(spec: &ActionSpec, sha: String, source: &'static str) -> ActionPinRecord {
    let action = spec.action_without_ref();
    ActionPinRecord {
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

fn print_npm_pin(record: &NpmPinRecord) {
    println!("npm {}", record.package);
    print_kv("version", &record.version);
    print_kv("npm spec", &record.npm_spec);
    print_kv("npm install", &record.npm_install);
    print_kv("package.json", &record.package_json);
    print_kv("package.json^", &record.package_json_caret);
}

fn print_crate_pin(record: &CratePinRecord) {
    println!("crate {}", record.name);
    print_kv("version", &record.version);
    print_kv("cargo add", &record.cargo_add);
    print_kv("Cargo.toml", &record.cargo_toml);
    print_kv("Cargo.toml =", &record.cargo_toml_exact);
}

fn print_action_pin(record: &ActionPinRecord) {
    let action = match record.path.as_deref() {
        Some(path) => format!("{}/{}/{}", record.owner, record.repo, path),
        None => format!("{}/{}", record.owner, record.repo),
    };
    println!("action {}@{}", action, record.input_ref);
    print_kv("sha", &record.sha);
    print_kv("uses", &record.uses);
}

fn print_kv(label: &str, value: &str) {
    println!("{label:<13} {value}");
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
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
    fn parse(input: &str) -> Result<Self> {
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
        if rest.iter().any(|part| part.is_empty()) {
            bail!("action path must not contain empty segments");
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

    fn action_without_ref(&self) -> String {
        match self.path.as_deref() {
            Some(path) => format!("{}/{}/{}", self.owner, self.repo, path),
            None => format!("{}/{}", self.owner, self.repo),
        }
    }
}

fn encode_npm_package_for_url(package: &str) -> String {
    package.replace('/', "%2f")
}

fn percent_encode_path_segment(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

fn is_full_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn build_http_client(base_url: &str) -> Result<Client> {
    let mut builder = Client::builder(base_url)
        .profile(ClientProfile::StandardSdk)
        .request_timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .total_timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .retry_policy(RetryPolicy::disabled())
        .client_name("za-pin");
    let scheme = base_url
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .unwrap_or("https");
    builder = apply_proxy_with_scope(builder, scheme, za_config::ProxyScope::Deps)
        .with_context(|| format!("configure HTTP client proxy for `{base_url}`"))?;
    builder
        .build()
        .with_context(|| format!("build HTTP client for `{base_url}`"))
}

fn apply_proxy_with_scope(
    mut builder: ClientBuilder,
    scheme: &str,
    proxy_scope: za_config::ProxyScope,
) -> Result<ClientBuilder> {
    let overrides = za_config::load_proxy_overrides(proxy_scope)?;
    let proxy_value = if scheme.eq_ignore_ascii_case("https") {
        overrides
            .https_proxy
            .clone()
            .or_else(|| overrides.all_proxy.clone())
            .or_else(|| overrides.http_proxy.clone())
    } else {
        overrides
            .http_proxy
            .clone()
            .or_else(|| overrides.all_proxy.clone())
            .or_else(|| overrides.https_proxy.clone())
    };

    let (proxy_var, proxy_value) = if let Some(value) = proxy_value {
        ("config".to_string(), value)
    } else if let Some((name, value)) = first_env_value(proxy_env_keys_for_scheme(scheme)) {
        (name, value)
    } else {
        return Ok(builder);
    };

    let proxy_uri = proxy_value
        .parse()
        .with_context(|| format!("invalid proxy URI in `{proxy_var}`"))?;
    builder = builder.http_proxy(proxy_uri);

    let no_proxy_raw = overrides
        .no_proxy
        .clone()
        .or_else(|| first_env_value(&["NO_PROXY", "no_proxy"]).map(|(_, value)| value));
    if let Some(no_proxy_raw) = no_proxy_raw {
        let rules = split_no_proxy_rules(&no_proxy_raw);
        if !rules.is_empty() {
            builder = builder
                .try_no_proxy(rules)
                .context("invalid `NO_PROXY`/`no_proxy` rules")?;
        }
    }

    Ok(builder)
}

fn proxy_env_keys_for_scheme(scheme: &str) -> &'static [&'static str] {
    if scheme.eq_ignore_ascii_case("https") {
        &[
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ]
    } else {
        &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    }
}

fn first_env_value(names: &[&str]) -> Option<(String, String)> {
    for name in names {
        let Ok(value) = env::var(name) else {
            continue;
        };
        let value = value.trim();
        if !value.is_empty() {
            return Some(((*name).to_string(), value.to_string()));
        }
    }
    None
}

fn split_no_proxy_rules(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn resolve_github_token(override_token: Option<String>) -> Result<Option<String>> {
    if let Some(token) = normalize_owned(override_token) {
        return Ok(Some(token));
    }
    for key in ["ZA_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(value) = env::var(key)
            && let Some(token) = normalize_owned(Some(value))
        {
            return Ok(Some(token));
        }
    }
    za_config::load_github_token()
}

fn normalize_owned(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(krate.cargo_toml, "serde = \"1.0.228\"");
        assert_eq!(krate.cargo_toml_exact, "serde = \"=1.0.228\"");
    }
}
