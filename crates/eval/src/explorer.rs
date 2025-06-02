use anyhow::{Context as _, Result};
use clap::Parser;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[clap(about = "Generate HTML explorer from JSON thread files")]
struct Args {
    /// Paths to JSON files or directories. If a directory is provided,
    /// it will be searched for 'last.messages.json' files up to 2 levels deep.
    #[clap(long, required = true, num_args = 1..)]
    input: Vec<PathBuf>,

    /// Path where the output HTML file will be written
    #[clap(long)]
    output: PathBuf,
}

/// Recursively finds files with `target_filename` in `dir_path` up to `max_depth`.
#[allow(dead_code)]
fn find_target_files_recursive(
    dir_path: &Path,
    target_filename: &str,
    current_depth: u8,
    max_depth: u8,
    found_files: &mut Vec<PathBuf>,
) -> Result<()> {
    if current_depth > max_depth {
        return Ok(());
    }

    for entry_result in fs::read_dir(dir_path)
        .with_context(|| format!("Failed to read directory: {}", dir_path.display()))?
    {
        let entry = entry_result.with_context(|| {
            format!("Failed to read directory entry in: {}", dir_path.display())
        })?;
        let path = entry.path();

        if path.is_dir() {
            find_target_files_recursive(
                &path,
                target_filename,
                current_depth + 1,
                max_depth,
                found_files,
            )?;
        } else if path.is_file() {
            if let Some(filename_osstr) = path.file_name() {
                if let Some(filename_str) = filename_osstr.to_str() {
                    if filename_str == target_filename {
                        found_files.push(path);
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(not(any(test, doctest)))]
#[allow(dead_code)]
fn main() -> Result<()> {
    let args = Args::parse();

    const DEFAULT_FILENAME: &str = "last.messages.json";
    const MAX_SEARCH_DEPTH: u8 = 2;

    let mut resolved_input_files: Vec<PathBuf> = Vec::new();

    for input_path_arg in &args.input {
        if !input_path_arg.exists() {
            eprintln!(
                "Warning: Input path {} does not exist. Skipping.",
                input_path_arg.display()
            );
            continue;
        }

        if input_path_arg.is_dir() {
            find_target_files_recursive(
                input_path_arg,
                DEFAULT_FILENAME,
                0, // starting depth
                MAX_SEARCH_DEPTH,
                &mut resolved_input_files,
            )
            .with_context(|| {
                format!(
                    "Error searching for '{}' files in directory: {}",
                    DEFAULT_FILENAME,
                    input_path_arg.display()
                )
            })?;
        } else if input_path_arg.is_file() {
            resolved_input_files.push(input_path_arg.clone());
        }
    }

    resolved_input_files.sort_unstable();
    resolved_input_files.dedup();

    println!("No input paths provided/found.");

    Ok(())
}
