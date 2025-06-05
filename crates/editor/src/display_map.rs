//! This module defines where the text should be displayed in an [`Editor`][Editor].
//!
//! Not literally though - rendering, layout and all that jazz is a responsibility of [`EditorElement`][EditorElement].
//! Instead, [`DisplayMap`] decides where Inlays/Inlay hints are displayed, when
//! to apply a soft wrap, where to add fold indicators, whether there are any tabs in the buffer that
//! we display as spaces and where to display custom blocks (like diagnostics).
//! Seems like a lot? That's because it is. [`DisplayMap`] is conceptually made up
//! of several smaller structures that form a hierarchy (starting at the bottom):
//! - [`InlayMap`] that decides where the [`Inlay`]s should be displayed.
//! - [`FoldMap`] that decides where the fold indicators should be; it also tracks parts of a source file that are currently folded.
//! - [`TabMap`] that keeps track of hard tabs in a buffer.
//! - [`WrapMap`] that handles soft wrapping.
//! - [`BlockMap`] that tracks custom blocks such as diagnostics that should be displayed within buffer.
//! - [`DisplayMap`] that adds background highlights to the regions of text.
//!   Each one of those builds on top of preceding map.
//!
//! [Editor]: crate::Editor
//! [EditorElement]: crate::element::EditorElement

mod block_map;
mod crease_map;
mod custom_highlights;
mod fold_map;
mod inlay_map;
pub(crate) mod invisibles;
mod tab_map;
mod wrap_map;

use crate::{
    EditorStyle, InlayId, RowExt, hover_links::InlayHighlight, movement::TextLayoutDetails,
};
pub use block_map::{
    Block, BlockChunks as DisplayChunks, BlockContext, BlockId, BlockMap, BlockPlacement,
    BlockPoint, BlockProperties, BlockRows, BlockStyle, CustomBlockId, EditorMargins, RenderBlock,
    StickyHeaderExcerpt,
};
use block_map::{BlockRow, BlockSnapshot};
use collections::{HashMap, HashSet};
pub use crease_map::*;
pub use fold_map::{ChunkRenderer, ChunkRendererContext, Fold, FoldId, FoldPlaceholder, FoldPoint};
use fold_map::{FoldMap, FoldSnapshot};
use gpui::{App, Context, Entity, Font, HighlightStyle, LineLayout, Pixels, UnderlineStyle};
pub use inlay_map::Inlay;
use inlay_map::{InlayMap, InlaySnapshot};
pub use inlay_map::{InlayOffset, InlayPoint};
pub use invisibles::{is_invisible, replacement};
use language::{
    OffsetUtf16, Point, Subscription as BufferSubscription, language_settings::language_settings,
};
use multi_buffer::{
    Anchor, AnchorRangeExt, ExcerptId, MultiBuffer, MultiBufferPoint, MultiBufferRow,
    MultiBufferSnapshot, RowInfo, ToOffset, ToPoint,
};
use project::project_settings::DiagnosticSeverity;
use serde::Deserialize;

use std::{
    any::TypeId,
    borrow::Cow,
    fmt::Debug,
    iter,
    num::NonZeroU32,
    ops::{Add, Range, Sub},
    sync::Arc,
};
use sum_tree::{Bias, TreeMap};
use tab_map::{TabMap, TabSnapshot};
use text::{BufferId, LineIndent};
use ui::{SharedString, px};
use unicode_segmentation::UnicodeSegmentation;
use wrap_map::{WrapMap, WrapSnapshot};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FoldStatus {
    Folded,
    Foldable,
}

pub trait ToDisplayPoint {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint;
}

type TextHighlights = TreeMap<TypeId, Arc<(HighlightStyle, Vec<Range<Anchor>>)>>;
type InlayHighlights = TreeMap<TypeId, TreeMap<InlayId, (HighlightStyle, InlayHighlight)>>;

/// Decides how text in a [`MultiBuffer`] should be displayed in a buffer, handling inlay hints,
/// folding, hard tabs, soft wrapping, custom blocks (like diagnostics), and highlighting.
///
/// See the [module level documentation](self) for more information.
pub struct DisplayMap {
    /// The buffer that we are displaying.
    buffer: Entity<MultiBuffer>,
    buffer_subscription: BufferSubscription,
    /// Decides where the [`Inlay`]s should be displayed.
    inlay_map: InlayMap,
    /// Decides where the fold indicators should be and tracks parts of a source file that are currently folded.
    fold_map: FoldMap,
    /// Keeps track of hard tabs in a buffer.
    tab_map: TabMap,
    /// Handles soft wrapping.
    wrap_map: Entity<WrapMap>,
    /// Tracks custom blocks such as diagnostics that should be displayed within buffer.
    block_map: BlockMap,
    /// Regions of text that should be highlighted.
    text_highlights: TextHighlights,
    /// Regions of inlays that should be highlighted.
    inlay_highlights: InlayHighlights,
    /// A container for explicitly foldable ranges, which supersede indentation based fold range suggestions.
    crease_map: CreaseMap,
    pub(crate) fold_placeholder: FoldPlaceholder,
    pub clip_at_line_ends: bool,
    pub(crate) masked: bool,
    pub(crate) diagnostics_max_severity: DiagnosticSeverity,
}

