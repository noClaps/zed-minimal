use futures::channel::oneshot;
use git2::{DiffLineType as GitDiffLineType, DiffOptions as GitOptions, Patch as GitPatch};
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Task, TaskLabel};
use language::{Language, LanguageRegistry};
use rope::Rope;
use std::{
    cmp::Ordering,
    future::Future,
    iter,
    ops::Range,
    sync::{Arc, LazyLock},
};
use sum_tree::SumTree;
use text::{Anchor, Bias, BufferId, OffsetRangeExt, Point, ToOffset as _};
use util::ResultExt;

pub static CALCULATE_DIFF_TASK: LazyLock<TaskLabel> = LazyLock::new(TaskLabel::new);

pub struct BufferDiff {
    pub buffer_id: BufferId,
    inner: BufferDiffInner,
    secondary_diff: Option<Entity<BufferDiff>>,
}

#[derive(Clone, Debug)]
pub struct BufferDiffSnapshot {
    inner: BufferDiffInner,
    secondary_diff: Option<Box<BufferDiffSnapshot>>,
}

#[derive(Clone)]
struct BufferDiffInner {
    hunks: SumTree<InternalDiffHunk>,
    pending_hunks: SumTree<PendingHunk>,
    base_text: language::BufferSnapshot,
    base_text_exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DiffHunkStatus {
    pub kind: DiffHunkStatusKind,
    pub secondary: DiffHunkSecondaryStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiffHunkStatusKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiffHunkSecondaryStatus {
    HasSecondaryHunk,
    OverlapsWithSecondaryHunk,
    NoSecondaryHunk,
    SecondaryHunkAdditionPending,
    SecondaryHunkRemovalPending,
}

/// A diff hunk resolved to rows in the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// The buffer range as points.
    pub range: Range<Point>,
    /// The range in the buffer to which this hunk corresponds.
    pub buffer_range: Range<Anchor>,
    /// The range in the buffer's diff base text to which this hunk corresponds.
    pub diff_base_byte_range: Range<usize>,
    pub secondary_status: DiffHunkSecondaryStatus,
}

/// We store [`InternalDiffHunk`]s internally so we don't need to store the additional row range.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InternalDiffHunk {
    buffer_range: Range<Anchor>,
    diff_base_byte_range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingHunk {
    buffer_range: Range<Anchor>,
    diff_base_byte_range: Range<usize>,
    buffer_version: clock::Global,
    new_status: DiffHunkSecondaryStatus,
}

#[derive(Debug, Default, Clone)]
pub struct DiffHunkSummary {
    buffer_range: Range<Anchor>,
}

impl sum_tree::Item for InternalDiffHunk {
    type Summary = DiffHunkSummary;

    fn summary(&self, _cx: &text::BufferSnapshot) -> Self::Summary {
        DiffHunkSummary {
            buffer_range: self.buffer_range.clone(),
        }
    }
}

impl sum_tree::Item for PendingHunk {
    type Summary = DiffHunkSummary;

    fn summary(&self, _cx: &text::BufferSnapshot) -> Self::Summary {
        DiffHunkSummary {
            buffer_range: self.buffer_range.clone(),
        }
    }
}

impl sum_tree::Summary for DiffHunkSummary {
    type Context = text::BufferSnapshot;

    fn zero(_cx: &Self::Context) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, other: &Self, buffer: &Self::Context) {
        self.buffer_range.start = self
            .buffer_range
            .start
            .min(&other.buffer_range.start, buffer);
        self.buffer_range.end = self.buffer_range.end.max(&other.buffer_range.end, buffer);
    }
}

impl sum_tree::SeekTarget<'_, DiffHunkSummary, DiffHunkSummary> for Anchor {
    fn cmp(&self, cursor_location: &DiffHunkSummary, buffer: &text::BufferSnapshot) -> Ordering {
        if self
            .cmp(&cursor_location.buffer_range.start, buffer)
            .is_lt()
        {
            Ordering::Less
        } else if self.cmp(&cursor_location.buffer_range.end, buffer).is_gt() {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }
}

impl std::fmt::Debug for BufferDiffInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferDiffSnapshot")
            .field("hunks", &self.hunks)
            .finish()
    }
}

impl BufferDiffSnapshot {
    fn empty(buffer: &text::BufferSnapshot, cx: &mut App) -> BufferDiffSnapshot {
        BufferDiffSnapshot {
            inner: BufferDiffInner {
                base_text: language::Buffer::build_empty_snapshot(cx),
                hunks: SumTree::new(buffer),
                pending_hunks: SumTree::new(buffer),
                base_text_exists: false,
            },
            secondary_diff: None,
        }
    }

