//! Implementation for `za stats`.

use anyhow::Result;
use regex::Regex;
use std::{
    cmp::Reverse,
    fs::{self, File},
    io::{self, Write},
    path::PathBuf,
    time::SystemTime,
};

use humantime::format_rfc3339_seconds;

use crate::command::{lang_of, md_header, walk_workspace, TextFile};

/// Entry for `za stats`
pub fn run(top: usize, days: u32, json: Option<PathBuf>, md_out: PathBuf) -> Result<()> {
    // Always include binaries for accurate size accounting.
    let (texts, bins) = walk_workspace(true)?;

    let (lang_map, total_lines) = aggregate_lang(&texts);
    let bin_bytes: usize = bins.iter().map(|b| b.bytes).sum();
    let largest = largest_files(&texts, top);
    let (comments, blanks, total) = comment_blank_metrics(&texts);
    let complexity = complexity_score(&texts);
    let hotspots = recent_git_hotspots(&texts, days)?; // currently returns empty map (pure no-deps)

    write_stats_md(
        &lang_map,
        total_lines,
        bin_bytes,
        &largest,
        (comments, blanks, total),
        complexity,
        &hotspots,
        days,
        &md_out,
    )?;

    if let Some(p) = json {
        write_stats_json(&lang_map, total_lines, bin_bytes, &largest, &p)?;
        println!("🗄  JSON written: {}", p.display());
    }

    println!("📊 Stats written: {}", md_out.display());
    Ok(())
}

/* ---------- language distribution ---------- */
#[derive(Clone, serde::Serialize)]
struct LangStat {
    files: usize,
    lines: usize,
}

fn aggregate_lang(texts: &[TextFile]) -> (std::collections::HashMap<String, LangStat>, usize) {
    let mut map = std::collections::HashMap::new();
    let mut total = 0;

    for t in texts {
        let lang = lang_of(&t.rel).to_owned();
        let entry = map.entry(lang).or_insert(LangStat { files: 0, lines: 0 });
        entry.files += 1;
        entry.lines += t.lines.len();
        total += t.lines.len();
    }
    (map, total)
}

/* ---------- largest files ---------- */
#[derive(Clone, serde::Serialize)]
struct FileSize {
    path: String,
    lines: usize,
}

fn largest_files(texts: &[TextFile], top: usize) -> Vec<FileSize> {
    let mut v: Vec<_> = texts.iter().map(|t| (t.lines.len(), &t.rel)).collect();
    // Desc by lines, tie-break by path for determinism.
    v.sort_by(|(la, pa), (lb, pb)| lb.cmp(la).then_with(|| pa.cmp(pb)));
    v.truncate(top);

    v.into_iter()
        .map(|(l, p)| FileSize {
            path: p.display().to_string(),
            lines: l,
        })
        .collect()
}

/* ---------- comment / blank ratio ---------- */

fn is_comment_line(lang: &str, trim: &str) -> bool {
    // Language-aware single-line comment detection (best-effort).
    match lang {
        // C-family and similar languages with // and /* ... */
        "rust" | "go" | "c" | "cpp" | "java"
        | "javascript" | "typescript" | "tsx" | "jsx"
        | "csharp" | "kotlin" | "php" | "swift" => {
            trim.starts_with("//") || trim.starts_with("/*") || trim.starts_with('*') || trim.starts_with("*/")
        }
        // Hash-prefixed languages / formats
        "python" | "shell" | "yaml" | "toml" | "make" | "ruby" | "dockerfile" => trim.starts_with('#'),
        // Markup with <!-- --> comments
        "markdown" | "html" | "xml" => trim.starts_with("<!--"),
        _ => false,
    }
}

fn comment_blank_metrics(texts: &[TextFile]) -> (usize, usize, usize) {
    let mut comments = 0;
    let mut blanks = 0;
    let mut total = 0;

    for t in texts {
        let lang = lang_of(&t.rel);
        for line in &t.lines {
            total += 1;
            let trim = line.trim();
            if trim.is_empty() {
                blanks += 1;
            } else if is_comment_line(lang, trim) {
                comments += 1;
            }
        }
    }

    (comments, blanks, total)
}

/* ---------- naive complexity score ---------- */
pub(crate) fn complexity_score(texts: &[TextFile]) -> usize {
    // A tiny proxy based on control-flow keywords.
    let kw_re = Regex::new(r"\b(if|for|while|match|loop|fn)\b").unwrap();
    let mut score = 0;
    for t in texts {
        for line in &t.lines {
            if kw_re.is_match(line) {
                score += 1;
            }
        }
    }
    score
}

