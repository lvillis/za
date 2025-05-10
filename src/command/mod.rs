//! Shared helpers and constants for all sub-commands.

pub mod r#gen;
pub mod stats;

use anyhow::Result;
use humantime::format_rfc3339_seconds;
use ignore::WalkBuilder;
use indicatif::{ProgressBar, ProgressStyle};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

/// ---------- constants ----------
pub const DEFAULT_MAX_LINES_PER_FILE: usize = 400;
pub const STAT_TOP_N: usize = 10;
pub const STAT_RECENT_DAYS: u32 = 30;
const SKIP_BASENAMES: &[&str] = &[".gitignore", ".aiignore", "CONTEXT.md"];

/// ---------- data structs ----------
#[derive(Clone)]
pub struct TextFile {
    pub rel: PathBuf,
    pub lines: Vec<String>,
}

#[derive(Clone)]
pub struct BinaryFile {
    pub rel: PathBuf,
    pub bytes: usize,
}

/// ---------- workspace traversal ----------
pub fn walk_workspace(include_binary: bool) -> Result<(Vec<TextFile>, Vec<BinaryFile>)> {
    let root = std::env::current_dir()?;

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner} {wide_msg}")?
            .tick_strings(&["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"]),
    );

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

        let bytes = fs::read(p)?;
        if let Ok(txt) = std::str::from_utf8(&bytes) {
            texts.push(TextFile {
                rel: p.strip_prefix(&root)?.to_path_buf(),
                lines: txt.lines().map(|s| s.to_owned()).collect(),
            });
        } else if include_binary {
            bins.push(BinaryFile {
                rel: p.strip_prefix(&root)?.to_path_buf(),
                bytes: bytes.len(),
            });
        }
    }

    pb.finish_and_clear();
    Ok((texts, bins))
}

/// ---------- language detection ----------
pub fn lang_of(path: &Path) -> &'static str {
    match path.extension().and_then(OsStr::to_str) {
        Some("rs") => "rust",
        Some("go") => "go",
        Some("py") => "python",
        Some("ts") => "typescript",
        Some("js") => "javascript",
        Some("java") => "java",
        Some("c" | "h") => "c",
        Some("cpp" | "hpp" | "cc") => "cpp",
        Some("toml") => "toml",
        Some("yaml" | "yml") => "yaml",
        Some("md") => "markdown",
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