    fn new_with_base_text(
        buffer: text::BufferSnapshot,
        base_text: Option<Arc<String>>,
        language: Option<Arc<Language>>,
        language_registry: Option<Arc<LanguageRegistry>>,
        cx: &mut App,
    ) -> impl Future<Output = Self> + use<> {
        let base_text_pair;
        let base_text_exists;
        let base_text_snapshot;
        if let Some(text) = &base_text {
            let base_text_rope = Rope::from(text.as_str());
            base_text_pair = Some((text.clone(), base_text_rope.clone()));
            let snapshot = language::Buffer::build_snapshot(
                base_text_rope,
                language.clone(),
                language_registry.clone(),
                cx,
            );
            base_text_snapshot = cx.background_spawn(snapshot);
            base_text_exists = true;
        } else {
            base_text_pair = None;
            base_text_snapshot = Task::ready(language::Buffer::build_empty_snapshot(cx));
            base_text_exists = false;
        };

        let hunks = cx
            .background_executor()
            .spawn_labeled(*CALCULATE_DIFF_TASK, {
                let buffer = buffer.clone();
                async move { compute_hunks(base_text_pair, buffer) }
            });

        async move {
            let (base_text, hunks) = futures::join!(base_text_snapshot, hunks);
            Self {
                inner: BufferDiffInner {
                    base_text,
                    hunks,
                    base_text_exists,
                    pending_hunks: SumTree::new(&buffer),
                },
                secondary_diff: None,
            }
        }
    }

    pub fn new_with_base_buffer(
        buffer: text::BufferSnapshot,
        base_text: Option<Arc<String>>,
        base_text_snapshot: language::BufferSnapshot,
        cx: &App,
    ) -> impl Future<Output = Self> + use<> {
        let base_text_exists = base_text.is_some();
        let base_text_pair = base_text.map(|text| (text, base_text_snapshot.as_rope().clone()));
        cx.background_executor()
            .spawn_labeled(*CALCULATE_DIFF_TASK, async move {
                Self {
                    inner: BufferDiffInner {
                        base_text: base_text_snapshot,
                        pending_hunks: SumTree::new(&buffer),
                        hunks: compute_hunks(base_text_pair, buffer),
                        base_text_exists,
                    },
                    secondary_diff: None,
                }
            })
    }

    pub fn is_empty(&self) -> bool {
        self.inner.hunks.is_empty()
    }

    pub fn secondary_diff(&self) -> Option<&BufferDiffSnapshot> {
        self.secondary_diff.as_deref()
    }

    pub fn hunks_intersecting_range<'a>(
        &'a self,
        range: Range<Anchor>,
        buffer: &'a text::BufferSnapshot,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        let unstaged_counterpart = self.secondary_diff.as_ref().map(|diff| &diff.inner);
        self.inner
            .hunks_intersecting_range(range, buffer, unstaged_counterpart)
    }

    pub fn hunks_intersecting_range_rev<'a>(
        &'a self,
        range: Range<Anchor>,
        buffer: &'a text::BufferSnapshot,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        self.inner.hunks_intersecting_range_rev(range, buffer)
    }

    pub fn base_text(&self) -> &language::BufferSnapshot {
        &self.inner.base_text
    }

    pub fn base_texts_eq(&self, other: &Self) -> bool {
        self.inner.base_text_exists == other.inner.base_text_exists
    }
}