/* ---------- Git hotspots (pure no-deps stub) ---------- */
fn recent_git_hotspots(
    _texts: &[TextFile],
    _days: u32,
) -> Result<std::collections::HashMap<String, usize>> {
    // To keep the build pure-Rust and avoid extra network/crypto stacks,
    // this version intentionally disables hotspot analysis.
    // If you want a pure-gix implementation, say the word and I will provide it.
    Ok(std::collections::HashMap::new())
}

/* ---------- render Markdown ---------- */
fn write_stats_md(
    langs: &std::collections::HashMap<String, LangStat>,
    total_lines: usize,
    bin_bytes: usize,
    largest: &[FileSize],
    (comments, blanks, total): (usize, usize, usize),
    complexity: usize,
    hotspots: &std::collections::HashMap<String, usize>,
    days: u32,
    out: &PathBuf,
) -> io::Result<()> {
    let mut f = File::create(out)?;
    md_header(&mut f, "# 📊 Repository Statistics — generated by za")?;

    let denom = total.max(1); // avoid div-by-zero
    writeln!(f, "## 1. Summary\n")?;
    writeln!(
        f,
        "- **Total files**: {}",
        langs.values().map(|l| l.files).sum::<usize>()
    )?;
    writeln!(f, "- **Total lines**: {}", total_lines)?;
    writeln!(f, "- **Binary size**: {:.2} MiB", bin_bytes as f64 / 1_048_576.0)?;
    writeln!(
        f,
        "- **Comments / blanks**: {:.1}% / {:.1}%",
        comments as f64 * 100.0 / denom as f64,
        blanks as f64 * 100.0 / denom as f64
    )?;
    writeln!(f, "- **Complexity estimate**: {}", complexity)?;
    writeln!(f)?;

    // Sort languages by lines desc, tie-break by name.
    let mut items: Vec<_> = langs.iter().collect();
    items.sort_by(|(la, sa), (lb, sb)| sb.lines.cmp(&sa.lines).then_with(|| la.cmp(lb)));

    writeln!(f, "## 2. Language Breakdown\n")?;
    writeln!(f, "| Language | Files | Lines | Ratio |")?;
    writeln!(f, "|----------|------:|------:|------:|")?;
    let denom_lang = total_lines.max(1);
    for (lang, s) in items {
        writeln!(
            f,
            "| {:<10} | {:>5} | {:>6} | {:>5.1}% |",
            lang,
            s.files,
            s.lines,
            s.lines as f64 * 100.0 / denom_lang as f64
        )?;
    }
    writeln!(f)?;

    writeln!(f, "## 3. Largest {} Files\n", largest.len())?;
    writeln!(f, "| File | Lines |")?;
    writeln!(f, "|------|------:|")?;
    for l in largest {
        writeln!(f, "| {} | {} |", l.path, l.lines)?;
    }
    writeln!(f)?;

    if !hotspots.is_empty() {
        writeln!(f, "## 4. Hotspots (commits in last {} days)\n", days)?;
        writeln!(f, "| File | Commits |")?;
        writeln!(f, "|------|--------:|")?;
        let mut v: Vec<_> = hotspots.iter().collect();
        v.sort_by_key(|(_, c)| Reverse(**c));
        for (p, c) in v.iter().take(20) {
            writeln!(f, "| {} | {} |", p, c)?;
        }
    }
    Ok(())
}

/* ---------- render JSON ---------- */
#[derive(serde::Serialize)]
struct JsonStats {
    generated_at: String,
    total_files: usize,
    total_lines: usize,
    total_binary_bytes: usize,
    languages: std::collections::HashMap<String, LangStat>,
    largest_files: Vec<FileSize>,
}

fn write_stats_json(
    langs: &std::collections::HashMap<String, LangStat>,
    total_lines: usize,
    bin_bytes: usize,
    largest: &[FileSize],
    out: &PathBuf,
) -> Result<()> {
    let js = JsonStats {
        generated_at: format_rfc3339_seconds(SystemTime::now()).to_string(),
        total_files: langs.values().map(|l| l.files).sum(),
        total_lines,
        total_binary_bytes: bin_bytes,
        languages: langs.clone(),
        largest_files: largest.to_vec(),
    };
    fs::write(out, serde_json::to_vec_pretty(&js)?)?;
    Ok(())
}
