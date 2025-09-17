//! Shared helpers and constants for all sub-commands.

pub mod r#gen;
pub mod stats;
pub mod gate;      // NEW: CI gate
mod secrets;       // NEW: secrets scanner (internal)

use anyhow::Result;
use humantime::format_rfc3339_seconds;
use ignore::WalkBuilder;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use is_terminal::IsTerminal;
use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// ---------- constants ----------
pub const DEFAULT_MAX_LINES_PER_FILE: usize = 400;
pub const STAT_TOP_N: usize = 10;
pub const STAT_RECENT_DAYS: u32 = 30;

/// Files to skip regardless of ignore settings.
const SKIP_BASENAMES: &[&str] = &[
    ".gitignore", ".aiignore",
    "CONTEXT.md", "STATS.md", "stats.json",
];

/// ---------- data structs ----------
#[derive(Clone)]
pub struct TextFile {
    pub rel: PathBuf,
    pub lines: Vec<String>,
    pub bytes: usize, // size from metadata for gate checks
}

#[derive(Clone)]
pub struct BinaryFile {
    pub rel: PathBuf,
    pub bytes: usize,
}

/// ---------- workspace traversal ----------
/// Walk current workspace and collect text & binary files.
/// NOTE:
/// - Text files are fully read into memory as lines.
/// - Binary files are never fully read; we record size via metadata only.
pub fn walk_workspace(include_binary: bool) -> Result<(Vec<TextFile>, Vec<BinaryFile>)> {
    let root = std::env::current_dir()?;

    // Configure spinner: steady tick; hide on non-TTY (e.g., CI).
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner} {wide_msg}")?
            .tick_strings(&["⣾","⣽","⣻","⢿","⡿","⣟","⣯","⣷"]),
    );
    pb.enable_steady_tick(Duration::from_millis(80));
    if !std::io::stderr().is_terminal() {
        pb.set_draw_target(ProgressDrawTarget::hidden());
    }

    let mut texts = Vec::new();
    let mut bins = Vec::new();

    let mut wb = WalkBuilder::new(&root);
    wb.standard_filters(true)
        .hidden(false)
        .add_custom_ignore_filename(".aiignore");

    for dent in wb.build() {
        let dent = dent?;
        let p = dent.path();

        if !p.is_file() {
            continue;
        }
        if p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| SKIP_BASENAMES.contains(&n))
            .unwrap_or(false)
        {
            continue;
        }

        pb.set_message(p.strip_prefix(&root)?.display().to_string());

        let meta = fs::metadata(p)?;
        match fs::read_to_string(p) {
            Ok(txt) => {
                texts.push(TextFile {
                    rel: p.strip_prefix(&root)?.to_path_buf(),
                    lines: txt.lines().map(|s| s.to_owned()).collect(),
                    bytes: meta.len() as usize,
                });
            }
            Err(_) if include_binary => {
                bins.push(BinaryFile {
                    rel: p.strip_prefix(&root)?.to_path_buf(),
                    bytes: meta.len() as usize,
                });
            }
            Err(_) => { /* ignore binary if not requested */ }
        }
    }

    pb.finish_and_clear();
    Ok((texts, bins))
}

/// ---------- language detection ----------
pub fn lang_of(path: &Path) -> &'static str {
    // Handle common no-extension filenames.
    if let Some(name) = path.file_name().and_then(OsStr::to_str) {
        if name.eq_ignore_ascii_case("Dockerfile") {
            return "dockerfile";
        }
        if name.eq_ignore_ascii_case("Makefile") {
            return "make";
        }
    }
    let ext = match path.extension().and_then(OsStr::to_str) {
        Some(e) => e.to_ascii_lowercase(),
        None => return "other",
    };
    match ext.as_str() {
        "rs" => "rust",
        "go" => "go",
        "py" => "python",
        "ts" => "typescript",
        "tsx" => "tsx",
        "js" => "javascript",
        "jsx" => "jsx",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "hpp" | "cc" | "cxx" | "hh" => "cpp",
        "cs" => "csharp",
        "kt" | "kts" => "kotlin",
        "php" => "php",
        "rb" => "ruby",
        "swift" => "swift",
        "sh" | "bash" | "zsh" => "shell",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        "md" | "mdx" => "markdown",
        "html" | "htm" => "html",
        "css" | "scss" | "sass" => "css",
        "sql" => "sql",
        "proto" => "protobuf",
        "xml" => "xml",
        _ => "other",
    }
}

/// ---------- Markdown header helper ----------
pub fn md_header(f: &mut File, title: &str) -> io::Result<()> {
    writeln!(
        f,
        "{title}\n_Generated at: {}_\n",
        format_rfc3339_seconds(SystemTime::now())
    )
}