impl BufferDiffInner {
    /// Returns the new index text and new pending hunks.
    fn stage_or_unstage_hunks_impl(
        &mut self,
        unstaged_diff: &Self,
        stage: bool,
        hunks: &[DiffHunk],
        buffer: &text::BufferSnapshot,
        file_exists: bool,
    ) -> Option<Rope> {
        let head_text = self
            .base_text_exists
            .then(|| self.base_text.as_rope().clone());
        let index_text = unstaged_diff
            .base_text_exists
            .then(|| unstaged_diff.base_text.as_rope().clone());

        // If the file doesn't exist in either HEAD or the index, then the
        // entire file must be either created or deleted in the index.
        let (index_text, head_text) = match (index_text, head_text) {
            (Some(index_text), Some(head_text)) if file_exists || !stage => (index_text, head_text),
            (index_text, head_text) => {
                let (new_index_text, new_status) = if stage {
                    log::debug!("stage all");
                    (
                        file_exists.then(|| buffer.as_rope().clone()),
                        DiffHunkSecondaryStatus::SecondaryHunkRemovalPending,
                    )
                } else {
                    log::debug!("unstage all");
                    (
                        head_text,
                        DiffHunkSecondaryStatus::SecondaryHunkAdditionPending,
                    )
                };

                let hunk = PendingHunk {
                    buffer_range: Anchor::MIN..Anchor::MAX,
                    diff_base_byte_range: 0..index_text.map_or(0, |rope| rope.len()),
                    buffer_version: buffer.version().clone(),
                    new_status,
                };
                self.pending_hunks = SumTree::from_item(hunk, buffer);
                return new_index_text;
            }
        };

        let mut pending_hunks = SumTree::new(buffer);
        let mut old_pending_hunks = self.pending_hunks.cursor::<DiffHunkSummary>(buffer);

        // first, merge new hunks into pending_hunks
        for DiffHunk {
            buffer_range,
            diff_base_byte_range,
            secondary_status,
            ..
        } in hunks.iter().cloned()
        {
            let preceding_pending_hunks =
                old_pending_hunks.slice(&buffer_range.start, Bias::Left, buffer);
            pending_hunks.append(preceding_pending_hunks, buffer);

            // Skip all overlapping or adjacent old pending hunks
            while old_pending_hunks.item().is_some_and(|old_hunk| {
                old_hunk
                    .buffer_range
                    .start
                    .cmp(&buffer_range.end, buffer)
                    .is_le()
            }) {
                old_pending_hunks.next(buffer);
            }

            if (stage && secondary_status == DiffHunkSecondaryStatus::NoSecondaryHunk)
                || (!stage && secondary_status == DiffHunkSecondaryStatus::HasSecondaryHunk)
            {
                continue;
            }

            pending_hunks.push(
                PendingHunk {
                    buffer_range,
                    diff_base_byte_range,
                    buffer_version: buffer.version().clone(),
                    new_status: if stage {
                        DiffHunkSecondaryStatus::SecondaryHunkRemovalPending
                    } else {
                        DiffHunkSecondaryStatus::SecondaryHunkAdditionPending
                    },
                },
                buffer,
            );
        }
        // append the remainder
        pending_hunks.append(old_pending_hunks.suffix(buffer), buffer);

        let mut unstaged_hunk_cursor = unstaged_diff.hunks.cursor::<DiffHunkSummary>(buffer);
        unstaged_hunk_cursor.next(buffer);

        // then, iterate over all pending hunks (both new ones and the existing ones) and compute the edits
        let mut prev_unstaged_hunk_buffer_end = 0;
        let mut prev_unstaged_hunk_base_text_end = 0;
        let mut edits = Vec::<(Range<usize>, String)>::new();
        let mut pending_hunks_iter = pending_hunks.iter().cloned().peekable();
        while let Some(PendingHunk {
            buffer_range,
            diff_base_byte_range,
            new_status,
            ..
        }) = pending_hunks_iter.next()
        {
            // Advance unstaged_hunk_cursor to skip unstaged hunks before current hunk
            let skipped_unstaged =
                unstaged_hunk_cursor.slice(&buffer_range.start, Bias::Left, buffer);

            if let Some(unstaged_hunk) = skipped_unstaged.last() {
                prev_unstaged_hunk_base_text_end = unstaged_hunk.diff_base_byte_range.end;
                prev_unstaged_hunk_buffer_end = unstaged_hunk.buffer_range.end.to_offset(buffer);
            }

            // Find where this hunk is in the index if it doesn't overlap
            let mut buffer_offset_range = buffer_range.to_offset(buffer);
            let start_overshoot = buffer_offset_range.start - prev_unstaged_hunk_buffer_end;
            let mut index_start = prev_unstaged_hunk_base_text_end + start_overshoot;

            loop {
                // Merge this hunk with any overlapping unstaged hunks.
                if let Some(unstaged_hunk) = unstaged_hunk_cursor.item() {
                    let unstaged_hunk_offset_range = unstaged_hunk.buffer_range.to_offset(buffer);
                    if unstaged_hunk_offset_range.start <= buffer_offset_range.end {
                        prev_unstaged_hunk_base_text_end = unstaged_hunk.diff_base_byte_range.end;
                        prev_unstaged_hunk_buffer_end = unstaged_hunk_offset_range.end;

                        index_start = index_start.min(unstaged_hunk.diff_base_byte_range.start);
                        buffer_offset_range.start = buffer_offset_range
                            .start
                            .min(unstaged_hunk_offset_range.start);
                        buffer_offset_range.end =
                            buffer_offset_range.end.max(unstaged_hunk_offset_range.end);

                        unstaged_hunk_cursor.next(buffer);
                        continue;
                    }
                }

                // If any unstaged hunks were merged, then subsequent pending hunks may
                // now overlap this hunk. Merge them.
                if let Some(next_pending_hunk) = pending_hunks_iter.peek() {
                    let next_pending_hunk_offset_range =
                        next_pending_hunk.buffer_range.to_offset(buffer);
                    if next_pending_hunk_offset_range.start <= buffer_offset_range.end {
                        buffer_offset_range.end = next_pending_hunk_offset_range.end;
                        pending_hunks_iter.next();
                        continue;
                    }
                }

                break;
            }

            let end_overshoot = buffer_offset_range
                .end
                .saturating_sub(prev_unstaged_hunk_buffer_end);
            let index_end = prev_unstaged_hunk_base_text_end + end_overshoot;
            let index_byte_range = index_start..index_end;

            let replacement_text = match new_status {
                DiffHunkSecondaryStatus::SecondaryHunkRemovalPending => {
                    log::debug!("staging hunk {:?}", buffer_offset_range);
                    buffer
                        .text_for_range(buffer_offset_range)
                        .collect::<String>()
                }
                DiffHunkSecondaryStatus::SecondaryHunkAdditionPending => {
                    log::debug!("unstaging hunk {:?}", buffer_offset_range);
                    head_text
                        .chunks_in_range(diff_base_byte_range.clone())
                        .collect::<String>()
                }
                _ => {
                    debug_assert!(false);
                    continue;
                }
            };

            edits.push((index_byte_range, replacement_text));
        }
        drop(pending_hunks_iter);
        drop(old_pending_hunks);
        self.pending_hunks = pending_hunks;

        let mut new_index_text = Rope::new();
        let mut index_cursor = index_text.cursor(0);

        for (old_range, replacement_text) in edits {
            new_index_text.append(index_cursor.slice(old_range.start));
            index_cursor.seek_forward(old_range.end);
            new_index_text.push(&replacement_text);
        }
        new_index_text.append(index_cursor.suffix());
        Some(new_index_text)
    }