impl DisplayMap {
    pub fn new(
        buffer: Entity<MultiBuffer>,
        font: Font,
        font_size: Pixels,
        wrap_width: Option<Pixels>,
        buffer_header_height: u32,
        excerpt_header_height: u32,
        fold_placeholder: FoldPlaceholder,
        diagnostics_max_severity: DiagnosticSeverity,
        cx: &mut Context<Self>,
    ) -> Self {
        let buffer_subscription = buffer.update(cx, |buffer, _| buffer.subscribe());

        let tab_size = Self::tab_size(&buffer, cx);
        let buffer_snapshot = buffer.read(cx).snapshot(cx);
        let crease_map = CreaseMap::new(&buffer_snapshot);
        let (inlay_map, snapshot) = InlayMap::new(buffer_snapshot);
        let (fold_map, snapshot) = FoldMap::new(snapshot);
        let (tab_map, snapshot) = TabMap::new(snapshot, tab_size);
        let (wrap_map, snapshot) = WrapMap::new(snapshot, font, font_size, wrap_width, cx);
        let block_map = BlockMap::new(snapshot, buffer_header_height, excerpt_header_height);

        cx.observe(&wrap_map, |_, _, cx| cx.notify()).detach();

        DisplayMap {
            buffer,
            buffer_subscription,
            fold_map,
            inlay_map,
            tab_map,
            wrap_map,
            block_map,
            crease_map,
            fold_placeholder,
            diagnostics_max_severity,
            text_highlights: Default::default(),
            inlay_highlights: Default::default(),
            clip_at_line_ends: false,
            masked: false,
        }
    }

    pub fn snapshot(&mut self, cx: &mut Context<Self>) -> DisplaySnapshot {
        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let (inlay_snapshot, edits) = self.inlay_map.sync(buffer_snapshot, edits);
        let (fold_snapshot, edits) = self.fold_map.read(inlay_snapshot.clone(), edits);
        let tab_size = Self::tab_size(&self.buffer, cx);
        let (tab_snapshot, edits) = self.tab_map.sync(fold_snapshot.clone(), edits, tab_size);
        let (wrap_snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(tab_snapshot.clone(), edits, cx));
        let block_snapshot = self.block_map.read(wrap_snapshot.clone(), edits).snapshot;

        DisplaySnapshot {
            buffer_snapshot: self.buffer.read(cx).snapshot(cx),
            fold_snapshot,
            inlay_snapshot,
            tab_snapshot,
            wrap_snapshot,
            block_snapshot,
            diagnostics_max_severity: self.diagnostics_max_severity,
            crease_snapshot: self.crease_map.snapshot(),
            text_highlights: self.text_highlights.clone(),
            inlay_highlights: self.inlay_highlights.clone(),
            clip_at_line_ends: self.clip_at_line_ends,
            masked: self.masked,
            fold_placeholder: self.fold_placeholder.clone(),
        }
    }

    pub fn set_state(&mut self, other: &DisplaySnapshot, cx: &mut Context<Self>) {
        self.fold(
            other
                .folds_in_range(0..other.buffer_snapshot.len())
                .map(|fold| {
                    Crease::simple(
                        fold.range.to_offset(&other.buffer_snapshot),
                        fold.placeholder.clone(),
                    )
                })
                .collect(),
            cx,
        );
    }

    /// Creates folds for the given creases.
    pub fn fold<T: Clone + ToOffset>(&mut self, creases: Vec<Crease<T>>, cx: &mut Context<Self>) {
        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);
        let (snapshot, edits) = self.inlay_map.sync(buffer_snapshot.clone(), edits);
        let (mut fold_map, snapshot, edits) = self.fold_map.write(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits);

