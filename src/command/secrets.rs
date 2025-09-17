//! Secrets scanner: detect common credential patterns in text content.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;

use globset::GlobSet;

use crate::command::TextFile;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    High,
    Medium,
    Low,
}

#[derive(Debug)]
struct Pattern {
    id: &'static str,
    description: &'static str,
    severity: Severity,
    re: Regex,
}

#[derive(Debug, Clone, Serialize)]
pub struct SecretFinding {
    pub path: String,
    pub line: usize,
    pub id: &'static str,
    pub description: &'static str,
    pub severity: Severity,
    pub snippet: String, // redacted
}

static PATTERNS: Lazy<Vec<Pattern>> = Lazy::new(|| {
    vec![
        Pattern {
            id: "PRIVATE_KEY",
            description: "Private key material (PEM)",
            severity: Severity::High,
            re: Regex::new(r"-----BEGIN (?:RSA|EC|DSA|OPENSSH) PRIVATE KEY-----").unwrap(),
        },
        Pattern {
            id: "AWS_ACCESS_KEY_ID",
            description: "AWS Access Key ID",
            severity: Severity::High,
            re: Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap(),
        },
        Pattern {
            id: "GITHUB_TOKEN",
            description: "GitHub personal access token",
            severity: Severity::High,
            re: Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{36}\b").unwrap(), // ghp_/gho_/ghs_/...
        },
        Pattern {
            id: "SLACK_TOKEN",
            description: "Slack token",
            severity: Severity::Medium,
            re: Regex::new(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b").unwrap(),
        },
        Pattern {
            id: "STRIPE_SECRET",
            description: "Stripe secret key",
            severity: Severity::High,
            re: Regex::new(r"\bsk_(?:live|test)_[A-Za-z0-9]{24,}\b").unwrap(),
        },
        Pattern {
            id: "GOOGLE_API_KEY",
            description: "Google API key",
            severity: Severity::Medium,
            re: Regex::new(r"\bAIza[0-9A-Za-z\-_]{35}\b").unwrap(),
        },
        // NOTE: This regex contains double quotes inside a character class.
        // Use r#"... "# raw string so that `"` is allowed in the literal.
        Pattern {
            id: "GENERIC_PASSWORD",
            description: "Potential hard-coded password/secret",
            severity: Severity::Low,
            re: Regex::new(
                r#"(?i)\b(pass(word)?|secret|token|api[_-]?key)\b\s*[:=]\s*['"][A-Za-z0-9/\+=!@#$%^&*()_\-]{8,}['"]"#
            ).unwrap(),
        },
    ]
});

fn redact(s: &str) -> String {
    // Keep first/last 4 chars to help triage; mask the rest.
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let prefix: String = chars.iter().take(4).collect();
    let suffix: String = chars.iter().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{prefix}…{suffix}")
}

/// Scan secrets in text files, honoring an optional allowlist globset.
/// Files matching `allow` are skipped entirely.
pub fn scan_secrets(texts: &[TextFile], allow: Option<&GlobSet>) -> Vec<SecretFinding> {
    let mut out = Vec::new();

    'file_loop: for t in texts {
        if let Some(gs) = allow {
            if gs.is_match(&t.rel) {
                continue 'file_loop;
            }
        }
        let p = t.rel.display().to_string();
        for (idx, line) in t.lines.iter().enumerate() {
            for pat in PATTERNS.iter() {
                for m in pat.re.find_iter(line) {
                    out.push(SecretFinding {
                        path: p.clone(),
                        line: idx + 1,
                        id: pat.id,
                        description: pat.description,
                        severity: pat.severity,
                        snippet: redact(m.as_str()),
                    });
                }
            }
        }
    }

    out
}