    fn hunks_intersecting_range<'a>(
        &'a self,
        range: Range<Anchor>,
        buffer: &'a text::BufferSnapshot,
        secondary: Option<&'a Self>,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        let range = range.to_offset(buffer);

        let mut cursor = self
            .hunks
            .filter::<_, DiffHunkSummary>(buffer, move |summary| {
                let summary_range = summary.buffer_range.to_offset(buffer);
                let before_start = summary_range.end < range.start;
                let after_end = summary_range.start > range.end;
                !before_start && !after_end
            });

        let anchor_iter = iter::from_fn(move || {
            cursor.next(buffer);
            cursor.item()
        })
        .flat_map(move |hunk| {
            [
                (
                    &hunk.buffer_range.start,
                    (hunk.buffer_range.start, hunk.diff_base_byte_range.start),
                ),
                (
                    &hunk.buffer_range.end,
                    (hunk.buffer_range.end, hunk.diff_base_byte_range.end),
                ),
            ]
        });

        let mut pending_hunks_cursor = self.pending_hunks.cursor::<DiffHunkSummary>(buffer);
        pending_hunks_cursor.next(buffer);

        let mut secondary_cursor = None;
        if let Some(secondary) = secondary.as_ref() {
            let mut cursor = secondary.hunks.cursor::<DiffHunkSummary>(buffer);
            cursor.next(buffer);
            secondary_cursor = Some(cursor);
        }

