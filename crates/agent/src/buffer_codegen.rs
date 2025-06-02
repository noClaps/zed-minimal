use crate::inline_prompt_editor::CodegenStatus;
use anyhow::Result;
use collections::HashSet;
use editor::{Anchor, AnchorRangeExt, MultiBuffer, MultiBufferSnapshot};
use futures::Stream;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Subscription, Task};
use language::{Buffer, Point, TransactionId, line_diff};
use language_model::{LanguageModel, LanguageModelRegistry};
use multi_buffer::MultiBufferRow;
use std::{
    iter,
    ops::{Range, RangeInclusive},
    pin::Pin,
    sync::Arc,
    task::{self, Poll},
};
use streaming_diff::LineOperation;

pub struct BufferCodegen {
    alternatives: Vec<Entity<CodegenAlternative>>,
    pub active_alternative: usize,
    seen_alternatives: HashSet<usize>,
    subscriptions: Vec<Subscription>,
    buffer: Entity<MultiBuffer>,
    range: Range<Anchor>,
    initial_transaction_id: Option<TransactionId>,
    pub is_insertion: bool,
}

impl BufferCodegen {
    pub fn new(
        buffer: Entity<MultiBuffer>,
        range: Range<Anchor>,
        initial_transaction_id: Option<TransactionId>,
        cx: &mut Context<Self>,
    ) -> Self {
        let codegen =
            cx.new(|cx| CodegenAlternative::new(buffer.clone(), range.clone(), false, cx));
        let mut this = Self {
            is_insertion: range.to_offset(&buffer.read(cx).snapshot(cx)).is_empty(),
            alternatives: vec![codegen],
            active_alternative: 0,
            seen_alternatives: HashSet::default(),
            subscriptions: Vec::new(),
            buffer,
            range,
            initial_transaction_id,
        };
        this.activate(0, cx);
        this
    }

    fn subscribe_to_alternative(&mut self, cx: &mut Context<Self>) {
        let codegen = self.active_alternative().clone();
        self.subscriptions.clear();
        self.subscriptions
            .push(cx.observe(&codegen, |_, _, cx| cx.notify()));
        self.subscriptions
            .push(cx.subscribe(&codegen, |_, _, event, cx| cx.emit(*event)));
    }

    pub fn active_alternative(&self) -> &Entity<CodegenAlternative> {
        &self.alternatives[self.active_alternative]
    }

    pub fn status<'a>(&self, cx: &'a App) -> &'a CodegenStatus {
        &self.active_alternative().read(cx).status
    }

    pub fn alternative_count(&self, cx: &App) -> usize {
        LanguageModelRegistry::read_global(cx)
            .inline_alternative_models()
            .len()
            + 1
    }

    pub fn cycle_prev(&mut self, cx: &mut Context<Self>) {
        let next_active_ix = if self.active_alternative == 0 {
            self.alternatives.len() - 1
        } else {
            self.active_alternative - 1
        };
        self.activate(next_active_ix, cx);
    }

    pub fn cycle_next(&mut self, cx: &mut Context<Self>) {
        let next_active_ix = (self.active_alternative + 1) % self.alternatives.len();
        self.activate(next_active_ix, cx);
    }

    fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        self.active_alternative()
            .update(cx, |codegen, cx| codegen.set_active(false, cx));
        self.seen_alternatives.insert(index);
        self.active_alternative = index;
        self.active_alternative()
            .update(cx, |codegen, cx| codegen.set_active(true, cx));
        self.subscribe_to_alternative(cx);
        cx.notify();
    }

    pub fn start(
        &mut self,
        primary_model: Arc<dyn LanguageModel>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let alternative_models = LanguageModelRegistry::read_global(cx)
            .inline_alternative_models()
            .to_vec();

        self.active_alternative()
            .update(cx, |alternative, cx| alternative.undo(cx));
        self.activate(0, cx);
        self.alternatives.truncate(1);

        for _ in 0..alternative_models.len() {
            self.alternatives.push(cx.new(|cx| {
                CodegenAlternative::new(self.buffer.clone(), self.range.clone(), false, cx)
            }));
        }

        for (_, alternative) in iter::once(primary_model)
            .chain(alternative_models)
            .zip(&self.alternatives)
        {
            alternative.update(cx, |alternative, cx| alternative.start(cx))?;
        }

        Ok(())
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        for codegen in &self.alternatives {
            codegen.update(cx, |codegen, cx| codegen.stop(cx));
        }
    }

    pub fn undo(&mut self, cx: &mut Context<Self>) {
        self.active_alternative()
            .update(cx, |codegen, cx| codegen.undo(cx));

        self.buffer.update(cx, |buffer, cx| {
            if let Some(transaction_id) = self.initial_transaction_id.take() {
                buffer.undo_transaction(transaction_id, cx);
                buffer.refresh_preview(cx);
            }
        });
    }

    pub fn buffer(&self, cx: &App) -> Entity<MultiBuffer> {
        self.active_alternative().read(cx).buffer.clone()
    }

    pub fn old_buffer(&self, cx: &App) -> Entity<Buffer> {
        self.active_alternative().read(cx).old_buffer.clone()
    }

    pub fn snapshot(&self, cx: &App) -> MultiBufferSnapshot {
        self.active_alternative().read(cx).snapshot.clone()
    }

    pub fn edit_position(&self, cx: &App) -> Option<Anchor> {
        self.active_alternative().read(cx).edit_position
    }

    pub fn diff<'a>(&self, cx: &'a App) -> &'a Diff {
        &self.active_alternative().read(cx).diff
    }

    pub fn last_equal_ranges<'a>(&self, cx: &'a App) -> &'a [Range<Anchor>] {
        self.active_alternative().read(cx).last_equal_ranges()
    }
}

