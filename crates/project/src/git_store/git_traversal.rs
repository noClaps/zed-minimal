use collections::HashMap;
use git::status::GitSummary;
use std::{ops::Deref, path::Path};
use sum_tree::Cursor;
use text::Bias;
use worktree::{Entry, PathProgress, PathTarget, Traversal};

use super::{RepositoryId, RepositorySnapshot, StatusEntry};

/// Walks the worktree entries and their associated git statuses.
pub struct GitTraversal<'a> {
    traversal: Traversal<'a>,
    current_entry_summary: Option<GitSummary>,
    repo_snapshots: &'a HashMap<RepositoryId, RepositorySnapshot>,
    repo_location: Option<(RepositoryId, Cursor<'a, StatusEntry, PathProgress<'a>>)>,
}

impl<'a> GitTraversal<'a> {
    pub fn new(
        repo_snapshots: &'a HashMap<RepositoryId, RepositorySnapshot>,
        traversal: Traversal<'a>,
    ) -> GitTraversal<'a> {
        let mut this = GitTraversal {
            traversal,
            repo_snapshots,
            current_entry_summary: None,
            repo_location: None,
        };
        this.synchronize_statuses(true);
        this
    }

    fn synchronize_statuses(&mut self, reset: bool) {
        self.current_entry_summary = None;

        let Some(entry) = self.entry() else {
            return;
        };

        let Ok(abs_path) = self.traversal.snapshot().absolutize(&entry.path) else {
            self.repo_location = None;
            return;
        };

        let Some((repo, repo_path)) = self
            .repo_snapshots
            .values()
            .filter_map(|repo_snapshot| {
                let repo_path = repo_snapshot.abs_path_to_repo_path(&abs_path)?;
                Some((repo_snapshot, repo_path))
            })
            .max_by_key(|(repo, _)| repo.work_directory_abs_path.clone())
        else {
            self.repo_location = None;
            return;
        };

        // Update our state if we changed repositories.
        if reset
            || self
                .repo_location
                .as_ref()
                .map(|(prev_repo_id, _)| *prev_repo_id)
                != Some(repo.id)
        {
            self.repo_location = Some((repo.id, repo.statuses_by_path.cursor::<PathProgress>(&())));
        }

        let Some((_, statuses)) = &mut self.repo_location else {
            return;
        };

        if entry.is_dir() {
            let mut statuses = statuses.clone();
            statuses.seek_forward(&PathTarget::Path(repo_path.as_ref()), Bias::Left, &());
            let summary =
                statuses.summary(&PathTarget::Successor(repo_path.as_ref()), Bias::Left, &());

            self.current_entry_summary = Some(summary);
        } else if entry.is_file() {
            // For a file entry, park the cursor on the corresponding status
            if statuses.seek_forward(&PathTarget::Path(repo_path.as_ref()), Bias::Left, &()) {
                // TODO: Investigate statuses.item() being None here.
                self.current_entry_summary = statuses.item().map(|item| item.status.into());
            } else {
                self.current_entry_summary = Some(GitSummary::UNCHANGED);
            }
        }
    }

    pub fn advance(&mut self) -> bool {
        self.advance_by(1)
    }

    pub fn advance_by(&mut self, count: usize) -> bool {
        let found = self.traversal.advance_by(count);
        self.synchronize_statuses(false);
        found
    }

    pub fn advance_to_sibling(&mut self) -> bool {
        let found = self.traversal.advance_to_sibling();
        self.synchronize_statuses(false);
        found
    }

    pub fn back_to_parent(&mut self) -> bool {
        let found = self.traversal.back_to_parent();
        self.synchronize_statuses(true);
        found
    }

    pub fn start_offset(&self) -> usize {
        self.traversal.start_offset()
    }

    pub fn end_offset(&self) -> usize {
        self.traversal.end_offset()
    }

    pub fn entry(&self) -> Option<GitEntryRef<'a>> {
        let entry = self.traversal.entry()?;
        let git_summary = self.current_entry_summary.unwrap_or(GitSummary::UNCHANGED);
        Some(GitEntryRef { entry, git_summary })
    }
}

impl<'a> Iterator for GitTraversal<'a> {
    type Item = GitEntryRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(item) = self.entry() {
            self.advance();
            Some(item)
        } else {
            None
        }
    }
}

pub struct ChildEntriesGitIter<'a> {
    parent_path: &'a Path,
    traversal: GitTraversal<'a>,
}

impl<'a> ChildEntriesGitIter<'a> {
    pub fn new(
        repo_snapshots: &'a HashMap<RepositoryId, RepositorySnapshot>,
        worktree_snapshot: &'a worktree::Snapshot,
        parent_path: &'a Path,
    ) -> Self {
        let mut traversal = GitTraversal::new(
            repo_snapshots,
            worktree_snapshot.traverse_from_path(true, true, true, parent_path),
        );
        traversal.advance();
        ChildEntriesGitIter {
            parent_path,
            traversal,
        }
    }
}

impl<'a> Iterator for ChildEntriesGitIter<'a> {
    type Item = GitEntryRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(item) = self.traversal.entry() {
            if item.path.starts_with(self.parent_path) {
                self.traversal.advance_to_sibling();
                return Some(item);
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GitEntryRef<'a> {
    pub entry: &'a Entry,
    pub git_summary: GitSummary,
}

impl GitEntryRef<'_> {
    pub fn to_owned(&self) -> GitEntry {
        GitEntry {
            entry: self.entry.clone(),
            git_summary: self.git_summary,
        }
    }
}

impl Deref for GitEntryRef<'_> {
    type Target = Entry;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

impl AsRef<Entry> for GitEntryRef<'_> {
    fn as_ref(&self) -> &Entry {
        self.entry
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitEntry {
    pub entry: Entry,
    pub git_summary: GitSummary,
}

impl GitEntry {
    pub fn to_ref(&self) -> GitEntryRef {
        GitEntryRef {
            entry: &self.entry,
            git_summary: self.git_summary,
        }
    }
}

impl Deref for GitEntry {
    type Target = Entry;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

impl AsRef<Entry> for GitEntry {
    fn as_ref(&self) -> &Entry {
        &self.entry
    }
}