        let max_point = buffer.max_point();
        let mut summaries = buffer.summaries_for_anchors_with_payload::<Point, _, _>(anchor_iter);
        iter::from_fn(move || {
            loop {
                let (start_point, (start_anchor, start_base)) = summaries.next()?;
                let (mut end_point, (mut end_anchor, end_base)) = summaries.next()?;

                if !start_anchor.is_valid(buffer) {
                    continue;
                }

                if end_point.column > 0 && end_point < max_point {
                    end_point.row += 1;
                    end_point.column = 0;
                    end_anchor = buffer.anchor_before(end_point);
                }

                let mut secondary_status = DiffHunkSecondaryStatus::NoSecondaryHunk;

                let mut has_pending = false;
                if start_anchor
                    .cmp(&pending_hunks_cursor.start().buffer_range.start, buffer)
                    .is_gt()
                {
                    pending_hunks_cursor.seek_forward(&start_anchor, Bias::Left, buffer);
                }

                if let Some(pending_hunk) = pending_hunks_cursor.item() {
                    let mut pending_range = pending_hunk.buffer_range.to_point(buffer);
                    if pending_range.end.column > 0 {
                        pending_range.end.row += 1;
                        pending_range.end.column = 0;
                    }

                    if pending_range == (start_point..end_point) {
                        if !buffer.has_edits_since_in_range(
                            &pending_hunk.buffer_version,
                            start_anchor..end_anchor,
                        ) {
                            has_pending = true;
                            secondary_status = pending_hunk.new_status;
                        }
                    }
                }

                if let (Some(secondary_cursor), false) = (secondary_cursor.as_mut(), has_pending) {
                    if start_anchor
                        .cmp(&secondary_cursor.start().buffer_range.start, buffer)
                        .is_gt()
                    {
                        secondary_cursor.seek_forward(&start_anchor, Bias::Left, buffer);
                    }

                    if let Some(secondary_hunk) = secondary_cursor.item() {
                        let mut secondary_range = secondary_hunk.buffer_range.to_point(buffer);
                        if secondary_range.end.column > 0 {
                            secondary_range.end.row += 1;
                            secondary_range.end.column = 0;
                        }
                        if secondary_range.is_empty()
                            && secondary_hunk.diff_base_byte_range.is_empty()
                        {
                            // ignore
                        } else if secondary_range == (start_point..end_point) {
                            secondary_status = DiffHunkSecondaryStatus::HasSecondaryHunk;
                        } else if secondary_range.start <= end_point {
                            secondary_status = DiffHunkSecondaryStatus::OverlapsWithSecondaryHunk;
                        }
                    }
                }

                return Some(DiffHunk {
                    range: start_point..end_point,
                    diff_base_byte_range: start_base..end_base,
                    buffer_range: start_anchor..end_anchor,
                    secondary_status,
                });
            }
        })
    }

    fn hunks_intersecting_range_rev<'a>(
        &'a self,
        range: Range<Anchor>,
        buffer: &'a text::BufferSnapshot,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        let mut cursor = self
            .hunks
            .filter::<_, DiffHunkSummary>(buffer, move |summary| {
                let before_start = summary.buffer_range.end.cmp(&range.start, buffer).is_lt();
                let after_end = summary.buffer_range.start.cmp(&range.end, buffer).is_gt();
                !before_start && !after_end
            });

        iter::from_fn(move || {
            cursor.prev(buffer);

            let hunk = cursor.item()?;
            let range = hunk.buffer_range.to_point(buffer);

            Some(DiffHunk {
                range,
                diff_base_byte_range: hunk.diff_base_byte_range.clone(),
                buffer_range: hunk.buffer_range.clone(),
                // The secondary status is not used by callers of this method.
                secondary_status: DiffHunkSecondaryStatus::NoSecondaryHunk,
            })
        })
    }

    fn compare(&self, old: &Self, new_snapshot: &text::BufferSnapshot) -> Option<Range<Anchor>> {
        let mut new_cursor = self.hunks.cursor::<()>(new_snapshot);
        let mut old_cursor = old.hunks.cursor::<()>(new_snapshot);
        old_cursor.next(new_snapshot);
        new_cursor.next(new_snapshot);
        let mut start = None;
        let mut end = None;

        loop {
            match (new_cursor.item(), old_cursor.item()) {
                (Some(new_hunk), Some(old_hunk)) => {
                    match new_hunk
                        .buffer_range
                        .start
                        .cmp(&old_hunk.buffer_range.start, new_snapshot)
                    {
                        Ordering::Less => {
                            start.get_or_insert(new_hunk.buffer_range.start);
                            end.replace(new_hunk.buffer_range.end);
                            new_cursor.next(new_snapshot);
                        }
                        Ordering::Equal => {
                            if new_hunk != old_hunk {
                                start.get_or_insert(new_hunk.buffer_range.start);
                                if old_hunk
                                    .buffer_range
                                    .end
                                    .cmp(&new_hunk.buffer_range.end, new_snapshot)
                                    .is_ge()
                                {
                                    end.replace(old_hunk.buffer_range.end);
                                } else {
                                    end.replace(new_hunk.buffer_range.end);
                                }
                            }

                            new_cursor.next(new_snapshot);
                            old_cursor.next(new_snapshot);
                        }
                        Ordering::Greater => {
                            start.get_or_insert(old_hunk.buffer_range.start);
                            end.replace(old_hunk.buffer_range.end);
                            old_cursor.next(new_snapshot);
                        }
                    }
                }
                (Some(new_hunk), None) => {
                    start.get_or_insert(new_hunk.buffer_range.start);
                    end.replace(new_hunk.buffer_range.end);
                    new_cursor.next(new_snapshot);
                }
                (None, Some(old_hunk)) => {
                    start.get_or_insert(old_hunk.buffer_range.start);
                    end.replace(old_hunk.buffer_range.end);
                    old_cursor.next(new_snapshot);
                }
                (None, None) => break,
            }
        }

        start.zip(end).map(|(start, end)| start..end)
    }
}

fn compute_hunks(
    diff_base: Option<(Arc<String>, Rope)>,
    buffer: text::BufferSnapshot,
) -> SumTree<InternalDiffHunk> {
    let mut tree = SumTree::new(&buffer);

    if let Some((diff_base, diff_base_rope)) = diff_base {
        let buffer_text = buffer.as_rope().to_string();

        let mut options = GitOptions::default();
        options.context_lines(0);
        let patch = GitPatch::from_buffers(
            diff_base.as_bytes(),
            None,
            buffer_text.as_bytes(),
            None,
            Some(&mut options),
        )
        .log_err();

        // A common case in Zed is that the empty buffer is represented as just a newline,
        // but if we just compute a naive diff you get a "preserved" line in the middle,
        // which is a bit odd.
        if buffer_text == "\n" && diff_base.ends_with("\n") && diff_base.len() > 1 {
            tree.push(
                InternalDiffHunk {
                    buffer_range: buffer.anchor_before(0)..buffer.anchor_before(0),
                    diff_base_byte_range: 0..diff_base.len() - 1,
                },
                &buffer,
            );
            return tree;
        }

        if let Some(patch) = patch {
            let mut divergence = 0;
            for hunk_index in 0..patch.num_hunks() {
                let hunk = process_patch_hunk(
                    &patch,
                    hunk_index,
                    &diff_base_rope,
                    &buffer,
                    &mut divergence,
                );
                tree.push(hunk, &buffer);
            }
        }
    } else {
        tree.push(
            InternalDiffHunk {
                buffer_range: Anchor::MIN..Anchor::MAX,
                diff_base_byte_range: 0..0,
            },
            &buffer,
        );
    }

    tree
}