impl EventEmitter<CodegenEvent> for BufferCodegen {}

pub struct CodegenAlternative {
    buffer: Entity<MultiBuffer>,
    old_buffer: Entity<Buffer>,
    snapshot: MultiBufferSnapshot,
    edit_position: Option<Anchor>,
    range: Range<Anchor>,
    last_equal_ranges: Vec<Range<Anchor>>,
    transformation_transaction_id: Option<TransactionId>,
    status: CodegenStatus,
    generation: Task<()>,
    diff: Diff,
    _subscription: gpui::Subscription,
    active: bool,
    edits: Vec<(Range<Anchor>, String)>,
    line_operations: Vec<LineOperation>,
    pub message_id: Option<String>,
}

impl EventEmitter<CodegenEvent> for CodegenAlternative {}

impl CodegenAlternative {
    pub fn new(
        buffer: Entity<MultiBuffer>,
        range: Range<Anchor>,
        active: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let snapshot = buffer.read(cx).snapshot(cx);

        let (old_buffer, _, _) = snapshot
            .range_to_buffer_ranges(range.clone())
            .pop()
            .unwrap();
        let old_buffer = cx.new(|cx| {
            let text = old_buffer.as_rope().clone();
            let line_ending = old_buffer.line_ending();
            let language = old_buffer.language().cloned();
            let language_registry = buffer
                .read(cx)
                .buffer(old_buffer.remote_id())
                .unwrap()
                .read(cx)
                .language_registry();

            let mut buffer = Buffer::local_normalized(text, line_ending, cx);
            buffer.set_language(language, cx);
            if let Some(language_registry) = language_registry {
                buffer.set_language_registry(language_registry)
            }
            buffer
        });

        Self {
            buffer: buffer.clone(),
            old_buffer,
            edit_position: None,
            message_id: None,
            snapshot,
            last_equal_ranges: Default::default(),
            transformation_transaction_id: None,
            status: CodegenStatus::Idle,
            generation: Task::ready(()),
            diff: Diff::default(),
            _subscription: cx.subscribe(&buffer, Self::handle_buffer_event),
            active,
            edits: Vec::new(),
            line_operations: Vec::new(),
            range,
        }
    }

    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if active != self.active {
            self.active = active;

            if self.active {
                let edits = self.edits.clone();
                self.apply_edits(edits, cx);
                if matches!(self.status, CodegenStatus::Pending) {
                    let line_operations = self.line_operations.clone();
                    self.reapply_line_based_diff(line_operations, cx);
                } else {
                    self.reapply_batch_diff(cx).detach();
                }
            } else if let Some(transaction_id) = self.transformation_transaction_id.take() {
                self.buffer.update(cx, |buffer, cx| {
                    buffer.undo_transaction(transaction_id, cx);
                    buffer.forget_transaction(transaction_id, cx);
                });
            }
        }
    }

    fn handle_buffer_event(
        &mut self,
        _buffer: Entity<MultiBuffer>,
        event: &multi_buffer::Event,
        cx: &mut Context<Self>,
    ) {
        if let multi_buffer::Event::TransactionUndone { transaction_id } = event {
            if self.transformation_transaction_id == Some(*transaction_id) {
                self.transformation_transaction_id = None;
                self.generation = Task::ready(());
                cx.emit(CodegenEvent::Undone);
            }
        }
    }

    pub fn last_equal_ranges(&self) -> &[Range<Anchor>] {
        &self.last_equal_ranges
    }

    pub fn start(&mut self, cx: &mut Context<Self>) -> Result<()> {
        if let Some(transformation_transaction_id) = self.transformation_transaction_id.take() {
            self.buffer.update(cx, |buffer, cx| {
                buffer.undo_transaction(transformation_transaction_id, cx);
            });
        }

        self.edit_position = Some(self.range.start.bias_right(&self.snapshot));
        Ok(())
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.last_equal_ranges.clear();
        if self.diff.is_empty() {
            self.status = CodegenStatus::Idle;
        } else {
            self.status = CodegenStatus::Done;
        }
        self.generation = Task::ready(());
        cx.emit(CodegenEvent::Finished);
        cx.notify();
    }

    pub fn undo(&mut self, cx: &mut Context<Self>) {
        self.buffer.update(cx, |buffer, cx| {
            if let Some(transaction_id) = self.transformation_transaction_id.take() {
                buffer.undo_transaction(transaction_id, cx);
                buffer.refresh_preview(cx);
            }
        });
    }

    fn apply_edits(
        &mut self,
        edits: impl IntoIterator<Item = (Range<Anchor>, String)>,
        cx: &mut Context<CodegenAlternative>,
    ) {
        let transaction = self.buffer.update(cx, |buffer, cx| {
            // Avoid grouping agent edits with user edits.
            buffer.finalize_last_transaction(cx);
            buffer.start_transaction(cx);
            buffer.edit(edits, None, cx);
            buffer.end_transaction(cx)
        });

        if let Some(transaction) = transaction {
            if let Some(first_transaction) = self.transformation_transaction_id {
                // Group all agent edits into the first transaction.
                self.buffer.update(cx, |buffer, cx| {
                    buffer.merge_transactions(transaction, first_transaction, cx)
                });
            } else {
                self.transformation_transaction_id = Some(transaction);
                self.buffer
                    .update(cx, |buffer, cx| buffer.finalize_last_transaction(cx));
            }
        }
    }

    fn reapply_line_based_diff(
        &mut self,
        line_operations: impl IntoIterator<Item = LineOperation>,
        cx: &mut Context<Self>,
    ) {
        let old_snapshot = self.snapshot.clone();
        let old_range = self.range.to_point(&old_snapshot);
        let new_snapshot = self.buffer.read(cx).snapshot(cx);
        let new_range = self.range.to_point(&new_snapshot);

        let mut old_row = old_range.start.row;
        let mut new_row = new_range.start.row;

        self.diff.deleted_row_ranges.clear();
        self.diff.inserted_row_ranges.clear();
        for operation in line_operations {
            match operation {
                LineOperation::Keep { lines } => {
                    old_row += lines;
                    new_row += lines;
                }
                LineOperation::Delete { lines } => {
                    let old_end_row = old_row + lines - 1;
                    let new_row = new_snapshot.anchor_before(Point::new(new_row, 0));

                    if let Some((_, last_deleted_row_range)) =
                        self.diff.deleted_row_ranges.last_mut()
                    {
                        if *last_deleted_row_range.end() + 1 == old_row {
                            *last_deleted_row_range = *last_deleted_row_range.start()..=old_end_row;
                        } else {
                            self.diff
                                .deleted_row_ranges
                                .push((new_row, old_row..=old_end_row));
                        }
                    } else {
                        self.diff
                            .deleted_row_ranges
                            .push((new_row, old_row..=old_end_row));
                    }

                    old_row += lines;
                }
                LineOperation::Insert { lines } => {
                    let new_end_row = new_row + lines - 1;
                    let start = new_snapshot.anchor_before(Point::new(new_row, 0));
                    let end = new_snapshot.anchor_before(Point::new(
                        new_end_row,
                        new_snapshot.line_len(MultiBufferRow(new_end_row)),
                    ));
                    self.diff.inserted_row_ranges.push(start..end);
                    new_row += lines;
                }
            }

            cx.notify();
        }
    }

    fn reapply_batch_diff(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let old_snapshot = self.snapshot.clone();
        let old_range = self.range.to_point(&old_snapshot);
        let new_snapshot = self.buffer.read(cx).snapshot(cx);
        let new_range = self.range.to_point(&new_snapshot);

        cx.spawn(async move |codegen, cx| {
            let (deleted_row_ranges, inserted_row_ranges) = cx
                .background_spawn(async move {
                    let old_text = old_snapshot
                        .text_for_range(
                            Point::new(old_range.start.row, 0)
                                ..Point::new(
                                    old_range.end.row,
                                    old_snapshot.line_len(MultiBufferRow(old_range.end.row)),
                                ),
                        )
                        .collect::<String>();
                    let new_text = new_snapshot
                        .text_for_range(
                            Point::new(new_range.start.row, 0)
                                ..Point::new(
                                    new_range.end.row,
                                    new_snapshot.line_len(MultiBufferRow(new_range.end.row)),
                                ),
                        )
                        .collect::<String>();

                    let old_start_row = old_range.start.row;
                    let new_start_row = new_range.start.row;
                    let mut deleted_row_ranges: Vec<(Anchor, RangeInclusive<u32>)> = Vec::new();
                    let mut inserted_row_ranges = Vec::new();
                    for (old_rows, new_rows) in line_diff(&old_text, &new_text) {
                        let old_rows = old_start_row + old_rows.start..old_start_row + old_rows.end;
                        let new_rows = new_start_row + new_rows.start..new_start_row + new_rows.end;
                        if !old_rows.is_empty() {
                            deleted_row_ranges.push((
                                new_snapshot.anchor_before(Point::new(new_rows.start, 0)),
                                old_rows.start..=old_rows.end - 1,
                            ));
                        }
                        if !new_rows.is_empty() {
                            let start = new_snapshot.anchor_before(Point::new(new_rows.start, 0));
                            let new_end_row = new_rows.end - 1;
                            let end = new_snapshot.anchor_before(Point::new(
                                new_end_row,
                                new_snapshot.line_len(MultiBufferRow(new_end_row)),
                            ));
                            inserted_row_ranges.push(start..end);
                        }
                    }
                    (deleted_row_ranges, inserted_row_ranges)
                })
                .await;

            codegen
                .update(cx, |codegen, cx| {
                    codegen.diff.deleted_row_ranges = deleted_row_ranges;
                    codegen.diff.inserted_row_ranges = inserted_row_ranges;
                    cx.notify();
                })
                .ok();
        })
    }
}

