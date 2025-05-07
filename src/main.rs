// src/main.rs
use anyhow::Result;
use clap::{Parser, Subcommand};
use ignore::WalkBuilder;
use indicatif::{ProgressBar, ProgressStyle};
use std::{
    ffi::OsStr,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Maximum number of lines kept for each text file.
const DEFAULT_MAX_LINES_PER_FILE: usize = 400;
/// Bytes shown when previewing a binary file.
const BINARY_PREVIEW_BYTES: usize = 64;
/// Files that must never appear in CONTEXT.md.
const SKIP_BASENAMES: &[&str] = &[".gitignore", ".aiignore", "CONTEXT.md"];

/// Command-line interface definition.
#[derive(Parser)]
#[command(name = "za", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate CONTEXT.md
    Gen {
        /// Max lines per text file snippet.
        #[arg(long, default_value_t = DEFAULT_MAX_LINES_PER_FILE)]
        max_lines: usize,
        /// Output path.
        #[arg(long, default_value = "CONTEXT.md")]
        output: PathBuf,
        /// Include a short hex preview for binary / non-UTF-8 files.
        #[arg(long)]
        include_binary: bool,
    },
    /// Placeholder for diff.
    Diff {},
    /// Placeholder for stats.
    Stats {},
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Commands::Gen {
            max_lines,
            output,
            include_binary,
        } => generate_context(max_lines, output, include_binary)?,
        Commands::Diff {} => todo!("diff feature not implemented yet"),
        Commands::Stats {} => todo!("stats feature not implemented yet"),
    }
    Ok(())
}

/// Main entry to generate CONTEXT.md.
fn generate_context(max_lines: usize, output_path: PathBuf, include_binary: bool) -> Result<()> {
    let ctx = scan_project(max_lines, &output_path, include_binary)?;
    render_markdown(&ctx, &output_path)?;
    println!("✅ Generated {}", output_path.display());
    Ok(())
}

/// A summarized file entry.
#[derive(Debug)]
struct FileSummary {
    rel_path: PathBuf,
    total_lines: usize,
    snippet: String,
}

/// The overall project context object.
#[derive(Debug)]
struct ProjectContext {
    generated_at: SystemTime,
    files: Vec<FileSummary>,
}

/// Recursively scan the workspace while respecting .gitignore / .aiignore.
fn scan_project(
    max_lines: usize,
    output_path: &Path,
    include_binary: bool,
) -> Result<ProjectContext> {
    let cwd = std::env::current_dir()?;
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner} scanning: {wide_msg}")?
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );

    let mut files = Vec::new();
    let mut builder = WalkBuilder::new(&cwd);
    builder
        .standard_filters(true) // .gitignore / .ignore / global gitignore
        .hidden(false)
        .add_custom_ignore_filename(".aiignore");

    for result in builder.build() {
        let dent = result?;
        let path = dent.path();

        if !path.is_file() {
            continue;
        }
        if path == output_path {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if SKIP_BASENAMES.contains(&name) {
                continue;
            }
        }

        pb.set_message(path.display().to_string());
        if let Some(summary) = summarize_file(path, &cwd, max_lines, include_binary)? {
            files.push(summary);
        }
    }
    pb.finish_with_message("scan complete");

    Ok(ProjectContext {
        generated_at: SystemTime::now(),
        files,
    })
}

/// Build a snippet for the given file.
/// Returns `Ok(None)` when the file should be skipped.
fn summarize_file(
    path: &Path,
    root: &Path,
    max: usize,
    include_binary: bool,
) -> Result<Option<FileSummary>> {
    let bytes = fs::read(path)?;

    // Try UTF-8 fast-path.
    if let Ok(content) = std::str::from_utf8(&bytes) {
        return Ok(Some(build_text_summary(content, path, root, max)));
    }

    // Not valid UTF-8: either skip or produce a hex preview.
    if include_binary {
        let preview_len = bytes.len().min(BINARY_PREVIEW_BYTES);
        let hex_preview = hex::encode(&bytes[..preview_len]);
        let snippet = format!(
            "(binary) first {} bytes (hex):\n{}\n⋯",
            preview_len, hex_preview
        );
        return Ok(Some(FileSummary {
            rel_path: path.strip_prefix(root)?.to_path_buf(),
            total_lines: 0,
            snippet,
        }));
    }

    // Skip binary file.
    Ok(None)
}

/// Helper to build text file summary.
fn build_text_summary(content: &str, path: &Path, root: &Path, max: usize) -> FileSummary {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let snippet = if total <= max {
        content.to_owned()
    } else {
        format!(
            "{}\n⋯(truncated)\n{}",
            lines[..max / 2].join("\n"),
            lines[total - (max / 2)..].join("\n")
        )
    };

    FileSummary {
        rel_path: path.strip_prefix(root).unwrap().to_path_buf(),
        total_lines: total,
        snippet,
    }
}

/// Render the context object into Markdown.
fn render_markdown(ctx: &ProjectContext, output: &Path) -> io::Result<()> {
    let mut file = File::create(output)?;
    let gen_time = humantime::format_rfc3339_seconds(ctx.generated_at);

    writeln!(file, "# 📚 Project Context — generated by za")?;
    writeln!(file, "_Generated at: {}_\n", gen_time)?;

    writeln!(file, "## 1. Directory Overview\n")?;
    writeln!(file, "```text")?;
    for f in &ctx.files {
        let count = if f.total_lines == 0 {
            "bin".to_string()
        } else {
            format!("{}", f.total_lines)
        };
        writeln!(file, "({:<4}) {}", count, f.rel_path.display())?;
    }
    writeln!(file, "```")?;

    writeln!(file, "\n## 2. File Snippets\n")?;
    for f in &ctx.files {
        let ext = path_extension(&f.rel_path);
        writeln!(
            file,
            "<details><summary>{}</summary>\n\n```{}\n{}\n```\n</details>\n",
            f.rel_path.display(),
            ext,
            f.snippet
        )?;
    }
    Ok(())
}

/// Guess fenced-code language from file extension.
fn path_extension(path: &Path) -> &'static str {
    match path.extension().and_then(OsStr::to_str) {
        Some("rs") => "rust",
        Some("go") => "go",
        Some("py") => "python",
        Some("ts") => "typescript",
        Some("js") => "javascript",
        Some("toml") => "toml",
        Some("yml" | "yaml") => "yaml",
        Some("md") => "markdown",
        _ => "",
    }
}
