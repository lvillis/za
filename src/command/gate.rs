//! CI Gate: enforce quality thresholds and deny rules; optional secrets scan.

use anyhow::{anyhow, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::command::walk_workspace;
use crate::command::stats::complexity_score;
use crate::command::secrets::{scan_secrets, SecretFinding, Severity};

use std::path::PathBuf;
use std::fs;

/// Gate violation codes for clarity.
#[derive(Debug)]
enum ViolationCode {
    BinaryTotalExceeded,
    FileTooLarge,
    DenyPattern,
    ComplexityExceeded,
    SecretLeak,
}

#[derive(Debug)]
struct Violation {
    code: ViolationCode,
    message: String,
    path: Option<String>,
}

pub fn run(
    max_binary_mib: Option<f64>,
    max_file_size_mib: Option<f64>,
    max_complexity: Option<usize>,
    deny_glob: Vec<String>,
    strict_secrets: bool,
    secrets_json: Option<PathBuf>,
    allow_secrets_in: Vec<String>,
) -> Result<()> {
    // Scan workspace, include binaries for size accounting.
    let (texts, bins) = walk_workspace(true)?;

    let mut violations: Vec<Violation> = Vec::new();

    // 1) Binary total size threshold
    let total_bin_bytes: usize = bins.iter().map(|b| b.bytes).sum();
    if let Some(th) = max_binary_mib {
        let limit_bytes = (th * 1_048_576.0) as usize;
        if total_bin_bytes > limit_bytes {
            violations.push(Violation {
                code: ViolationCode::BinaryTotalExceeded,
                message: format!(
                    "Total binary size {:.2} MiB exceeds limit {:.2} MiB",
                    mib(total_bin_bytes),
                    th
                ),
                path: None,
            });
        }
    }

    // 2) Per-file size threshold (text + bin)
    if let Some(th) = max_file_size_mib {
        let limit_bytes = (th * 1_048_576.0) as usize;
        for t in &texts {
            if t.bytes > limit_bytes {
                violations.push(Violation {
                    code: ViolationCode::FileTooLarge,
                    message: format!(
                        "File exceeds size limit ({:.2} MiB > {:.2} MiB)",
                        mib(t.bytes),
                        th
                    ),
                    path: Some(t.rel.display().to_string()),
                });
            }
        }
        for b in &bins {
            if b.bytes > limit_bytes {
                violations.push(Violation {
                    code: ViolationCode::FileTooLarge,
                    message: format!(
                        "File exceeds size limit ({:.2} MiB > {:.2} MiB)",
                        mib(b.bytes),
                        th
                    ),
                    path: Some(b.rel.display().to_string()),
                });
            }
        }
    }

    // 3) Deny patterns glob (text + bin)
    if let Some(deny) = build_globset(&deny_glob)? {
        for t in &texts {
            if deny.is_match(&t.rel) {
                violations.push(Violation {
                    code: ViolationCode::DenyPattern,
                    message: "File matches deny pattern".to_string(),
                    path: Some(t.rel.display().to_string()),
                });
            }
        }
        for b in &bins {
            if deny.is_match(&b.rel) {
                violations.push(Violation {
                    code: ViolationCode::DenyPattern,
                    message: "File matches deny pattern".to_string(),
                    path: Some(b.rel.display().to_string()),
                });
            }
        }
    }

    // 4) Complexity threshold
    if let Some(limit) = max_complexity {
        let c = complexity_score(&texts);
        if c > limit {
            violations.push(Violation {
                code: ViolationCode::ComplexityExceeded,
                message: format!("Complexity score {} exceeds limit {}", c, limit),
                path: None,
            });
        }
    }

    // 5) Secret scanning (warn or error)
    let allow_secrets = build_globset(&allow_secrets_in)?;
    let findings = scan_secrets(&texts, allow_secrets.as_ref());
    if let Some(dest) = secrets_json {
        write_secrets_json(&findings, dest)?;
    }
    if !findings.is_empty() {
        print_secret_findings(&findings);
        if strict_secrets {
            for f in findings {
                violations.push(Violation {
                    code: ViolationCode::SecretLeak,
                    message: format!("{}: {}", f.id, f.description),
                    path: Some(format!("{}:{}", f.path, f.line)),
                });
            }
        } else {
            println!("⚠️  Secrets detected (warnings). Use --strict-secrets to fail the gate.");
        }
    } else {
        println!("🔐 No secrets detected.");
    }

    // ---- Result & output ----
    if violations.is_empty() {
        println!("✅ Gate passed: no violations.");
        return Ok(());
    }

    // Print a concise violation list
    println!("\n❌ Gate failed with {} violation(s):", violations.len());
    for v in &violations {
        match &v.path {
            Some(p) => println!(" - {:?}: {} — {}", v.code, v.message, p),
            None => println!(" - {:?}: {}", v.code, v.message),
        }
    }

    Err(anyhow!("gate failed with {} violation(s)", violations.len()))
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / 1_048_576.0
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        let glob = Glob::new(p)?;
        builder.add(glob);
    }
    Ok(Some(builder.build()?))
}

fn write_secrets_json(findings: &[SecretFinding], dest: PathBuf) -> Result<()> {
    #[derive(serde::Serialize)]
    struct Report<'a> {
        generated_at: String,
        findings: &'a [SecretFinding],
    }
    let now = humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string();
    let report = Report { generated_at: now, findings };
    let buf = serde_json::to_vec_pretty(&report)?;
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    fs::write(dest, buf)?;
    println!("🧾 Secrets report written.");
    Ok(())
}

fn print_secret_findings(findings: &[SecretFinding]) {
    println!("🔐 Secret scan findings ({}):", findings.len());
    // Group by severity for better readability
    let mut high: Vec<_> = findings.iter().filter(|f| matches!(f.severity, Severity::High)).collect();
    let mut med:  Vec<_> = findings.iter().filter(|f| matches!(f.severity, Severity::Medium)).collect();
    let mut low:  Vec<_> = findings.iter().filter(|f| matches!(f.severity, Severity::Low)).collect();

    // Stable ordering by path/line
    high.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    med.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    low.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));

    for (label, items) in [("HIGH", high), ("MEDIUM", med), ("LOW", low)] {
        if items.is_empty() { continue; }
        println!("  ▸ Severity {label}: {}", items.len());
        for f in items {
            println!(
                "    - {}:{} [{}] {} — {}",
                f.path, f.line, f.id, f.description, f.snippet
            );
        }
    }
}