#[derive(Copy, Clone, Debug)]
pub enum CodegenEvent {
    Finished,
    Undone,
}

struct StripInvalidSpans<T> {
    stream: T,
    stream_done: bool,
    buffer: String,
    first_line: bool,
    line_end: bool,
    starts_with_code_block: bool,
}

impl<T> Stream for StripInvalidSpans<T>
where
    T: Stream<Item = Result<String>>,
{
    type Item = Result<String>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<Option<Self::Item>> {
        const CODE_BLOCK_DELIMITER: &str = "```";
        const CURSOR_SPAN: &str = "<|CURSOR|>";

        let this = unsafe { self.get_unchecked_mut() };
        loop {
            if !this.stream_done {
                let mut stream = unsafe { Pin::new_unchecked(&mut this.stream) };
                match stream.as_mut().poll_next(cx) {
                    Poll::Ready(Some(Ok(chunk))) => {
                        this.buffer.push_str(&chunk);
                    }
                    Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error))),
                    Poll::Ready(None) => {
                        this.stream_done = true;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            let mut chunk = String::new();
            let mut consumed = 0;
            if !this.buffer.is_empty() {
                let mut lines = this.buffer.split('\n').enumerate().peekable();
                while let Some((line_ix, line)) = lines.next() {
                    if line_ix > 0 {
                        this.first_line = false;
                    }

                    if this.first_line {
                        let trimmed_line = line.trim();
                        if lines.peek().is_some() {
                            if trimmed_line.starts_with(CODE_BLOCK_DELIMITER) {
                                consumed += line.len() + 1;
                                this.starts_with_code_block = true;
                                continue;
                            }
                        } else if trimmed_line.is_empty()
                            || prefixes(CODE_BLOCK_DELIMITER)
                                .any(|prefix| trimmed_line.starts_with(prefix))
                        {
                            break;
                        }
                    }

                    let line_without_cursor = line.replace(CURSOR_SPAN, "");
                    if lines.peek().is_some() {
                        if this.line_end {
                            chunk.push('\n');
                        }

                        chunk.push_str(&line_without_cursor);
                        this.line_end = true;
                        consumed += line.len() + 1;
                    } else if this.stream_done {
                        if !this.starts_with_code_block
                            || !line_without_cursor.trim().ends_with(CODE_BLOCK_DELIMITER)
                        {
                            if this.line_end {
                                chunk.push('\n');
                            }

                            chunk.push_str(&line);
                        }

                        consumed += line.len();
                    } else {
                        let trimmed_line = line.trim();
                        if trimmed_line.is_empty()
                            || prefixes(CURSOR_SPAN).any(|prefix| trimmed_line.ends_with(prefix))
                            || prefixes(CODE_BLOCK_DELIMITER)
                                .any(|prefix| trimmed_line.ends_with(prefix))
                        {
                            break;
                        } else {
                            if this.line_end {
                                chunk.push('\n');
                                this.line_end = false;
                            }

                            chunk.push_str(&line_without_cursor);
                            consumed += line.len();
                        }
                    }
                }
            }

            this.buffer = this.buffer.split_off(consumed);
            if !chunk.is_empty() {
                return Poll::Ready(Some(Ok(chunk)));
            } else if this.stream_done {
                return Poll::Ready(None);
            }
        }
    }
}

