use anyhow::Result;
use project::DiagnosticSummary;
use serde::{Deserialize, Serialize};

use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use util::command::new_smol_command;

use crate::ToolMetrics;
use crate::assertions::AssertionsReport;
use crate::example::Example;

pub const ZED_REPO_URL: &str = "https://github.com/zed-industries/zed.git";

#[derive(Clone)]
pub struct ExampleInstance {
    pub thread: Rc<dyn Example>,
    pub name: String,
    pub run_directory: PathBuf,
    pub log_prefix: String,
    /// The repetition number for this example (0-based)
    /// When running multiple repetitions of the same example, each instance is assigned a unique repetition number.
    /// This affects the worktree path and log prefix to avoid clobbering results between runs.
    pub repetition: usize,
    pub repo_path: PathBuf,
    /// Path to the directory containing the requests and responses for the agentic loop
    worktrees_dir: PathBuf,
}

#[derive(Debug, Serialize, Clone)]
pub struct RunOutput {
    pub repository_diff: String,
    pub diagnostic_summary_before: DiagnosticSummary,
    pub diagnostic_summary_after: DiagnosticSummary,
    pub diagnostics_before: Option<String>,
    pub diagnostics_after: Option<String>,
    pub response_count: usize,
    pub tool_metrics: ToolMetrics,
    pub all_messages: String,
    pub programmatic_assertions: AssertionsReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeDiffInput {
    pub repository_diff: String,
    pub assertion: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeThreadInput {
    pub messages: String,
    pub assertion: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeOutput {
    pub thread: AssertionsReport,
    pub diff: AssertionsReport,
}

impl ExampleInstance {
    pub fn new(
        thread: Rc<dyn Example>,
        repos_dir: &Path,
        run_dir: &Path,
        worktrees_dir: &Path,
        repetition: usize,
    ) -> Self {
        let name = thread.meta().name.to_string();
        let run_directory = run_dir
            .join(&name)
            .join(repetition.to_string())
            .to_path_buf();

        let repo_path = repo_path_for_url(repos_dir, &thread.meta().url);

        Self {
            name,
            thread,
            log_prefix: String::new(),
            run_directory,
            repetition,
            repo_path,
            worktrees_dir: worktrees_dir.to_path_buf(),
        }
    }

    pub fn repo_url(&self) -> String {
        self.thread.meta().url
    }

    pub fn worktree_name(&self) -> String {
        format!("{}-{}", self.name, self.repetition)
    }

    pub fn set_log_prefix_style(&mut self, color: &str, name_width: usize) {
        self.log_prefix = format!(
            "{}{:<width$}\x1b[0m | ",
            color,
            self.worktree_name(),
            width = name_width
        );
    }

    /// Set up the example by checking out the specified Git revision
    pub async fn fetch(&mut self) -> Result<()> {
        let meta = self.thread.meta();

        let revision_exists = run_git(
            &self.repo_path,
            &["rev-parse", &format!("{}^{{commit}}", &meta.revision)],
        )
        .await
        .is_ok();

        if !revision_exists {
            println!("{}Fetching revision {}", self.log_prefix, &meta.revision);
            run_git(
                &self.repo_path,
                &["fetch", "--depth", "1", "origin", &meta.revision],
            )
            .await?;
        }
        Ok(())
    }

    /// Set up the example by checking out the specified Git revision
    pub async fn setup(&mut self) -> Result<()> {
        let worktree_path = self.worktree_path();
        let meta = self.thread.meta();
        if worktree_path.is_dir() {
            println!("{}Resetting existing worktree", self.log_prefix);

            // TODO: consider including "-x" to remove ignored files. The downside of this is that
            // it will also remove build artifacts, and so prevent incremental reuse there.
            run_git(&worktree_path, &["clean", "--force", "-d"]).await?;
            run_git(&worktree_path, &["reset", "--hard", "HEAD"]).await?;
            run_git(&worktree_path, &["checkout", &meta.revision]).await?;
        } else {
            println!("{}Creating worktree", self.log_prefix);

            let worktree_path_string = worktree_path.to_string_lossy().to_string();

            run_git(
                &self.repo_path,
                &[
                    "worktree",
                    "add",
                    "-f",
                    &worktree_path_string,
                    &meta.revision,
                ],
            )
            .await?;
        }

        if meta.url == ZED_REPO_URL {
            std::fs::write(worktree_path.join(".rules"), std::fs::read(".rules")?)?;
        }

        std::fs::create_dir_all(&self.run_directory)?;

        Ok(())
    }

    pub fn worktree_path(&self) -> PathBuf {
        self.worktrees_dir
            .join(self.worktree_name())
            .join(self.thread.meta().repo_name())
    }
}

pub fn repo_path_for_url(repos_dir: &Path, repo_url: &str) -> PathBuf {
    let repo_name = repo_url
        .trim_start_matches("https://")
        .replace(|c: char| !c.is_alphanumeric(), "-");
    Path::new(repos_dir).join(repo_name)
}

pub async fn run_git(repo_path: &Path, args: &[&str]) -> Result<String> {
    let output = new_smol_command("git")
        .current_dir(repo_path)
        .args(args)
        .output()
        .await?;

    anyhow::ensure!(
        output.status.success(),
        "`git {}` within `{}` failed with status: {}\nstderr:\n{}\nstdout:\n{}",
        args.join(" "),
        repo_path.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}