        let inline = creases.iter().filter_map(|crease| {
            if let Crease::Inline {
                range, placeholder, ..
            } = crease
            {
                Some((range.clone(), placeholder.clone()))
            } else {
                None
            }
        });
        let (snapshot, edits) = fold_map.fold(inline);

        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        let mut block_map = self.block_map.write(snapshot, edits);
        let blocks = creases.into_iter().filter_map(|crease| {
            if let Crease::Block {
                range,
                block_height,
                render_block,
                block_style,
                block_priority,
                ..
            } = crease
            {
                Some((
                    range,
                    render_block,
                    block_height,
                    block_style,
                    block_priority,
                ))
            } else {
                None
            }
        });
        block_map.insert(
            blocks
                .into_iter()
                .map(|(range, render, height, style, priority)| {
                    let start = buffer_snapshot.anchor_before(range.start);
                    let end = buffer_snapshot.anchor_after(range.end);
                    BlockProperties {
                        placement: BlockPlacement::Replace(start..=end),
                        render,
                        height: Some(height),
                        style,
                        priority,
                        render_in_minimap: true,
                    }
                }),
        );
    }

    /// Removes any folds with the given ranges.
    pub fn remove_folds_with_type<T: ToOffset>(
        &mut self,
        ranges: impl IntoIterator<Item = Range<T>>,
        type_id: TypeId,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);
        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (mut fold_map, snapshot, edits) = self.fold_map.write(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits);
        let (snapshot, edits) = fold_map.remove_folds(ranges, type_id);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.write(snapshot, edits);
    }

    /// Removes any folds whose ranges intersect any of the given ranges.
    pub fn unfold_intersecting<T: ToOffset>(
        &mut self,
        ranges: impl IntoIterator<Item = Range<T>>,
        inclusive: bool,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let offset_ranges = ranges
            .into_iter()
            .map(|range| range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot))
            .collect::<Vec<_>>();
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);
        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (mut fold_map, snapshot, edits) = self.fold_map.write(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits);

        let (snapshot, edits) =
            fold_map.unfold_intersecting(offset_ranges.iter().cloned(), inclusive);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        let mut block_map = self.block_map.write(snapshot, edits);
        block_map.remove_intersecting_replace_blocks(offset_ranges, inclusive);
    }

    pub fn disable_header_for_buffer(&mut self, buffer_id: BufferId, cx: &mut Context<Self>) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);
        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        let mut block_map = self.block_map.write(snapshot, edits);
        block_map.disable_header_for_buffer(buffer_id)
    }

    pub fn fold_buffers(
        &mut self,
        buffer_ids: impl IntoIterator<Item = language::BufferId>,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);
        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        let mut block_map = self.block_map.write(snapshot, edits);
        block_map.fold_buffers(buffer_ids, self.buffer.read(cx), cx)
    }

    pub fn unfold_buffers(
        &mut self,
        buffer_ids: impl IntoIterator<Item = language::BufferId>,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);
        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        let mut block_map = self.block_map.write(snapshot, edits);
        block_map.unfold_buffers(buffer_ids, self.buffer.read(cx), cx)
    }

    pub(crate) fn is_buffer_folded(&self, buffer_id: language::BufferId) -> bool {
        self.block_map.folded_buffers.contains(&buffer_id)
    }

    pub(crate) fn folded_buffers(&self) -> &HashSet<BufferId> {
        &self.block_map.folded_buffers
    }

    pub fn insert_creases(
        &mut self,
        creases: impl IntoIterator<Item = Crease<Anchor>>,
        cx: &mut Context<Self>,
    ) -> Vec<CreaseId> {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        self.crease_map.insert(creases, &snapshot)
    }

    pub fn remove_creases(
        &mut self,
        crease_ids: impl IntoIterator<Item = CreaseId>,
        cx: &mut Context<Self>,
    ) -> Vec<(CreaseId, Range<Anchor>)> {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        self.crease_map.remove(crease_ids, &snapshot)
    }

    pub fn insert_blocks(
        &mut self,
        blocks: impl IntoIterator<Item = BlockProperties<Anchor>>,
        cx: &mut Context<Self>,
    ) -> Vec<CustomBlockId> {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);
        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        let mut block_map = self.block_map.write(snapshot, edits);
        block_map.insert(blocks)
    }

    pub fn resize_blocks(&mut self, heights: HashMap<CustomBlockId, u32>, cx: &mut Context<Self>) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);
        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        let mut block_map = self.block_map.write(snapshot, edits);
        block_map.resize(heights);
    }

    pub fn replace_blocks(&mut self, renderers: HashMap<CustomBlockId, RenderBlock>) {
        self.block_map.replace_blocks(renderers);
    }

    pub fn remove_blocks(&mut self, ids: HashSet<CustomBlockId>, cx: &mut Context<Self>) {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);
        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        let mut block_map = self.block_map.write(snapshot, edits);
        block_map.remove(ids);
    }

    pub fn row_for_block(
        &mut self,
        block_id: CustomBlockId,
        cx: &mut Context<Self>,
    ) -> Option<DisplayRow> {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);
        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        let block_map = self.block_map.read(snapshot, edits);
        let block_row = block_map.row_for_block(block_id)?;
        Some(DisplayRow(block_row.0))
    }

    pub fn highlight_text(
        &mut self,
        type_id: TypeId,
        ranges: Vec<Range<Anchor>>,
        style: HighlightStyle,
    ) {
        self.text_highlights
            .insert(type_id, Arc::new((style, ranges)));
    }

    pub(crate) fn highlight_inlays(
        &mut self,
        type_id: TypeId,
        highlights: Vec<InlayHighlight>,
        style: HighlightStyle,
    ) {
        for highlight in highlights {
            let update = self.inlay_highlights.update(&type_id, |highlights| {
                highlights.insert(highlight.inlay, (style, highlight.clone()))
            });
            if update.is_none() {
                self.inlay_highlights.insert(
                    type_id,
                    TreeMap::from_ordered_entries([(highlight.inlay, (style, highlight))]),
                );
            }
        }
    }

    pub fn text_highlights(&self, type_id: TypeId) -> Option<(HighlightStyle, &[Range<Anchor>])> {
        let highlights = self.text_highlights.get(&type_id)?;
        Some((highlights.0, &highlights.1))
    }
    pub fn clear_highlights(&mut self, type_id: TypeId) -> bool {
        let mut cleared = self.text_highlights.remove(&type_id).is_some();
        cleared |= self.inlay_highlights.remove(&type_id).is_some();
        cleared
    }

    pub fn set_font(&self, font: Font, font_size: Pixels, cx: &mut Context<Self>) -> bool {
        self.wrap_map
            .update(cx, |map, cx| map.set_font_with_size(font, font_size, cx))
    }

    pub fn set_wrap_width(&self, width: Option<Pixels>, cx: &mut Context<Self>) -> bool {
        self.wrap_map
            .update(cx, |map, cx| map.set_wrap_width(width, cx))
    }

    pub fn update_fold_widths(
        &mut self,
        widths: impl IntoIterator<Item = (FoldId, Pixels)>,
        cx: &mut Context<Self>,
    ) -> bool {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let tab_size = Self::tab_size(&self.buffer, cx);
        let (snapshot, edits) = self.inlay_map.sync(snapshot, edits);
        let (mut fold_map, snapshot, edits) = self.fold_map.write(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits);

        let (snapshot, edits) = fold_map.update_fold_widths(widths);
        let widths_changed = !edits.is_empty();
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits);

        widths_changed
    }

    pub(crate) fn current_inlays(&self) -> impl Iterator<Item = &Inlay> {
        self.inlay_map.current_inlays()
    }

    pub(crate) fn splice_inlays(
        &mut self,
        to_remove: &[InlayId],
        to_insert: Vec<Inlay>,
        cx: &mut Context<Self>,
    ) {
        if to_remove.is_empty() && to_insert.is_empty() {
            return;
        }
        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = self.buffer_subscription.consume().into_inner();
        let (snapshot, edits) = self.inlay_map.sync(buffer_snapshot, edits);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let tab_size = Self::tab_size(&self.buffer, cx);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits);

        let (snapshot, edits) = self.inlay_map.splice(to_remove, to_insert);
        let (snapshot, edits) = self.fold_map.read(snapshot, edits);
        let (snapshot, edits) = self.tab_map.sync(snapshot, edits, tab_size);
        let (snapshot, edits) = self
            .wrap_map
            .update(cx, |map, cx| map.sync(snapshot, edits, cx));
        self.block_map.read(snapshot, edits);
    }

    pub fn remove_inlays_for_excerpts(&mut self, excerpts_removed: &[ExcerptId]) {
        let to_remove = self
            .inlay_map
            .current_inlays()
            .filter_map(|inlay| {
                if excerpts_removed.contains(&inlay.position.excerpt_id) {
                    Some(inlay.id)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        self.inlay_map.splice(&to_remove, Vec::new());
    }

    fn tab_size(buffer: &Entity<MultiBuffer>, cx: &App) -> NonZeroU32 {
        let buffer = buffer.read(cx).as_singleton().map(|buffer| buffer.read(cx));
        let language = buffer
            .and_then(|buffer| buffer.language())
            .map(|l| l.name());
        let file = buffer.and_then(|buffer| buffer.file());
        language_settings(language, file, cx).tab_size
    }
}

#[derive(Debug, Default)]
pub(crate) struct Highlights<'a> {
    pub text_highlights: Option<&'a TextHighlights>,
    pub inlay_highlights: Option<&'a InlayHighlights>,
    pub styles: HighlightStyles,
}

#[derive(Clone, Copy, Debug)]
pub struct InlineCompletionStyles {
    pub insertion: HighlightStyle,
    pub whitespace: HighlightStyle,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct HighlightStyles {
    pub inlay_hint: Option<HighlightStyle>,
    pub inline_completion: Option<InlineCompletionStyles>,
}

#[derive(Clone)]
pub enum ChunkReplacement {
    Renderer(ChunkRenderer),
    Str(SharedString),
}

pub struct HighlightedChunk<'a> {
    pub text: &'a str,
    pub style: Option<HighlightStyle>,
    pub is_tab: bool,
    pub is_inlay: bool,
    pub replacement: Option<ChunkReplacement>,
}

impl<'a> HighlightedChunk<'a> {
    fn highlight_invisibles(
        self,
        editor_style: &'a EditorStyle,
    ) -> impl Iterator<Item = Self> + 'a {
        let mut chars = self.text.chars().peekable();
        let mut text = self.text;
        let style = self.style;
        let is_tab = self.is_tab;
        let renderer = self.replacement;
        let is_inlay = self.is_inlay;
        iter::from_fn(move || {
            let mut prefix_len = 0;
            while let Some(&ch) = chars.peek() {
                if !is_invisible(ch) {
                    prefix_len += ch.len_utf8();
                    chars.next();
                    continue;
                }
                if prefix_len > 0 {
                    let (prefix, suffix) = text.split_at(prefix_len);
                    text = suffix;
                    return Some(HighlightedChunk {
                        text: prefix,
                        style,
                        is_tab,
                        is_inlay,
                        replacement: renderer.clone(),
                    });
                }
                chars.next();
                let (prefix, suffix) = text.split_at(ch.len_utf8());
                text = suffix;
                if let Some(replacement) = replacement(ch) {
                    let invisible_highlight = HighlightStyle {
                        background_color: Some(editor_style.status.hint_background),
                        underline: Some(UnderlineStyle {
                            color: Some(editor_style.status.hint),
                            thickness: px(1.),
                            wavy: false,
                        }),
                        ..Default::default()
                    };
                    let invisible_style = if let Some(mut style) = style {
                        style.highlight(invisible_highlight);
                        style
                    } else {
                        invisible_highlight
                    };
                    return Some(HighlightedChunk {
                        text: prefix,
                        style: Some(invisible_style),
                        is_tab: false,
                        is_inlay,
                        replacement: Some(ChunkReplacement::Str(replacement.into())),
                    });
                } else {
                    let invisible_highlight = HighlightStyle {
                        background_color: Some(editor_style.status.hint_background),
                        underline: Some(UnderlineStyle {
                            color: Some(editor_style.status.hint),
                            thickness: px(1.),
                            wavy: false,
                        }),
                        ..Default::default()
                    };
                    let invisible_style = if let Some(mut style) = style {
                        style.highlight(invisible_highlight);
                        style
                    } else {
                        invisible_highlight
                    };

                    return Some(HighlightedChunk {
                        text: prefix,
                        style: Some(invisible_style),
                        is_tab: false,
                        is_inlay,
                        replacement: renderer.clone(),
                    });
                }
            }

            if !text.is_empty() {
                let remainder = text;
                text = "";
                Some(HighlightedChunk {
                    text: remainder,
                    style,
                    is_tab,
                    is_inlay,
                    replacement: renderer.clone(),
                })
            } else {
                None
            }
        })
    }
}