fn prefixes(text: &str) -> impl Iterator<Item = &str> {
    (0..text.len() - 1).map(|ix| &text[..ix + 1])
}

#[derive(Default)]
pub struct Diff {
    pub deleted_row_ranges: Vec<(Anchor, RangeInclusive<u32>)>,
    pub inserted_row_ranges: Vec<Range<Anchor>>,
}

impl Diff {
    fn is_empty(&self) -> bool {
        self.deleted_row_ranges.is_empty() && self.inserted_row_ranges.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use indoc::indoc;
    use language::{
        Buffer, Language, LanguageConfig, LanguageMatcher, Point, language_settings,
        tree_sitter_rust,
    };
    use language_model::LanguageModelRegistry;
    use rand::prelude::*;
    use serde::Serialize;
    use settings::SettingsStore;
    use std::cmp;
    use std::sync::Arc;

    #[derive(Serialize)]
    pub struct DummyCompletionRequest {
        pub name: String,
    }

    #[gpui::test(iterations = 10)]
    async fn test_transform_autoindent(cx: &mut TestAppContext, mut rng: StdRng) {
        init_test(cx);

        let text = indoc! {"
            fn main() {
                let x = 0;
                for _ in 0..10 {
                    x += 1;
                }
            }
        "};
        let buffer = cx.new(|cx| Buffer::local(text, cx).with_language(Arc::new(rust_lang()), cx));
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

        let mut new_text = concat!(
            "       let mut x = 0;\n",
            "       while x < 10 {\n",
            "           x += 1;\n",
            "       }",
        );
        while !new_text.is_empty() {
            let max_len = cmp::min(new_text.len(), 10);
            let len = rng.gen_range(1..=max_len);
            let (_, suffix) = new_text.split_at(len);
            new_text = suffix;
            cx.background_executor.run_until_parked();
        }
        cx.background_executor.run_until_parked();

        assert_eq!(
            buffer.read_with(cx, |buffer, cx| buffer.snapshot(cx).text()),
            indoc! {"
                fn main() {
                    let mut x = 0;
                    while x < 10 {
                        x += 1;
                    }
                }
            "}
        );
    }

    #[gpui::test(iterations = 10)]
    async fn test_autoindent_when_generating_past_indentation(
        cx: &mut TestAppContext,
        mut rng: StdRng,
    ) {
        init_test(cx);

        let text = indoc! {"
            fn main() {
                le
            }
        "};
        let buffer = cx.new(|cx| Buffer::local(text, cx).with_language(Arc::new(rust_lang()), cx));
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

        cx.background_executor.run_until_parked();

        let mut new_text = concat!(
            "t mut x = 0;\n",
            "while x < 10 {\n",
            "    x += 1;\n",
            "}", //
        );
        while !new_text.is_empty() {
            let max_len = cmp::min(new_text.len(), 10);
            let len = rng.gen_range(1..=max_len);
            let (_, suffix) = new_text.split_at(len);
            new_text = suffix;
            cx.background_executor.run_until_parked();
        }
        cx.background_executor.run_until_parked();

        assert_eq!(
            buffer.read_with(cx, |buffer, cx| buffer.snapshot(cx).text()),
            indoc! {"
                fn main() {
                    let mut x = 0;
                    while x < 10 {
                        x += 1;
                    }
                }
            "}
        );
    }

    #[gpui::test(iterations = 10)]
    async fn test_autoindent_when_generating_before_indentation(
        cx: &mut TestAppContext,
        mut rng: StdRng,
    ) {
        init_test(cx);

        let text = concat!(
            "fn main() {\n",
            "  \n",
            "}\n" //
        );
        let buffer = cx.new(|cx| Buffer::local(text, cx).with_language(Arc::new(rust_lang()), cx));
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

        cx.background_executor.run_until_parked();

        let mut new_text = concat!(
            "let mut x = 0;\n",
            "while x < 10 {\n",
            "    x += 1;\n",
            "}", //
        );
        while !new_text.is_empty() {
            let max_len = cmp::min(new_text.len(), 10);
            let len = rng.gen_range(1..=max_len);
            let (_, suffix) = new_text.split_at(len);
            new_text = suffix;
            cx.background_executor.run_until_parked();
        }
        cx.background_executor.run_until_parked();

        assert_eq!(
            buffer.read_with(cx, |buffer, cx| buffer.snapshot(cx).text()),
            indoc! {"
                fn main() {
                    let mut x = 0;
                    while x < 10 {
                        x += 1;
                    }
                }
            "}
        );
    }

    #[gpui::test(iterations = 10)]
    async fn test_autoindent_respects_tabs_in_selection(cx: &mut TestAppContext) {
        init_test(cx);

        let text = indoc! {"
            func main() {
            \tx := 0
            \tfor i := 0; i < 10; i++ {
            \t\tx++
            \t}
            }
        "};
        let buffer = cx.new(|cx| Buffer::local(text, cx));
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

        cx.background_executor.run_until_parked();

        assert_eq!(
            buffer.read_with(cx, |buffer, cx| buffer.snapshot(cx).text()),
            indoc! {"
                func main() {
                \tx := 0
                \tfor x < 10 {
                \t\tx++
                \t}
                }
            "}
        );
    }

    #[gpui::test]
    async fn test_inactive_codegen_alternative(cx: &mut TestAppContext) {
        init_test(cx);

        let text = indoc! {"
            fn main() {
                let x = 0;
            }
        "};
        let buffer = cx.new(|cx| Buffer::local(text, cx).with_language(Arc::new(rust_lang()), cx));
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        let range = buffer.read_with(cx, |buffer, cx| {
            let snapshot = buffer.snapshot(cx);
            snapshot.anchor_before(Point::new(1, 0))..snapshot.anchor_after(Point::new(1, 14))
        });
        let codegen =
            cx.new(|cx| CodegenAlternative::new(buffer.clone(), range.clone(), false, cx));

        cx.run_until_parked();

        // The codegen is inactive, so the buffer doesn't get modified.
        assert_eq!(
            buffer.read_with(cx, |buffer, cx| buffer.snapshot(cx).text()),
            text
        );

        // Activating the codegen applies the changes.
        codegen.update(cx, |codegen, cx| codegen.set_active(true, cx));
        assert_eq!(
            buffer.read_with(cx, |buffer, cx| buffer.snapshot(cx).text()),
            indoc! {"
                fn main() {
                    let mut x = 0;
                    x += 1;
                }
            "}
        );

        // Deactivating the codegen undoes the changes.
        codegen.update(cx, |codegen, cx| codegen.set_active(false, cx));
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, cx| buffer.snapshot(cx).text()),
            text
        );
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(LanguageModelRegistry::test);
        cx.set_global(cx.update(SettingsStore::test));
        cx.update(language_settings::init);
    }

    fn rust_lang() -> Language {
        Language::new(
            LanguageConfig {
                name: "Rust".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["rs".to_string()],
                    ..Default::default()
                },
                ..Default::default()
            },
            Some(tree_sitter_rust::LANGUAGE.into()),
        )
        .with_indents_query(
            r#"
            (call_expression) @indent
            (field_expression) @indent
            (_ "(" ")" @end) @indent
            (_ "{" "}" @end) @indent
            "#,
        )
        .unwrap()
    }
}
