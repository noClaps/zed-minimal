use crate::Oid;
use crate::commit::get_messages;
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use futures::AsyncWriteExt;
use gpui::SharedString;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::{ops::Range, path::Path};
use text::Rope;
use time::OffsetDateTime;
use time::UtcOffset;
use time::macros::format_description;

pub use git2 as libgit;

#[derive(Debug, Clone, Default)]
pub struct Blame {
    pub entries: Vec<BlameEntry>,
    pub messages: HashMap<Oid, String>,
    pub remote_url: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ParsedCommitMessage {
    pub message: SharedString,
    pub permalink: Option<url::Url>,
    pub pull_request: Option<crate::hosting_provider::PullRequest>,
}

impl Blame {
    pub async fn for_path(
        git_binary: &Path,
        working_directory: &Path,
        path: &Path,
        content: &Rope,
        remote_url: Option<String>,
    ) -> Result<Self> {
        let output = run_git_blame(git_binary, working_directory, path, content).await?;
        let mut entries = parse_git_blame(&output)?;
        entries.sort_unstable_by(|a, b| a.range.start.cmp(&b.range.start));

        let mut unique_shas = HashSet::default();

        for entry in entries.iter_mut() {
            unique_shas.insert(entry.sha);
        }

        let shas = unique_shas.into_iter().collect::<Vec<_>>();
        let messages = get_messages(working_directory, &shas)
            .await
            .context("failed to get commit messages")?;

        Ok(Self {
            entries,
            messages,
            remote_url,
        })
    }
}

const GIT_BLAME_NO_COMMIT_ERROR: &str = "fatal: no such ref: HEAD";
const GIT_BLAME_NO_PATH: &str = "fatal: no such path";

async fn run_git_blame(
    git_binary: &Path,
    working_directory: &Path,
    path: &Path,
    contents: &Rope,
) -> Result<String> {
    let mut child = util::command::new_smol_command(git_binary)
        .current_dir(working_directory)
        .arg("blame")
        .arg("--incremental")
        .arg("--contents")
        .arg("-")
        .arg(path.as_os_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting git blame process")?;

    let stdin = child
        .stdin
        .as_mut()
        .context("failed to get pipe to stdin of git blame command")?;

    for chunk in contents.chunks() {
        stdin.write_all(chunk.as_bytes()).await?;
    }
    stdin.flush().await?;

    let output = child.output().await.context("reading git blame output")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let trimmed = stderr.trim();
        if trimmed == GIT_BLAME_NO_COMMIT_ERROR || trimmed.contains(GIT_BLAME_NO_PATH) {
            return Ok(String::new());
        }
        anyhow::bail!("git blame process failed: {stderr}");
    }

    Ok(String::from_utf8(output.stdout)?)
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
pub struct BlameEntry {
    pub sha: Oid,

    pub range: Range<u32>,

    pub original_line_number: u32,

    pub author: Option<String>,
    pub author_mail: Option<String>,
    pub author_time: Option<i64>,
    pub author_tz: Option<String>,

    pub committer_name: Option<String>,
    pub committer_email: Option<String>,
    pub committer_time: Option<i64>,
    pub committer_tz: Option<String>,

    pub summary: Option<String>,

    pub previous: Option<String>,
    pub filename: String,
}

impl BlameEntry {
    // Returns a BlameEntry by parsing the first line of a `git blame --incremental`
    // entry. The line MUST have this format:
    //
    //     <40-byte-hex-sha1> <sourceline> <resultline> <num-lines>
    fn new_from_blame_line(line: &str) -> Result<BlameEntry> {
        let mut parts = line.split_whitespace();

        let sha = parts
            .next()
            .and_then(|line| line.parse::<Oid>().ok())
            .context("parsing sha")?;

        let original_line_number = parts
            .next()
            .and_then(|line| line.parse::<u32>().ok())
            .context("parsing original line number")?;
        let final_line_number = parts
            .next()
            .and_then(|line| line.parse::<u32>().ok())
            .context("parsing final line number")?;

        let line_count = parts
            .next()
            .and_then(|line| line.parse::<u32>().ok())
            .context("parsing line count")?;

        let start_line = final_line_number.saturating_sub(1);
        let end_line = start_line + line_count;
        let range = start_line..end_line;

        Ok(Self {
            sha,
            range,
            original_line_number,
            ..Default::default()
        })
    }

    pub fn author_offset_date_time(&self) -> Result<time::OffsetDateTime> {
        if let (Some(author_time), Some(author_tz)) = (self.author_time, &self.author_tz) {
            let format = format_description!("[offset_hour][offset_minute]");
            let offset = UtcOffset::parse(author_tz, &format)?;
            let date_time_utc = OffsetDateTime::from_unix_timestamp(author_time)?;

            Ok(date_time_utc.to_offset(offset))
        } else {
            // Directly return current time in UTC if there's no committer time or timezone
            Ok(time::OffsetDateTime::now_utc())
        }
    }
}

// parse_git_blame parses the output of `git blame --incremental`, which returns
// all the blame-entries for a given path incrementally, as it finds them.
//
// Each entry *always* starts with:
//
//     <40-byte-hex-sha1> <sourceline> <resultline> <num-lines>
//
// Each entry *always* ends with:
//
//     filename <whitespace-quoted-filename-goes-here>
//
// Line numbers are 1-indexed.
//
// A `git blame --incremental` entry looks like this:
//
//    6ad46b5257ba16d12c5ca9f0d4900320959df7f4 2 2 1
//    author Joe Schmoe
//    author-mail <joe.schmoe@example.com>
//    author-time 1709741400
//    author-tz +0100
//    committer Joe Schmoe
//    committer-mail <joe.schmoe@example.com>
//    committer-time 1709741400
//    committer-tz +0100
//    summary Joe's cool commit
//    previous 486c2409237a2c627230589e567024a96751d475 index.js
//    filename index.js
//
// If the entry has the same SHA as an entry that was already printed then no
// signature information is printed:
//
//    6ad46b5257ba16d12c5ca9f0d4900320959df7f4 3 4 1
//    previous 486c2409237a2c627230589e567024a96751d475 index.js
//    filename index.js
//
// More about `--incremental` output: https://mirrors.edge.kernel.org/pub/software/scm/git/docs/git-blame.html
fn parse_git_blame(output: &str) -> Result<Vec<BlameEntry>> {
    let mut entries: Vec<BlameEntry> = Vec::new();
    let mut index: HashMap<Oid, usize> = HashMap::default();

    let mut current_entry: Option<BlameEntry> = None;

    for line in output.lines() {
        let mut done = false;

        match &mut current_entry {
            None => {
                let mut new_entry = BlameEntry::new_from_blame_line(line)?;

                if let Some(existing_entry) = index
                    .get(&new_entry.sha)
                    .and_then(|slot| entries.get(*slot))
                {
                    new_entry.author.clone_from(&existing_entry.author);
                    new_entry
                        .author_mail
                        .clone_from(&existing_entry.author_mail);
                    new_entry.author_time = existing_entry.author_time;
                    new_entry.author_tz.clone_from(&existing_entry.author_tz);
                    new_entry
                        .committer_name
                        .clone_from(&existing_entry.committer_name);
                    new_entry
                        .committer_email
                        .clone_from(&existing_entry.committer_email);
                    new_entry.committer_time = existing_entry.committer_time;
                    new_entry
                        .committer_tz
                        .clone_from(&existing_entry.committer_tz);
                    new_entry.summary.clone_from(&existing_entry.summary);
                }

                current_entry.replace(new_entry);
            }
            Some(entry) => {
                let Some((key, value)) = line.split_once(' ') else {
                    continue;
                };
                let is_committed = !entry.sha.is_zero();
                match key {
                    "filename" => {
                        entry.filename = value.into();
                        done = true;
                    }
                    "previous" => entry.previous = Some(value.into()),

                    "summary" if is_committed => entry.summary = Some(value.into()),
                    "author" if is_committed => entry.author = Some(value.into()),
                    "author-mail" if is_committed => entry.author_mail = Some(value.into()),
                    "author-time" if is_committed => {
                        entry.author_time = Some(value.parse::<i64>()?)
                    }
                    "author-tz" if is_committed => entry.author_tz = Some(value.into()),

                    "committer" if is_committed => entry.committer_name = Some(value.into()),
                    "committer-mail" if is_committed => entry.committer_email = Some(value.into()),
                    "committer-time" if is_committed => {
                        entry.committer_time = Some(value.parse::<i64>()?)
                    }
                    "committer-tz" if is_committed => entry.committer_tz = Some(value.into()),
                    _ => {}
                }
            }
        };

        if done {
            if let Some(entry) = current_entry.take() {
                index.insert(entry.sha, entries.len());

                // We only want annotations that have a commit.
                if !entry.sha.is_zero() {
                    entries.push(entry);
                }
            }
        }
    }

    Ok(entries)
}