#[derive(Clone)]
pub struct DisplaySnapshot {
    pub buffer_snapshot: MultiBufferSnapshot,
    pub fold_snapshot: FoldSnapshot,
    pub crease_snapshot: CreaseSnapshot,
    inlay_snapshot: InlaySnapshot,
    tab_snapshot: TabSnapshot,
    wrap_snapshot: WrapSnapshot,
    block_snapshot: BlockSnapshot,
    text_highlights: TextHighlights,
    inlay_highlights: InlayHighlights,
    clip_at_line_ends: bool,
    masked: bool,
    diagnostics_max_severity: DiagnosticSeverity,
    pub(crate) fold_placeholder: FoldPlaceholder,
}

impl DisplaySnapshot {
    pub fn is_empty(&self) -> bool {
        self.buffer_snapshot.len() == 0
    }

    pub fn row_infos(&self, start_row: DisplayRow) -> impl Iterator<Item = RowInfo> + '_ {
        self.block_snapshot.row_infos(BlockRow(start_row.0))
    }

    pub fn widest_line_number(&self) -> u32 {
        self.buffer_snapshot.widest_line_number()
    }

    pub fn prev_line_boundary(&self, mut point: MultiBufferPoint) -> (Point, DisplayPoint) {
        loop {
            let mut inlay_point = self.inlay_snapshot.to_inlay_point(point);
            let mut fold_point = self.fold_snapshot.to_fold_point(inlay_point, Bias::Left);
            fold_point.0.column = 0;
            inlay_point = fold_point.to_inlay_point(&self.fold_snapshot);
            point = self.inlay_snapshot.to_buffer_point(inlay_point);

            let mut display_point = self.point_to_display_point(point, Bias::Left);
            *display_point.column_mut() = 0;
            let next_point = self.display_point_to_point(display_point, Bias::Left);
            if next_point == point {
                return (point, display_point);
            }
            point = next_point;
        }
    }

    pub fn next_line_boundary(
        &self,
        mut point: MultiBufferPoint,
    ) -> (MultiBufferPoint, DisplayPoint) {
        let original_point = point;
        loop {
            let mut inlay_point = self.inlay_snapshot.to_inlay_point(point);
            let mut fold_point = self.fold_snapshot.to_fold_point(inlay_point, Bias::Right);
            fold_point.0.column = self.fold_snapshot.line_len(fold_point.row());
            inlay_point = fold_point.to_inlay_point(&self.fold_snapshot);
            point = self.inlay_snapshot.to_buffer_point(inlay_point);

            let mut display_point = self.point_to_display_point(point, Bias::Right);
            *display_point.column_mut() = self.line_len(display_point.row());
            let next_point = self.display_point_to_point(display_point, Bias::Right);
            if next_point == point || original_point == point || original_point == next_point {
                return (point, display_point);
            }
            point = next_point;
        }
    }

    // used by line_mode selections
    pub fn expand_to_line(&self, range: Range<Point>) -> Range<Point> {
        let new_start = MultiBufferPoint::new(range.start.row, 0);
        let new_end = if range.end.column > 0 {
            MultiBufferPoint::new(
                range.end.row,
                self.buffer_snapshot.line_len(MultiBufferRow(range.end.row)),
            )
        } else {
            range.end
        };

        new_start..new_end
    }

    pub fn point_to_display_point(&self, point: MultiBufferPoint, bias: Bias) -> DisplayPoint {
        let inlay_point = self.inlay_snapshot.to_inlay_point(point);
        let fold_point = self.fold_snapshot.to_fold_point(inlay_point, bias);
        let tab_point = self.tab_snapshot.to_tab_point(fold_point);
        let wrap_point = self.wrap_snapshot.tab_point_to_wrap_point(tab_point);
        let block_point = self.block_snapshot.to_block_point(wrap_point);
        DisplayPoint(block_point)
    }

    pub fn display_point_to_point(&self, point: DisplayPoint, bias: Bias) -> Point {
        self.inlay_snapshot
            .to_buffer_point(self.display_point_to_inlay_point(point, bias))
    }

    pub fn display_point_to_inlay_offset(&self, point: DisplayPoint, bias: Bias) -> InlayOffset {
        self.inlay_snapshot
            .to_offset(self.display_point_to_inlay_point(point, bias))
    }

    pub fn anchor_to_inlay_offset(&self, anchor: Anchor) -> InlayOffset {
        self.inlay_snapshot
            .to_inlay_offset(anchor.to_offset(&self.buffer_snapshot))
    }

    pub fn display_point_to_anchor(&self, point: DisplayPoint, bias: Bias) -> Anchor {
        self.buffer_snapshot
            .anchor_at(point.to_offset(self, bias), bias)
    }

    fn display_point_to_inlay_point(&self, point: DisplayPoint, bias: Bias) -> InlayPoint {
        let block_point = point.0;
        let wrap_point = self.block_snapshot.to_wrap_point(block_point, bias);
        let tab_point = self.wrap_snapshot.to_tab_point(wrap_point);
        let fold_point = self.tab_snapshot.to_fold_point(tab_point, bias).0;
        fold_point.to_inlay_point(&self.fold_snapshot)
    }

    pub fn display_point_to_fold_point(&self, point: DisplayPoint, bias: Bias) -> FoldPoint {
        let block_point = point.0;
        let wrap_point = self.block_snapshot.to_wrap_point(block_point, bias);
        let tab_point = self.wrap_snapshot.to_tab_point(wrap_point);
        self.tab_snapshot.to_fold_point(tab_point, bias).0
    }

    pub fn fold_point_to_display_point(&self, fold_point: FoldPoint) -> DisplayPoint {
        let tab_point = self.tab_snapshot.to_tab_point(fold_point);
        let wrap_point = self.wrap_snapshot.tab_point_to_wrap_point(tab_point);
        let block_point = self.block_snapshot.to_block_point(wrap_point);
        DisplayPoint(block_point)
    }

    pub fn max_point(&self) -> DisplayPoint {
        DisplayPoint(self.block_snapshot.max_point())
    }

    /// Returns text chunks starting at the given display row until the end of the file
    pub fn text_chunks(&self, display_row: DisplayRow) -> impl Iterator<Item = &str> {
        self.block_snapshot
            .chunks(
                display_row.0..self.max_point().row().next_row().0,
                false,
                self.masked,
                Highlights::default(),
            )
            .map(|h| h.text)
    }

    /// Returns text chunks starting at the end of the given display row in reverse until the start of the file
    pub fn reverse_text_chunks(&self, display_row: DisplayRow) -> impl Iterator<Item = &str> {
        (0..=display_row.0).rev().flat_map(move |row| {
            self.block_snapshot
                .chunks(row..row + 1, false, self.masked, Highlights::default())
                .map(|h| h.text)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
        })
    }

    pub fn chunks(
        &self,
        display_rows: Range<DisplayRow>,
        language_aware: bool,
        highlight_styles: HighlightStyles,
    ) -> DisplayChunks<'_> {
        self.block_snapshot.chunks(
            display_rows.start.0..display_rows.end.0,
            language_aware,
            self.masked,
            Highlights {
                text_highlights: Some(&self.text_highlights),
                inlay_highlights: Some(&self.inlay_highlights),
                styles: highlight_styles,
            },
        )
    }

    pub fn highlighted_chunks<'a>(
        &'a self,
        display_rows: Range<DisplayRow>,
        language_aware: bool,
        editor_style: &'a EditorStyle,
    ) -> impl Iterator<Item = HighlightedChunk<'a>> {
        self.chunks(
            display_rows,
            language_aware,
            HighlightStyles {
                inlay_hint: Some(editor_style.inlay_hints_style),
                inline_completion: Some(editor_style.inline_completion_styles),
            },
        )
        .flat_map(|chunk| {
            let mut highlight_style = chunk
                .syntax_highlight_id
                .and_then(|id| id.style(&editor_style.syntax));

            if let Some(chunk_highlight) = chunk.highlight_style {
                if let Some(highlight_style) = highlight_style.as_mut() {
                    highlight_style.highlight(chunk_highlight);
                } else {
                    highlight_style = Some(chunk_highlight);
                }
            }

            let mut diagnostic_highlight = HighlightStyle::default();

            if let Some(severity) = chunk.diagnostic_severity.filter(|severity| {
                self.diagnostics_max_severity
                    .into_lsp()
                    .map_or(false, |max_severity| severity <= &max_severity)
            }) {
                if chunk.is_unnecessary {
                    diagnostic_highlight.fade_out = Some(editor_style.unnecessary_code_fade);
                }
                if chunk.underline
                    && editor_style.show_underlines
                    && !(chunk.is_unnecessary && severity > lsp::DiagnosticSeverity::WARNING)
                {
                    let diagnostic_color = super::diagnostic_style(severity, &editor_style.status);
                    diagnostic_highlight.underline = Some(UnderlineStyle {
                        color: Some(diagnostic_color),
                        thickness: 1.0.into(),
                        wavy: true,
                    });
                }
            }

            if let Some(highlight_style) = highlight_style.as_mut() {
                highlight_style.highlight(diagnostic_highlight);
            } else {
                highlight_style = Some(diagnostic_highlight);
            }

            HighlightedChunk {
                text: chunk.text,
                style: highlight_style,
                is_tab: chunk.is_tab,
                is_inlay: chunk.is_inlay,
                replacement: chunk.renderer.map(ChunkReplacement::Renderer),
            }
            .highlight_invisibles(editor_style)
        })
    }

    pub fn layout_row(
        &self,
        display_row: DisplayRow,
        TextLayoutDetails {
            text_system,
            editor_style,
            rem_size,
            scroll_anchor: _,
            visible_rows: _,
            vertical_scroll_margin: _,
        }: &TextLayoutDetails,
    ) -> Arc<LineLayout> {
        let mut runs = Vec::new();
        let mut line = String::new();

        let range = display_row..display_row.next_row();
        for chunk in self.highlighted_chunks(range, false, editor_style) {
            line.push_str(chunk.text);

            let text_style = if let Some(style) = chunk.style {
                Cow::Owned(editor_style.text.clone().highlight(style))
            } else {
                Cow::Borrowed(&editor_style.text)
            };

            runs.push(text_style.to_run(chunk.text.len()))
        }

        if line.ends_with('\n') {
            line.pop();
            if let Some(last_run) = runs.last_mut() {
                last_run.len -= 1;
                if last_run.len == 0 {
                    runs.pop();
                }
            }
        }

        let font_size = editor_style.text.font_size.to_pixels(*rem_size);
        text_system.layout_line(&line, font_size, &runs)
    }

    pub fn x_for_display_point(
        &self,
        display_point: DisplayPoint,
        text_layout_details: &TextLayoutDetails,
    ) -> Pixels {
        let line = self.layout_row(display_point.row(), text_layout_details);
        line.x_for_index(display_point.column() as usize)
    }

    pub fn display_column_for_x(
        &self,
        display_row: DisplayRow,
        x: Pixels,
        details: &TextLayoutDetails,
    ) -> u32 {
        let layout_line = self.layout_row(display_row, details);
        layout_line.closest_index_for_x(x) as u32
    }

    pub fn grapheme_at(&self, mut point: DisplayPoint) -> Option<SharedString> {
        point = DisplayPoint(self.block_snapshot.clip_point(point.0, Bias::Left));
        let chars = self
            .text_chunks(point.row())
            .flat_map(str::chars)
            .skip_while({
                let mut column = 0;
                move |char| {
                    let at_point = column >= point.column();
                    column += char.len_utf8() as u32;
                    !at_point
                }
            })
            .take_while({
                let mut prev = false;
                move |char| {
                    let now = char.is_ascii();
                    let end = char.is_ascii() && (char.is_ascii_whitespace() || prev);
                    prev = now;
                    !end
                }
            });
        chars.collect::<String>().graphemes(true).next().map(|s| {
            if let Some(invisible) = s.chars().next().filter(|&c| is_invisible(c)) {
                replacement(invisible).unwrap_or(s).to_owned().into()
            } else if s == "\n" {
                " ".into()
            } else {
                s.to_owned().into()
            }
        })
    }

    pub fn buffer_chars_at(&self, mut offset: usize) -> impl Iterator<Item = (char, usize)> + '_ {
        self.buffer_snapshot.chars_at(offset).map(move |ch| {
            let ret = (ch, offset);
            offset += ch.len_utf8();
            ret
        })
    }

    pub fn reverse_buffer_chars_at(
        &self,
        mut offset: usize,
    ) -> impl Iterator<Item = (char, usize)> + '_ {
        self.buffer_snapshot
            .reversed_chars_at(offset)
            .map(move |ch| {
                offset -= ch.len_utf8();
                (ch, offset)
            })
    }

    pub fn clip_point(&self, point: DisplayPoint, bias: Bias) -> DisplayPoint {
        let mut clipped = self.block_snapshot.clip_point(point.0, bias);
        if self.clip_at_line_ends {
            clipped = self.clip_at_line_end(DisplayPoint(clipped)).0
        }
        DisplayPoint(clipped)
    }

    pub fn clip_ignoring_line_ends(&self, point: DisplayPoint, bias: Bias) -> DisplayPoint {
        DisplayPoint(self.block_snapshot.clip_point(point.0, bias))
    }

    pub fn clip_at_line_end(&self, display_point: DisplayPoint) -> DisplayPoint {
        let mut point = self.display_point_to_point(display_point, Bias::Left);

        if point.column != self.buffer_snapshot.line_len(MultiBufferRow(point.row)) {
            return display_point;
        }
        point.column = point.column.saturating_sub(1);
        point = self.buffer_snapshot.clip_point(point, Bias::Left);
        self.point_to_display_point(point, Bias::Left)
    }

    pub fn folds_in_range<T>(&self, range: Range<T>) -> impl Iterator<Item = &Fold>
    where
        T: ToOffset,
    {
        self.fold_snapshot.folds_in_range(range)
    }

    pub fn blocks_in_range(
        &self,
        rows: Range<DisplayRow>,
    ) -> impl Iterator<Item = (DisplayRow, &Block)> {
        self.block_snapshot
            .blocks_in_range(rows.start.0..rows.end.0)
            .map(|(row, block)| (DisplayRow(row), block))
    }

    pub fn sticky_header_excerpt(&self, row: f32) -> Option<StickyHeaderExcerpt<'_>> {
        self.block_snapshot.sticky_header_excerpt(row)
    }

    pub fn block_for_id(&self, id: BlockId) -> Option<Block> {
        self.block_snapshot.block_for_id(id)
    }

    pub fn intersects_fold<T: ToOffset>(&self, offset: T) -> bool {
        self.fold_snapshot.intersects_fold(offset)
    }

    pub fn is_line_folded(&self, buffer_row: MultiBufferRow) -> bool {
        self.block_snapshot.is_line_replaced(buffer_row)
            || self.fold_snapshot.is_line_folded(buffer_row)
    }

    pub fn is_block_line(&self, display_row: DisplayRow) -> bool {
        self.block_snapshot.is_block_line(BlockRow(display_row.0))
    }

    pub fn is_folded_buffer_header(&self, display_row: DisplayRow) -> bool {
        self.block_snapshot
            .is_folded_buffer_header(BlockRow(display_row.0))
    }

    pub fn soft_wrap_indent(&self, display_row: DisplayRow) -> Option<u32> {
        let wrap_row = self
            .block_snapshot
            .to_wrap_point(BlockPoint::new(display_row.0, 0), Bias::Left)
            .row();
        self.wrap_snapshot.soft_wrap_indent(wrap_row)
    }

    pub fn text(&self) -> String {
        self.text_chunks(DisplayRow(0)).collect()
    }

    pub fn line(&self, display_row: DisplayRow) -> String {
        let mut result = String::new();
        for chunk in self.text_chunks(display_row) {
            if let Some(ix) = chunk.find('\n') {
                result.push_str(&chunk[0..ix]);
                break;
            } else {
                result.push_str(chunk);
            }
        }
        result
    }

    pub fn line_indent_for_buffer_row(&self, buffer_row: MultiBufferRow) -> LineIndent {
        self.buffer_snapshot.line_indent_for_row(buffer_row)
    }

    pub fn line_len(&self, row: DisplayRow) -> u32 {
        self.block_snapshot.line_len(BlockRow(row.0))
    }

    pub fn longest_row(&self) -> DisplayRow {
        DisplayRow(self.block_snapshot.longest_row())
    }

    pub fn longest_row_in_range(&self, range: Range<DisplayRow>) -> DisplayRow {
        let block_range = BlockRow(range.start.0)..BlockRow(range.end.0);
        let longest_row = self.block_snapshot.longest_row_in_range(block_range);
        DisplayRow(longest_row.0)
    }

    pub fn starts_indent(&self, buffer_row: MultiBufferRow) -> bool {
        let max_row = self.buffer_snapshot.max_row();
        if buffer_row >= max_row {
            return false;
        }

        let line_indent = self.line_indent_for_buffer_row(buffer_row);
        if line_indent.is_line_blank() {
            return false;
        }

        (buffer_row.0 + 1..=max_row.0)
            .find_map(|next_row| {
                let next_line_indent = self.line_indent_for_buffer_row(MultiBufferRow(next_row));
                if next_line_indent.raw_len() > line_indent.raw_len() {
                    Some(true)
                } else if !next_line_indent.is_line_blank() {
                    Some(false)
                } else {
                    None
                }
            })
            .unwrap_or(false)
    }

    pub fn crease_for_buffer_row(&self, buffer_row: MultiBufferRow) -> Option<Crease<Point>> {
        let start = MultiBufferPoint::new(buffer_row.0, self.buffer_snapshot.line_len(buffer_row));
        if let Some(crease) = self
            .crease_snapshot
            .query_row(buffer_row, &self.buffer_snapshot)
        {
            match crease {
                Crease::Inline {
                    range,
                    placeholder,
                    render_toggle,
                    render_trailer,
                    metadata,
                } => Some(Crease::Inline {
                    range: range.to_point(&self.buffer_snapshot),
                    placeholder: placeholder.clone(),
                    render_toggle: render_toggle.clone(),
                    render_trailer: render_trailer.clone(),
                    metadata: metadata.clone(),
                }),
                Crease::Block {
                    range,
                    block_height,
                    block_style,
                    render_block,
                    block_priority,
                    render_toggle,
                } => Some(Crease::Block {
                    range: range.to_point(&self.buffer_snapshot),
                    block_height: *block_height,
                    block_style: *block_style,
                    render_block: render_block.clone(),
                    block_priority: *block_priority,
                    render_toggle: render_toggle.clone(),
                }),
            }
        } else if self.starts_indent(MultiBufferRow(start.row))
            && !self.is_line_folded(MultiBufferRow(start.row))
        {
            let start_line_indent = self.line_indent_for_buffer_row(buffer_row);
            let max_point = self.buffer_snapshot.max_point();
            let mut end = None;

            for row in (buffer_row.0 + 1)..=max_point.row {
                let line_indent = self.line_indent_for_buffer_row(MultiBufferRow(row));
                if !line_indent.is_line_blank()
                    && line_indent.raw_len() <= start_line_indent.raw_len()
                {
                    let prev_row = row - 1;
                    end = Some(Point::new(
                        prev_row,
                        self.buffer_snapshot.line_len(MultiBufferRow(prev_row)),
                    ));
                    break;
                }
            }

            let mut row_before_line_breaks = end.unwrap_or(max_point);
            while row_before_line_breaks.row > start.row
                && self
                    .buffer_snapshot
                    .is_line_blank(MultiBufferRow(row_before_line_breaks.row))
            {
                row_before_line_breaks.row -= 1;
            }

            row_before_line_breaks = Point::new(
                row_before_line_breaks.row,
                self.buffer_snapshot
                    .line_len(MultiBufferRow(row_before_line_breaks.row)),
            );

            Some(Crease::Inline {
                range: start..row_before_line_breaks,
                placeholder: self.fold_placeholder.clone(),
                render_toggle: None,
                render_trailer: None,
                metadata: None,
            })
        } else {
            None
        }
    }

    pub fn buffer_header_height(&self) -> u32 {
        self.block_snapshot.buffer_header_height
    }

    pub fn excerpt_header_height(&self) -> u32 {
        self.block_snapshot.excerpt_header_height
    }
}

