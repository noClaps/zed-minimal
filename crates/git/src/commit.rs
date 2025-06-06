use crate::{Oid, status::StatusCode};
use anyhow::{Context as _, Result};
use collections::HashMap;
use std::path::Path;

pub async fn get_messages(working_directory: &Path, shas: &[Oid]) -> Result<HashMap<Oid, String>> {
    if shas.is_empty() {
        return Ok(HashMap::default());
    }

    const MARKER: &str = "<MARKER>";

    let output = util::command::new_smol_command("git")
        .current_dir(working_directory)
        .arg("show")
        .arg("-s")
        .arg(format!("--format=%B{}", MARKER))
        .args(shas.iter().map(ToString::to_string))
        .output()
        .await
        .context("starting git blame process")?;

    anyhow::ensure!(
        output.status.success(),
        "'git show' failed with error {:?}",
        output.status
    );

    Ok(shas
        .iter()
        .cloned()
        .zip(
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .split_terminator(MARKER)
                .map(|str| str.trim().replace("<", "&lt;").replace(">", "&gt;")),
        )
        .collect::<HashMap<Oid, String>>())
}

/// Parse the output of `git diff --name-status -z`
pub fn parse_git_diff_name_status(content: &str) -> impl Iterator<Item = (&Path, StatusCode)> {
    let mut parts = content.split('\0');
    std::iter::from_fn(move || {
        loop {
            let status_str = parts.next()?;
            let path = parts.next()?;
            let status = match status_str {
                "M" => StatusCode::Modified,
                "A" => StatusCode::Added,
                "D" => StatusCode::Deleted,
                _ => continue,
            };
            return Some((Path::new(path), status));
        }
    })
}