fn process_patch_hunk(
    patch: &GitPatch<'_>,
    hunk_index: usize,
    diff_base: &Rope,
    buffer: &text::BufferSnapshot,
    buffer_row_divergence: &mut i64,
) -> InternalDiffHunk {
    let line_item_count = patch.num_lines_in_hunk(hunk_index).unwrap();
    assert!(line_item_count > 0);

    let mut first_deletion_buffer_row: Option<u32> = None;
    let mut buffer_row_range: Option<Range<u32>> = None;
    let mut diff_base_byte_range: Option<Range<usize>> = None;
    let mut first_addition_old_row: Option<u32> = None;

    for line_index in 0..line_item_count {
        let line = patch.line_in_hunk(hunk_index, line_index).unwrap();
        let kind = line.origin_value();
        let content_offset = line.content_offset() as isize;
        let content_len = line.content().len() as isize;
        match kind {
            GitDiffLineType::Addition => {
                if first_addition_old_row.is_none() {
                    first_addition_old_row = Some(
                        (line.new_lineno().unwrap() as i64 - *buffer_row_divergence - 1) as u32,
                    );
                }
                *buffer_row_divergence += 1;
                let row = line.new_lineno().unwrap().saturating_sub(1);

                match &mut buffer_row_range {
                    Some(Range { end, .. }) => *end = row + 1,
                    None => buffer_row_range = Some(row..row + 1),
                }
            }
            GitDiffLineType::Deletion => {
                let end = content_offset + content_len;

                match &mut diff_base_byte_range {
                    Some(head_byte_range) => head_byte_range.end = end as usize,
                    None => diff_base_byte_range = Some(content_offset as usize..end as usize),
                }

                if first_deletion_buffer_row.is_none() {
                    let old_row = line.old_lineno().unwrap().saturating_sub(1);
                    let row = old_row as i64 + *buffer_row_divergence;
                    first_deletion_buffer_row = Some(row as u32);
                }

                *buffer_row_divergence -= 1;
            }
            _ => {}
        }
    }

    let buffer_row_range = buffer_row_range.unwrap_or_else(|| {
        // Pure deletion hunk without addition.
        let row = first_deletion_buffer_row.unwrap();
        row..row
    });
    let diff_base_byte_range = diff_base_byte_range.unwrap_or_else(|| {
        // Pure addition hunk without deletion.
        let row = first_addition_old_row.unwrap();
        let offset = diff_base.point_to_offset(Point::new(row, 0));
        offset..offset
    });

    let start = Point::new(buffer_row_range.start, 0);
    let end = Point::new(buffer_row_range.end, 0);
    let buffer_range = buffer.anchor_before(start)..buffer.anchor_before(end);
    InternalDiffHunk {
        buffer_range,
        diff_base_byte_range,
    }
}

impl std::fmt::Debug for BufferDiff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferChangeSet")
            .field("snapshot", &self.inner)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub enum BufferDiffEvent {
    DiffChanged {
        changed_range: Option<Range<text::Anchor>>,
    },
    LanguageChanged,
    HunksStagedOrUnstaged(Option<Rope>),
}

impl EventEmitter<BufferDiffEvent> for BufferDiff {}

impl BufferDiff {
    pub fn new(buffer: &text::BufferSnapshot, cx: &mut App) -> Self {
        BufferDiff {
            buffer_id: buffer.remote_id(),
            inner: BufferDiffSnapshot::empty(buffer, cx).inner,
            secondary_diff: None,
        }
    }

    pub fn set_secondary_diff(&mut self, diff: Entity<BufferDiff>) {
        self.secondary_diff = Some(diff);
    }

    pub fn secondary_diff(&self) -> Option<Entity<BufferDiff>> {
        self.secondary_diff.clone()
    }

    pub fn clear_pending_hunks(&mut self, cx: &mut Context<Self>) {
        if self.secondary_diff.is_some() {
            self.inner.pending_hunks = SumTree::from_summary(DiffHunkSummary::default());
            cx.emit(BufferDiffEvent::DiffChanged {
                changed_range: Some(Anchor::MIN..Anchor::MAX),
            });
        }
    }

    pub fn stage_or_unstage_hunks(
        &mut self,
        stage: bool,
        hunks: &[DiffHunk],
        buffer: &text::BufferSnapshot,
        file_exists: bool,
        cx: &mut Context<Self>,
    ) -> Option<Rope> {
        let new_index_text = self.inner.stage_or_unstage_hunks_impl(
            &self.secondary_diff.as_ref()?.read(cx).inner,
            stage,
            &hunks,
            buffer,
            file_exists,
        );

        cx.emit(BufferDiffEvent::HunksStagedOrUnstaged(
            new_index_text.clone(),
        ));
        if let Some((first, last)) = hunks.first().zip(hunks.last()) {
            let changed_range = first.buffer_range.start..last.buffer_range.end;
            cx.emit(BufferDiffEvent::DiffChanged {
                changed_range: Some(changed_range),
            });
        }
        new_index_text
    }