#[derive(Copy, Clone, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct DisplayPoint(BlockPoint);

impl Debug for DisplayPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "DisplayPoint({}, {})",
            self.row().0,
            self.column()
        ))
    }
}

impl Add for DisplayPoint {
    type Output = Self;

    fn add(self, other: Self) -> Self::Output {
        DisplayPoint(BlockPoint(self.0.0 + other.0.0))
    }
}

impl Sub for DisplayPoint {
    type Output = Self;

    fn sub(self, other: Self) -> Self::Output {
        DisplayPoint(BlockPoint(self.0.0 - other.0.0))
    }
}

#[derive(Debug, Copy, Clone, Default, Eq, Ord, PartialOrd, PartialEq, Deserialize, Hash)]
#[serde(transparent)]
pub struct DisplayRow(pub u32);

impl Add<DisplayRow> for DisplayRow {
    type Output = Self;

    fn add(self, other: Self) -> Self::Output {
        DisplayRow(self.0 + other.0)
    }
}

impl Add<u32> for DisplayRow {
    type Output = Self;

    fn add(self, other: u32) -> Self::Output {
        DisplayRow(self.0 + other)
    }
}

impl Sub<DisplayRow> for DisplayRow {
    type Output = Self;

    fn sub(self, other: Self) -> Self::Output {
        DisplayRow(self.0 - other.0)
    }
}

