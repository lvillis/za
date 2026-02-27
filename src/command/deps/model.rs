use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::time::SystemTime;

pub(super) struct DependencySpecBuilder {
    pub(super) requirements: BTreeSet<String>,
    pub(super) kinds: BTreeSet<String>,
    pub(super) optional: bool,
}

impl Default for DependencySpecBuilder {
    fn default() -> Self {
        Self {
            requirements: BTreeSet::new(),
            kinds: BTreeSet::new(),
            optional: true,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct DependencySpec {
    pub(super) name: String,
    pub(super) requirement: String,
    pub(super) kinds: String,
    pub(super) optional: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum RiskLevel {
    High,
    Medium,
    Low,
    Unknown,
}

impl RiskLevel {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Unknown => "unknown",
        }
    }

    pub(super) fn weight(self) -> u8 {
        match self {
            Self::High => 4,
            Self::Medium => 3,
            Self::Low => 2,
            Self::Unknown => 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct AuditSummary {
    pub(super) high: usize,
    pub(super) medium: usize,
    pub(super) low: usize,
    pub(super) unknown: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct DepAuditRecord {
    pub(super) name: String,
    pub(super) requirement: String,
    pub(super) kinds: String,
    pub(super) optional: bool,
    pub(super) latest_version: Option<String>,
    pub(super) crate_updated_at: Option<String>,
    pub(super) latest_release_at: Option<String>,
    pub(super) latest_release_age_days: Option<u64>,
    pub(super) repository: Option<String>,
    pub(super) github_stars: Option<u64>,
    pub(super) github_archived: Option<bool>,
    pub(super) github_pushed_at: Option<String>,
    pub(super) github_push_age_days: Option<u64>,
    pub(super) std_alternative: Option<String>,
    pub(super) risk: RiskLevel,
    pub(super) notes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct AuditReport {
    pub(super) generated_at: String,
    pub(super) manifest_path: String,
    pub(super) summary: AuditSummary,
    pub(super) dependencies: Vec<DepAuditRecord>,
}

pub(super) fn classify_risk(record: &mut DepAuditRecord) {
    let mut risk = RiskLevel::Low;
    let mut reasons = Vec::new();
    let github_expected = record
        .repository
        .as_deref()
        .and_then(github_repo_from_url)
        .is_some();

    if record.github_archived == Some(true) {
        elevate(&mut risk, RiskLevel::High);
        reasons.push("GitHub repo is archived".to_string());
    }

    if let Some(days) = record.latest_release_age_days {
        if days >= 1460 {
            elevate(&mut risk, RiskLevel::High);
            reasons.push(format!("latest crate release is stale ({days} days)"));
        } else if days >= 730 {
            elevate(&mut risk, RiskLevel::Medium);
            reasons.push(format!("crate release not recent ({days} days)"));
        }
    }

    if let Some(days) = record.github_push_age_days {
        if days >= 1460 {
            elevate(&mut risk, RiskLevel::High);
            reasons.push(format!("GitHub repo activity is stale ({days} days)"));
        } else if days >= 365 {
            elevate(&mut risk, RiskLevel::Medium);
            reasons.push(format!("GitHub activity older than 1 year ({days} days)"));
        }
    }

    if let Some(stars) = record.github_stars {
        if stars <= 50 {
            elevate(&mut risk, RiskLevel::Medium);
            reasons.push(format!("low community signal (stars={stars})"));
        } else if stars <= 150 {
            elevate(&mut risk, RiskLevel::Low);
            reasons.push(format!("small community size (stars={stars})"));
        }
    }

    if let Some(std_alt) = record.std_alternative.as_deref() {
        reasons.push(format!("std alternative available: {std_alt}"));
    }

    if github_expected
        && record.github_stars.is_none()
        && record.github_archived.is_none()
        && record.github_pushed_at.is_none()
    {
        risk = RiskLevel::Unknown;
        reasons.push("GitHub signals unavailable (set GITHUB_TOKEN for stable quota)".to_string());
    }

    if record.latest_release_at.is_none() && record.github_pushed_at.is_none() {
        risk = RiskLevel::Unknown;
        reasons.push("insufficient maintenance signals".to_string());
    }

    record.risk = risk;
    record.notes.splice(0..0, reasons);
}

pub(super) fn elevate(current: &mut RiskLevel, next: RiskLevel) {
    if next.weight() > current.weight() {
        *current = next;
    }
}

pub(super) fn age_days_from_now(rfc3339: &str) -> Option<u64> {
    let ts = humantime::parse_rfc3339_weak(rfc3339).ok()?;
    match SystemTime::now().duration_since(ts) {
        Ok(duration) => Some(duration.as_secs() / 86_400),
        Err(_) => Some(0),
    }
}

pub(super) fn std_alternative(crate_name: &str) -> Option<&'static str> {
    match crate_name {
        "once_cell" => Some("std::sync::LazyLock / OnceLock"),
        "is-terminal" => Some("std::io::IsTerminal"),
        _ => None,
    }
}

pub(super) fn github_repo_from_url(url: &str) -> Option<(String, String)> {
    let mut raw = url.trim().trim_end_matches('/');
    if raw.is_empty() {
        return None;
    }

    if let Some(rest) = raw.strip_prefix("git@github.com:") {
        return parse_owner_repo(rest);
    }
    if let Some((_, rest)) = raw.split_once("github.com/") {
        raw = rest;
        return parse_owner_repo(raw);
    }
    None
}

fn parse_owner_repo(path: &str) -> Option<(String, String)> {
    let trimmed = path
        .split('?')
        .next()
        .unwrap_or(path)
        .split('#')
        .next()
        .unwrap_or(path)
        .trim_end_matches('/');
    let mut parts = trimmed.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim().trim_end_matches(".git");
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

#[derive(Clone)]
pub(super) enum GitHubCacheEntry {
    Hit(super::GitHubRepoResponse),
    Miss(String),
}

impl GitHubCacheEntry {
    pub(super) fn into_result(self) -> Result<super::GitHubRepoResponse> {
        match self {
            Self::Hit(repo) => Ok(repo),
            Self::Miss(err) => bail!("{err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RiskLevel, elevate, github_repo_from_url, parse_owner_repo};

    #[test]
    fn parse_github_https_repo() {
        let slug = github_repo_from_url("https://github.com/owner/repo");
        assert_eq!(slug, Some(("owner".to_string(), "repo".to_string())));
    }

    #[test]
    fn parse_github_ssh_repo() {
        let slug = github_repo_from_url("git@github.com:owner/repo.git");
        assert_eq!(slug, Some(("owner".to_string(), "repo".to_string())));
    }

    #[test]
    fn parse_owner_repo_rejects_missing() {
        assert!(parse_owner_repo("owner").is_none());
        assert!(parse_owner_repo("/").is_none());
    }

    #[test]
    fn elevate_risk_level() {
        let mut risk = RiskLevel::Low;
        elevate(&mut risk, RiskLevel::Medium);
        assert_eq!(risk, RiskLevel::Medium);
        elevate(&mut risk, RiskLevel::Low);
        assert_eq!(risk, RiskLevel::Medium);
        elevate(&mut risk, RiskLevel::High);
        assert_eq!(risk, RiskLevel::High);
    }
}