    pub fn range_to_hunk_range(
        &self,
        range: Range<Anchor>,
        buffer: &text::BufferSnapshot,
        cx: &App,
    ) -> Option<Range<Anchor>> {
        let start = self
            .hunks_intersecting_range(range.clone(), &buffer, cx)
            .next()?
            .buffer_range
            .start;
        let end = self
            .hunks_intersecting_range_rev(range.clone(), &buffer)
            .next()?
            .buffer_range
            .end;
        Some(start..end)
    }

    pub async fn update_diff(
        this: Entity<BufferDiff>,
        buffer: text::BufferSnapshot,
        base_text: Option<Arc<String>>,
        base_text_changed: bool,
        language_changed: bool,
        language: Option<Arc<Language>>,
        language_registry: Option<Arc<LanguageRegistry>>,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<BufferDiffSnapshot> {
        Ok(if base_text_changed || language_changed {
            cx.update(|cx| {
                BufferDiffSnapshot::new_with_base_text(
                    buffer.clone(),
                    base_text,
                    language.clone(),
                    language_registry.clone(),
                    cx,
                )
            })?
            .await
        } else {
            this.read_with(cx, |this, cx| {
                BufferDiffSnapshot::new_with_base_buffer(
                    buffer.clone(),
                    base_text,
                    this.base_text().clone(),
                    cx,
                )
            })?
            .await
        })
    }

    pub fn language_changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(BufferDiffEvent::LanguageChanged);
    }

    pub fn set_snapshot(
        &mut self,
        new_snapshot: BufferDiffSnapshot,
        buffer: &text::BufferSnapshot,
        cx: &mut Context<Self>,
    ) -> Option<Range<Anchor>> {
        self.set_snapshot_with_secondary(new_snapshot, buffer, None, false, cx)
    }

    pub fn set_snapshot_with_secondary(
        &mut self,
        new_snapshot: BufferDiffSnapshot,
        buffer: &text::BufferSnapshot,
        secondary_diff_change: Option<Range<Anchor>>,
        clear_pending_hunks: bool,
        cx: &mut Context<Self>,
    ) -> Option<Range<Anchor>> {
        log::debug!("set snapshot with secondary {secondary_diff_change:?}");

        let state = &mut self.inner;
        let new_state = new_snapshot.inner;
        let (base_text_changed, mut changed_range) =
            match (state.base_text_exists, new_state.base_text_exists) {
                (false, false) => (true, None),
                (true, true) if state.base_text.remote_id() == new_state.base_text.remote_id() => {
                    (false, new_state.compare(&state, buffer))
                }
                _ => (true, Some(text::Anchor::MIN..text::Anchor::MAX)),
            };

        if let Some(secondary_changed_range) = secondary_diff_change {
            if let Some(secondary_hunk_range) =
                self.range_to_hunk_range(secondary_changed_range, &buffer, cx)
            {
                if let Some(range) = &mut changed_range {
                    range.start = secondary_hunk_range.start.min(&range.start, &buffer);
                    range.end = secondary_hunk_range.end.max(&range.end, &buffer);
                } else {
                    changed_range = Some(secondary_hunk_range);
                }
            }
        }

        let state = &mut self.inner;
        state.base_text_exists = new_state.base_text_exists;
        state.base_text = new_state.base_text;
        state.hunks = new_state.hunks;
        if base_text_changed || clear_pending_hunks {
            if let Some((first, last)) = state.pending_hunks.first().zip(state.pending_hunks.last())
            {
                if let Some(range) = &mut changed_range {
                    range.start = range.start.min(&first.buffer_range.start, &buffer);
                    range.end = range.end.max(&last.buffer_range.end, &buffer);
                } else {
                    changed_range = Some(first.buffer_range.start..last.buffer_range.end);
                }
            }
            state.pending_hunks = SumTree::new(buffer);
        }

        cx.emit(BufferDiffEvent::DiffChanged {
            changed_range: changed_range.clone(),
        });
        changed_range
    }

    pub fn base_text(&self) -> &language::BufferSnapshot {
        &self.inner.base_text
    }

    pub fn base_text_exists(&self) -> bool {
        self.inner.base_text_exists
    }

    pub fn snapshot(&self, cx: &App) -> BufferDiffSnapshot {
        BufferDiffSnapshot {
            inner: self.inner.clone(),
            secondary_diff: self
                .secondary_diff
                .as_ref()
                .map(|diff| Box::new(diff.read(cx).snapshot(cx))),
        }
    }