impl Sub<u32> for DisplayRow {
    type Output = Self;

    fn sub(self, other: u32) -> Self::Output {
        DisplayRow(self.0 - other)
    }
}

impl DisplayPoint {
    pub fn new(row: DisplayRow, column: u32) -> Self {
        Self(BlockPoint(Point::new(row.0, column)))
    }

    pub fn zero() -> Self {
        Self::new(DisplayRow(0), 0)
    }

    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    pub fn row(self) -> DisplayRow {
        DisplayRow(self.0.row)
    }

    pub fn column(self) -> u32 {
        self.0.column
    }

    pub fn row_mut(&mut self) -> &mut u32 {
        &mut self.0.row
    }

    pub fn column_mut(&mut self) -> &mut u32 {
        &mut self.0.column
    }

    pub fn to_point(self, map: &DisplaySnapshot) -> Point {
        map.display_point_to_point(self, Bias::Left)
    }

    pub fn to_offset(self, map: &DisplaySnapshot, bias: Bias) -> usize {
        let wrap_point = map.block_snapshot.to_wrap_point(self.0, bias);
        let tab_point = map.wrap_snapshot.to_tab_point(wrap_point);
        let fold_point = map.tab_snapshot.to_fold_point(tab_point, bias).0;
        let inlay_point = fold_point.to_inlay_point(&map.fold_snapshot);
        map.inlay_snapshot
            .to_buffer_offset(map.inlay_snapshot.to_offset(inlay_point))
    }
}

impl ToDisplayPoint for usize {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint {
        map.point_to_display_point(self.to_point(&map.buffer_snapshot), Bias::Left)
    }
}

impl ToDisplayPoint for OffsetUtf16 {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint {
        self.to_offset(&map.buffer_snapshot).to_display_point(map)
    }
}

impl ToDisplayPoint for Point {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint {
        map.point_to_display_point(*self, Bias::Left)
    }
}

impl ToDisplayPoint for Anchor {
    fn to_display_point(&self, map: &DisplaySnapshot) -> DisplayPoint {
        self.to_point(&map.buffer_snapshot).to_display_point(map)
    }
}