    pub fn hunks<'a>(
        &'a self,
        buffer_snapshot: &'a text::BufferSnapshot,
        cx: &'a App,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        self.hunks_intersecting_range(Anchor::MIN..Anchor::MAX, buffer_snapshot, cx)
    }

    pub fn hunks_intersecting_range<'a>(
        &'a self,
        range: Range<text::Anchor>,
        buffer_snapshot: &'a text::BufferSnapshot,
        cx: &'a App,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        let unstaged_counterpart = self
            .secondary_diff
            .as_ref()
            .map(|diff| &diff.read(cx).inner);
        self.inner
            .hunks_intersecting_range(range, buffer_snapshot, unstaged_counterpart)
    }

    pub fn hunks_intersecting_range_rev<'a>(
        &'a self,
        range: Range<text::Anchor>,
        buffer_snapshot: &'a text::BufferSnapshot,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        self.inner
            .hunks_intersecting_range_rev(range, buffer_snapshot)
    }

    pub fn hunks_in_row_range<'a>(
        &'a self,
        range: Range<u32>,
        buffer: &'a text::BufferSnapshot,
        cx: &'a App,
    ) -> impl 'a + Iterator<Item = DiffHunk> {
        let start = buffer.anchor_before(Point::new(range.start, 0));
        let end = buffer.anchor_after(Point::new(range.end, 0));
        self.hunks_intersecting_range(start..end, buffer, cx)
    }

    pub fn set_base_text_buffer(
        &mut self,
        base_buffer: Entity<language::Buffer>,
        buffer: text::BufferSnapshot,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<()> {
        let base_buffer = base_buffer.read(cx);
        let language_registry = base_buffer.language_registry();
        let base_buffer = base_buffer.snapshot();
        self.set_base_text(base_buffer, language_registry, buffer, cx)
    }

    /// Used in cases where the change set isn't derived from git.
    pub fn set_base_text(
        &mut self,
        base_buffer: language::BufferSnapshot,
        language_registry: Option<Arc<LanguageRegistry>>,
        buffer: text::BufferSnapshot,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        let this = cx.weak_entity();
        let base_text = Arc::new(base_buffer.text());

        let snapshot = BufferDiffSnapshot::new_with_base_text(
            buffer.clone(),
            Some(base_text),
            base_buffer.language().cloned(),
            language_registry,
            cx,
        );
        let complete_on_drop = util::defer(|| {
            tx.send(()).ok();
        });
        cx.spawn(async move |_, cx| {
            let snapshot = snapshot.await;
            let Some(this) = this.upgrade() else {
                return;
            };
            this.update(cx, |this, cx| {
                this.set_snapshot(snapshot, &buffer, cx);
            })
            .log_err();
            drop(complete_on_drop)
        })
        .detach();
        rx
    }

    pub fn base_text_string(&self) -> Option<String> {
        self.inner
            .base_text_exists
            .then(|| self.inner.base_text.text())
    }
}

impl DiffHunk {
    pub fn is_created_file(&self) -> bool {
        self.diff_base_byte_range == (0..0) && self.buffer_range == (Anchor::MIN..Anchor::MAX)
    }

    pub fn status(&self) -> DiffHunkStatus {
        let kind = if self.buffer_range.start == self.buffer_range.end {
            DiffHunkStatusKind::Deleted
        } else if self.diff_base_byte_range.is_empty() {
            DiffHunkStatusKind::Added
        } else {
            DiffHunkStatusKind::Modified
        };
        DiffHunkStatus {
            kind,
            secondary: self.secondary_status,
        }
    }
}

impl DiffHunkStatus {
    pub fn has_secondary_hunk(&self) -> bool {
        matches!(
            self.secondary,
            DiffHunkSecondaryStatus::HasSecondaryHunk
                | DiffHunkSecondaryStatus::SecondaryHunkAdditionPending
                | DiffHunkSecondaryStatus::OverlapsWithSecondaryHunk
        )
    }

    pub fn is_pending(&self) -> bool {
        matches!(
            self.secondary,
            DiffHunkSecondaryStatus::SecondaryHunkAdditionPending
                | DiffHunkSecondaryStatus::SecondaryHunkRemovalPending
        )
    }

    pub fn is_deleted(&self) -> bool {
        self.kind == DiffHunkStatusKind::Deleted
    }

    pub fn is_added(&self) -> bool {
        self.kind == DiffHunkStatusKind::Added
    }

    pub fn is_modified(&self) -> bool {
        self.kind == DiffHunkStatusKind::Modified
    }

    pub fn added(secondary: DiffHunkSecondaryStatus) -> Self {
        Self {
            kind: DiffHunkStatusKind::Added,
            secondary,
        }
    }

    pub fn modified(secondary: DiffHunkSecondaryStatus) -> Self {
        Self {
            kind: DiffHunkStatusKind::Modified,
            secondary,
        }
    }

    pub fn deleted(secondary: DiffHunkSecondaryStatus) -> Self {
        Self {
            kind: DiffHunkStatusKind::Deleted,
            secondary,
        }
    }

    pub fn deleted_none() -> Self {
        Self {
            kind: DiffHunkStatusKind::Deleted,
            secondary: DiffHunkSecondaryStatus::NoSecondaryHunk,
        }
    }

    pub fn added_none() -> Self {
        Self {
            kind: DiffHunkStatusKind::Added,
            secondary: DiffHunkSecondaryStatus::NoSecondaryHunk,
        }
    }

    pub fn modified_none() -> Self {
        Self {
            kind: DiffHunkStatusKind::Modified,
            secondary: DiffHunkSecondaryStatus::NoSecondaryHunk,
        }
    }
}
