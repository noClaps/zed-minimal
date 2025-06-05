use super::{
    Highlights,
    fold_map::Chunk,
    wrap_map::{self, WrapEdit, WrapPoint, WrapSnapshot},
};
use crate::{EditorStyle, GutterDimensions};
use collections::{Bound, HashMap, HashSet};
use gpui::{AnyElement, App, EntityId, Pixels, Window};
use language::{Patch, Point};
use multi_buffer::{
    Anchor, ExcerptId, ExcerptInfo, MultiBuffer, MultiBufferRow, MultiBufferSnapshot, RowInfo,
    ToOffset, ToPoint as _,
};
use parking_lot::Mutex;
use std::{
    cell::RefCell,
    cmp::{self, Ordering},
    fmt::Debug,
    ops::{Deref, DerefMut, Range, RangeBounds, RangeInclusive},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
};
use sum_tree::{Bias, SumTree, Summary, TreeMap};
use text::{BufferId, Edit};
use ui::ElementId;

const NEWLINES: &[u8] = &[b'\n'; u8::MAX as usize];
const BULLETS: &str = "********************************************************************************************************************************";

/// Tracks custom blocks such as diagnostics that should be displayed within buffer.
///
/// See the [`display_map` module documentation](crate::display_map) for more information.
pub struct BlockMap {
    next_block_id: AtomicUsize,
    wrap_snapshot: RefCell<WrapSnapshot>,
    custom_blocks: Vec<Arc<CustomBlock>>,
    custom_blocks_by_id: TreeMap<CustomBlockId, Arc<CustomBlock>>,
    transforms: RefCell<SumTree<Transform>>,
    buffer_header_height: u32,
    excerpt_header_height: u32,
    pub(super) folded_buffers: HashSet<BufferId>,
    buffers_with_disabled_headers: HashSet<BufferId>,
}

pub struct BlockMapReader<'a> {
    blocks: &'a Vec<Arc<CustomBlock>>,
    pub snapshot: BlockSnapshot,
}

pub struct BlockMapWriter<'a>(&'a mut BlockMap);

#[derive(Clone)]
pub struct BlockSnapshot {
    wrap_snapshot: WrapSnapshot,
    transforms: SumTree<Transform>,
    custom_blocks_by_id: TreeMap<CustomBlockId, Arc<CustomBlock>>,
    pub(super) buffer_header_height: u32,
    pub(super) excerpt_header_height: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CustomBlockId(pub usize);

impl From<CustomBlockId> for ElementId {
    fn from(val: CustomBlockId) -> Self {
        val.0.into()
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct BlockPoint(pub Point);

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
pub struct BlockRow(pub(super) u32);

#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialOrd, PartialEq)]
struct WrapRow(u32);

pub type RenderBlock = Arc<dyn Send + Sync + Fn(&mut BlockContext) -> AnyElement>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlockPlacement<T> {
    Above(T),
    Below(T),
    Near(T),
    Replace(RangeInclusive<T>),
}

impl<T> BlockPlacement<T> {
    pub fn start(&self) -> &T {
        match self {
            BlockPlacement::Above(position) => position,
            BlockPlacement::Below(position) => position,
            BlockPlacement::Near(position) => position,
            BlockPlacement::Replace(range) => range.start(),
        }
    }

    fn end(&self) -> &T {
        match self {
            BlockPlacement::Above(position) => position,
            BlockPlacement::Below(position) => position,
            BlockPlacement::Near(position) => position,
            BlockPlacement::Replace(range) => range.end(),
        }
    }

    pub fn as_ref(&self) -> BlockPlacement<&T> {
        match self {
            BlockPlacement::Above(position) => BlockPlacement::Above(position),
            BlockPlacement::Below(position) => BlockPlacement::Below(position),
            BlockPlacement::Near(position) => BlockPlacement::Near(position),
            BlockPlacement::Replace(range) => BlockPlacement::Replace(range.start()..=range.end()),
        }
    }

    pub fn map<R>(self, mut f: impl FnMut(T) -> R) -> BlockPlacement<R> {
        match self {
            BlockPlacement::Above(position) => BlockPlacement::Above(f(position)),
            BlockPlacement::Below(position) => BlockPlacement::Below(f(position)),
            BlockPlacement::Near(position) => BlockPlacement::Near(f(position)),
            BlockPlacement::Replace(range) => {
                let (start, end) = range.into_inner();
                BlockPlacement::Replace(f(start)..=f(end))
            }
        }
    }

    fn sort_order(&self) -> u8 {
        match self {
            BlockPlacement::Above(_) => 0,
            BlockPlacement::Replace(_) => 1,
            BlockPlacement::Near(_) => 2,
            BlockPlacement::Below(_) => 3,
        }
    }
}

impl BlockPlacement<Anchor> {
    fn cmp(&self, other: &Self, buffer: &MultiBufferSnapshot) -> Ordering {
        self.start()
            .cmp(other.start(), buffer)
            .then_with(|| other.end().cmp(self.end(), buffer))
            .then_with(|| self.sort_order().cmp(&other.sort_order()))
    }

    fn to_wrap_row(&self, wrap_snapshot: &WrapSnapshot) -> Option<BlockPlacement<WrapRow>> {
        let buffer_snapshot = wrap_snapshot.buffer_snapshot();
        match self {
            BlockPlacement::Above(position) => {
                let mut position = position.to_point(buffer_snapshot);
                position.column = 0;
                let wrap_row = WrapRow(wrap_snapshot.make_wrap_point(position, Bias::Left).row());
                Some(BlockPlacement::Above(wrap_row))
            }
            BlockPlacement::Near(position) => {
                let mut position = position.to_point(buffer_snapshot);
                position.column = buffer_snapshot.line_len(MultiBufferRow(position.row));
                let wrap_row = WrapRow(wrap_snapshot.make_wrap_point(position, Bias::Left).row());
                Some(BlockPlacement::Near(wrap_row))
            }
            BlockPlacement::Below(position) => {
                let mut position = position.to_point(buffer_snapshot);
                position.column = buffer_snapshot.line_len(MultiBufferRow(position.row));
                let wrap_row = WrapRow(wrap_snapshot.make_wrap_point(position, Bias::Left).row());
                Some(BlockPlacement::Below(wrap_row))
            }
            BlockPlacement::Replace(range) => {
                let mut start = range.start().to_point(buffer_snapshot);
                let mut end = range.end().to_point(buffer_snapshot);
                if start == end {
                    None
                } else {
                    start.column = 0;
                    let start_wrap_row =
                        WrapRow(wrap_snapshot.make_wrap_point(start, Bias::Left).row());
                    end.column = buffer_snapshot.line_len(MultiBufferRow(end.row));
                    let end_wrap_row =
                        WrapRow(wrap_snapshot.make_wrap_point(end, Bias::Left).row());
                    Some(BlockPlacement::Replace(start_wrap_row..=end_wrap_row))
                }
            }
        }
    }
}

pub struct CustomBlock {
    pub id: CustomBlockId,
    pub placement: BlockPlacement<Anchor>,
    pub height: Option<u32>,
    style: BlockStyle,
    render: Arc<Mutex<RenderBlock>>,
    priority: usize,
    pub(crate) render_in_minimap: bool,
}

#[derive(Clone)]
pub struct BlockProperties<P> {
    pub placement: BlockPlacement<P>,
    // None if the block takes up no space
    // (e.g. a horizontal line)
    pub height: Option<u32>,
    pub style: BlockStyle,
    pub render: RenderBlock,
    pub priority: usize,
    pub render_in_minimap: bool,
}

impl<P: Debug> Debug for BlockProperties<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockProperties")
            .field("placement", &self.placement)
            .field("height", &self.height)
            .field("style", &self.style)
            .finish()
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub enum BlockStyle {
    Fixed,
    Flex,
    Sticky,
}

#[derive(Debug, Default, Copy, Clone)]
pub struct EditorMargins {
    pub gutter: GutterDimensions,
    pub right: Pixels,
}

#[derive(gpui::AppContext, gpui::VisualContext)]
pub struct BlockContext<'a, 'b> {
    #[window]
    pub window: &'a mut Window,
    #[app]
    pub app: &'b mut App,
    pub anchor_x: Pixels,
    pub max_width: Pixels,
    pub margins: &'b EditorMargins,
    pub em_width: Pixels,
    pub line_height: Pixels,
    pub block_id: BlockId,
    pub selected: bool,
    pub editor_style: &'b EditorStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub enum BlockId {
    ExcerptBoundary(ExcerptId),
    FoldedBuffer(ExcerptId),
    Custom(CustomBlockId),
}

impl From<BlockId> for ElementId {
    fn from(value: BlockId) -> Self {
        match value {
            BlockId::Custom(CustomBlockId(id)) => ("Block", id).into(),
            BlockId::ExcerptBoundary(excerpt_id) => {
                ("ExcerptBoundary", EntityId::from(excerpt_id)).into()
            }
            BlockId::FoldedBuffer(id) => ("FoldedBuffer", EntityId::from(id)).into(),
        }
    }
}

impl std::fmt::Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Custom(id) => write!(f, "Block({id:?})"),
            Self::ExcerptBoundary(id) => write!(f, "ExcerptHeader({id:?})"),
            Self::FoldedBuffer(id) => write!(f, "FoldedBuffer({id:?})"),
        }
    }
}

#[derive(Clone, Debug)]
struct Transform {
    summary: TransformSummary,
    block: Option<Block>,
}

#[derive(Clone)]
pub enum Block {
    Custom(Arc<CustomBlock>),
    FoldedBuffer {
        first_excerpt: ExcerptInfo,
        height: u32,
    },
    ExcerptBoundary {
        excerpt: ExcerptInfo,
        height: u32,
        starts_new_buffer: bool,
    },
}

impl Block {
    pub fn id(&self) -> BlockId {
        match self {
            Block::Custom(block) => BlockId::Custom(block.id),
            Block::ExcerptBoundary {
                excerpt: next_excerpt,
                ..
            } => BlockId::ExcerptBoundary(next_excerpt.id),
            Block::FoldedBuffer { first_excerpt, .. } => BlockId::FoldedBuffer(first_excerpt.id),
        }
    }

    pub fn has_height(&self) -> bool {
        match self {
            Block::Custom(block) => block.height.is_some(),
            Block::ExcerptBoundary { .. } | Block::FoldedBuffer { .. } => true,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            Block::Custom(block) => block.height.unwrap_or(0),
            Block::ExcerptBoundary { height, .. } | Block::FoldedBuffer { height, .. } => *height,
        }
    }

    pub fn style(&self) -> BlockStyle {
        match self {
            Block::Custom(block) => block.style,
            Block::ExcerptBoundary { .. } | Block::FoldedBuffer { .. } => BlockStyle::Sticky,
        }
    }

    fn place_above(&self) -> bool {
        match self {
            Block::Custom(block) => matches!(block.placement, BlockPlacement::Above(_)),
            Block::FoldedBuffer { .. } => false,
            Block::ExcerptBoundary { .. } => true,
        }
    }

    pub fn place_near(&self) -> bool {
        match self {
            Block::Custom(block) => matches!(block.placement, BlockPlacement::Near(_)),
            Block::FoldedBuffer { .. } => false,
            Block::ExcerptBoundary { .. } => false,
        }
    }

    fn place_below(&self) -> bool {
        match self {
            Block::Custom(block) => matches!(
                block.placement,
                BlockPlacement::Below(_) | BlockPlacement::Near(_)
            ),
            Block::FoldedBuffer { .. } => false,
            Block::ExcerptBoundary { .. } => false,
        }
    }

    fn is_replacement(&self) -> bool {
        match self {
            Block::Custom(block) => matches!(block.placement, BlockPlacement::Replace(_)),
            Block::FoldedBuffer { .. } => true,
            Block::ExcerptBoundary { .. } => false,
        }
    }

    fn is_header(&self) -> bool {
        match self {
            Block::Custom(_) => false,
            Block::FoldedBuffer { .. } => true,
            Block::ExcerptBoundary { .. } => true,
        }
    }

    pub fn is_buffer_header(&self) -> bool {
        match self {
            Block::Custom(_) => false,
            Block::FoldedBuffer { .. } => true,
            Block::ExcerptBoundary {
                starts_new_buffer, ..
            } => *starts_new_buffer,
        }
    }
}

impl Debug for Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Custom(block) => f.debug_struct("Custom").field("block", block).finish(),
            Self::FoldedBuffer {
                first_excerpt,
                height,
            } => f
                .debug_struct("FoldedBuffer")
                .field("first_excerpt", &first_excerpt)
                .field("height", height)
                .finish(),
            Self::ExcerptBoundary {
                starts_new_buffer,
                excerpt,
                height,
            } => f
                .debug_struct("ExcerptBoundary")
                .field("excerpt", excerpt)
                .field("starts_new_buffer", starts_new_buffer)
                .field("height", height)
                .finish(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct TransformSummary {
    input_rows: u32,
    output_rows: u32,
    longest_row: u32,
    longest_row_chars: u32,
}

pub struct BlockChunks<'a> {
    transforms: sum_tree::Cursor<'a, Transform, (BlockRow, WrapRow)>,
    input_chunks: wrap_map::WrapChunks<'a>,
    input_chunk: Chunk<'a>,
    output_row: u32,
    max_output_row: u32,
    masked: bool,
}

#[derive(Clone)]
pub struct BlockRows<'a> {
    transforms: sum_tree::Cursor<'a, Transform, (BlockRow, WrapRow)>,
    input_rows: wrap_map::WrapRows<'a>,
    output_row: BlockRow,
    started: bool,
}

impl BlockMap {
    pub fn new(
        wrap_snapshot: WrapSnapshot,
        buffer_header_height: u32,
        excerpt_header_height: u32,
    ) -> Self {
        let row_count = wrap_snapshot.max_point().row() + 1;
        let mut transforms = SumTree::default();
        push_isomorphic(&mut transforms, row_count, &wrap_snapshot);
        let map = Self {
            next_block_id: AtomicUsize::new(0),
            custom_blocks: Vec::new(),
            custom_blocks_by_id: TreeMap::default(),
            folded_buffers: HashSet::default(),
            buffers_with_disabled_headers: HashSet::default(),
            transforms: RefCell::new(transforms),
            wrap_snapshot: RefCell::new(wrap_snapshot.clone()),
            buffer_header_height,
            excerpt_header_height,
        };
        map.sync(
            &wrap_snapshot,
            Patch::new(vec![Edit {
                old: 0..row_count,
                new: 0..row_count,
            }]),
        );
        map
    }

    pub fn read(&self, wrap_snapshot: WrapSnapshot, edits: Patch<u32>) -> BlockMapReader {
        self.sync(&wrap_snapshot, edits);
        *self.wrap_snapshot.borrow_mut() = wrap_snapshot.clone();
        BlockMapReader {
            blocks: &self.custom_blocks,
            snapshot: BlockSnapshot {
                wrap_snapshot,
                transforms: self.transforms.borrow().clone(),
                custom_blocks_by_id: self.custom_blocks_by_id.clone(),
                buffer_header_height: self.buffer_header_height,
                excerpt_header_height: self.excerpt_header_height,
            },
        }
    }

    pub fn write(&mut self, wrap_snapshot: WrapSnapshot, edits: Patch<u32>) -> BlockMapWriter {
        self.sync(&wrap_snapshot, edits);
        *self.wrap_snapshot.borrow_mut() = wrap_snapshot;
        BlockMapWriter(self)
    }

    fn sync(&self, wrap_snapshot: &WrapSnapshot, mut edits: Patch<u32>) {
        let buffer = wrap_snapshot.buffer_snapshot();

        // Handle changing the last excerpt if it is empty.
        if buffer.trailing_excerpt_update_count()
            != self
                .wrap_snapshot
                .borrow()
                .buffer_snapshot()
                .trailing_excerpt_update_count()
        {
            let max_point = wrap_snapshot.max_point();
            let edit_start = wrap_snapshot.prev_row_boundary(max_point);
            let edit_end = max_point.row() + 1;
            edits = edits.compose([WrapEdit {
                old: edit_start..edit_end,
                new: edit_start..edit_end,
            }]);
        }

        let edits = edits.into_inner();
        if edits.is_empty() {
            return;
        }

        let mut transforms = self.transforms.borrow_mut();
        let mut new_transforms = SumTree::default();
        let mut cursor = transforms.cursor::<WrapRow>(&());
        let mut last_block_ix = 0;
        let mut blocks_in_edit = Vec::new();
        let mut edits = edits.into_iter().peekable();

        while let Some(edit) = edits.next() {
            let mut old_start = WrapRow(edit.old.start);
            let mut new_start = WrapRow(edit.new.start);

            // Only preserve transforms that:
            // * Strictly precedes this edit
            // * Isomorphic transforms that end *at* the start of the edit
            // * Below blocks that end at the start of the edit
            // However, if we hit a replace block that ends at the start of the edit we want to reconstruct it.
            new_transforms.append(cursor.slice(&old_start, Bias::Left, &()), &());
            if let Some(transform) = cursor.item() {
                if transform.summary.input_rows > 0
                    && cursor.end(&()) == old_start
                    && transform
                        .block
                        .as_ref()
                        .map_or(true, |b| !b.is_replacement())
                {
                    // Preserve the transform (push and next)
                    new_transforms.push(transform.clone(), &());
                    cursor.next(&());

                    // Preserve below blocks at end of edit
                    while let Some(transform) = cursor.item() {
                        if transform.block.as_ref().map_or(false, |b| b.place_below()) {
                            new_transforms.push(transform.clone(), &());
                            cursor.next(&());
                        } else {
                            break;
                        }
                    }
                }
            }

            // Ensure the edit starts at a transform boundary.
            // If the edit starts within an isomorphic transform, preserve its prefix
            // If the edit lands within a replacement block, expand the edit to include the start of the replaced input range
            let transform = cursor.item().unwrap();
            let transform_rows_before_edit = old_start.0 - cursor.start().0;
            if transform_rows_before_edit > 0 {
                if transform.block.is_none() {
                    // Preserve any portion of the old isomorphic transform that precedes this edit.
                    push_isomorphic(
                        &mut new_transforms,
                        transform_rows_before_edit,
                        wrap_snapshot,
                    );
                } else {
                    // We landed within a block that replaces some lines, so we
                    // extend the edit to start at the beginning of the
                    // replacement.
                    debug_assert!(transform.summary.input_rows > 0);
                    old_start.0 -= transform_rows_before_edit;
                    new_start.0 -= transform_rows_before_edit;
                }
            }

            // Decide where the edit ends
            // * It should end at a transform boundary
            // * Coalesce edits that intersect the same transform
            let mut old_end = WrapRow(edit.old.end);
            let mut new_end = WrapRow(edit.new.end);
            loop {
                // Seek to the transform starting at or after the end of the edit
                cursor.seek(&old_end, Bias::Left, &());
                cursor.next(&());

                // Extend edit to the end of the discarded transform so it is reconstructed in full
                let transform_rows_after_edit = cursor.start().0 - old_end.0;
                old_end.0 += transform_rows_after_edit;
                new_end.0 += transform_rows_after_edit;

                // Combine this edit with any subsequent edits that intersect the same transform.
                while let Some(next_edit) = edits.peek() {
                    if next_edit.old.start <= cursor.start().0 {
                        old_end = WrapRow(next_edit.old.end);
                        new_end = WrapRow(next_edit.new.end);
                        cursor.seek(&old_end, Bias::Left, &());
                        cursor.next(&());
                        edits.next();
                    } else {
                        break;
                    }
                }

                if *cursor.start() == old_end {
                    break;
                }
            }

            // Discard below blocks at the end of the edit. They'll be reconstructed.
            while let Some(transform) = cursor.item() {
                if transform.block.as_ref().map_or(false, |b| b.place_below()) {
                    cursor.next(&());
                } else {
                    break;
                }
            }

            // Find the blocks within this edited region.
            let new_buffer_start =
                wrap_snapshot.to_point(WrapPoint::new(new_start.0, 0), Bias::Left);
            let start_bound = Bound::Included(new_buffer_start);
            let start_block_ix =
                match self.custom_blocks[last_block_ix..].binary_search_by(|probe| {
                    probe
                        .start()
                        .to_point(buffer)
                        .cmp(&new_buffer_start)
                        // Move left until we find the index of the first block starting within this edit
                        .then(Ordering::Greater)
                }) {
                    Ok(ix) | Err(ix) => last_block_ix + ix,
                };

            let end_bound;
            let end_block_ix = if new_end.0 > wrap_snapshot.max_point().row() {
                end_bound = Bound::Unbounded;
                self.custom_blocks.len()
            } else {
                let new_buffer_end =
                    wrap_snapshot.to_point(WrapPoint::new(new_end.0, 0), Bias::Left);
                end_bound = Bound::Excluded(new_buffer_end);
                match self.custom_blocks[start_block_ix..].binary_search_by(|probe| {
                    probe
                        .start()
                        .to_point(buffer)
                        .cmp(&new_buffer_end)
                        .then(Ordering::Greater)
                }) {
                    Ok(ix) | Err(ix) => start_block_ix + ix,
                }
            };
            last_block_ix = end_block_ix;

            debug_assert!(blocks_in_edit.is_empty());

            blocks_in_edit.extend(
                self.custom_blocks[start_block_ix..end_block_ix]
                    .iter()
                    .filter_map(|block| {
                        let placement = block.placement.to_wrap_row(wrap_snapshot)?;
                        if let BlockPlacement::Above(row) = placement {
                            if row < new_start {
                                return None;
                            }
                        }
                        Some((placement, Block::Custom(block.clone())))
                    }),
            );

            if buffer.show_headers() {
                blocks_in_edit.extend(self.header_and_footer_blocks(
                    buffer,
                    (start_bound, end_bound),
                    wrap_snapshot,
                ));
            }

            BlockMap::sort_blocks(&mut blocks_in_edit);

            // For each of these blocks, insert a new isomorphic transform preceding the block,
            // and then insert the block itself.
            for (block_placement, block) in blocks_in_edit.drain(..) {
                let mut summary = TransformSummary {
                    input_rows: 0,
                    output_rows: block.height(),
                    longest_row: 0,
                    longest_row_chars: 0,
                };

                let rows_before_block;
                match block_placement {
                    BlockPlacement::Above(position) => {
                        rows_before_block = position.0 - new_transforms.summary().input_rows;
                    }
                    BlockPlacement::Near(position) | BlockPlacement::Below(position) => {
                        if position.0 + 1 < new_transforms.summary().input_rows {
                            continue;
                        }
                        rows_before_block = (position.0 + 1) - new_transforms.summary().input_rows;
                    }
                    BlockPlacement::Replace(range) => {
                        rows_before_block = range.start().0 - new_transforms.summary().input_rows;
                        summary.input_rows = range.end().0 - range.start().0 + 1;
                    }
                }

                push_isomorphic(&mut new_transforms, rows_before_block, wrap_snapshot);
                new_transforms.push(
                    Transform {
                        summary,
                        block: Some(block),
                    },
                    &(),
                );
            }

            // Insert an isomorphic transform after the final block.
            let rows_after_last_block = new_end
                .0
                .saturating_sub(new_transforms.summary().input_rows);
            push_isomorphic(&mut new_transforms, rows_after_last_block, wrap_snapshot);
        }

        new_transforms.append(cursor.suffix(&()), &());
        debug_assert_eq!(
            new_transforms.summary().input_rows,
            wrap_snapshot.max_point().row() + 1
        );

        drop(cursor);
        *transforms = new_transforms;
    }

    pub fn replace_blocks(&mut self, mut renderers: HashMap<CustomBlockId, RenderBlock>) {
        for block in &mut self.custom_blocks {
            if let Some(render) = renderers.remove(&block.id) {
                *block.render.lock() = render;
            }
        }
    }

    fn header_and_footer_blocks<'a, R, T>(
        &'a self,
        buffer: &'a multi_buffer::MultiBufferSnapshot,
        range: R,
        wrap_snapshot: &'a WrapSnapshot,
    ) -> impl Iterator<Item = (BlockPlacement<WrapRow>, Block)> + 'a
    where
        R: RangeBounds<T>,
        T: multi_buffer::ToOffset,
    {
        let mut boundaries = buffer.excerpt_boundaries_in_range(range).peekable();

        std::iter::from_fn(move || {
            loop {
                let excerpt_boundary = boundaries.next()?;
                let wrap_row = wrap_snapshot
                    .make_wrap_point(Point::new(excerpt_boundary.row.0, 0), Bias::Left)
                    .row();

                let new_buffer_id = match (&excerpt_boundary.prev, &excerpt_boundary.next) {
                    (None, next) => Some(next.buffer_id),
                    (Some(prev), next) => {
                        if prev.buffer_id != next.buffer_id {
                            Some(next.buffer_id)
                        } else {
                            None
                        }
                    }
                };

                let mut height = 0;

                if let Some(new_buffer_id) = new_buffer_id {
                    let first_excerpt = excerpt_boundary.next.clone();
                    if self.buffers_with_disabled_headers.contains(&new_buffer_id) {
                        continue;
                    }
                    if self.folded_buffers.contains(&new_buffer_id) {
                        let mut last_excerpt_end_row = first_excerpt.end_row;

                        while let Some(next_boundary) = boundaries.peek() {
                            if next_boundary.next.buffer_id == new_buffer_id {
                                last_excerpt_end_row = next_boundary.next.end_row;
                            } else {
                                break;
                            }

                            boundaries.next();
                        }

                        let wrap_end_row = wrap_snapshot
                            .make_wrap_point(
                                Point::new(
                                    last_excerpt_end_row.0,
                                    buffer.line_len(last_excerpt_end_row),
                                ),
                                Bias::Right,
                            )
                            .row();

                        return Some((
                            BlockPlacement::Replace(WrapRow(wrap_row)..=WrapRow(wrap_end_row)),
                            Block::FoldedBuffer {
                                height: height + self.buffer_header_height,
                                first_excerpt,
                            },
                        ));
                    }
                }

                if new_buffer_id.is_some() {
                    height += self.buffer_header_height;
                } else {
                    height += self.excerpt_header_height;
                }

                return Some((
                    BlockPlacement::Above(WrapRow(wrap_row)),
                    Block::ExcerptBoundary {
                        excerpt: excerpt_boundary.next,
                        height,
                        starts_new_buffer: new_buffer_id.is_some(),
                    },
                ));
            }
        })
    }

    fn sort_blocks(blocks: &mut Vec<(BlockPlacement<WrapRow>, Block)>) {
        blocks.sort_unstable_by(|(placement_a, block_a), (placement_b, block_b)| {
            placement_a
                .start()
                .cmp(placement_b.start())
                .then_with(|| placement_b.end().cmp(placement_a.end()))
                .then_with(|| {
                    if block_a.is_header() {
                        Ordering::Less
                    } else if block_b.is_header() {
                        Ordering::Greater
                    } else {
                        Ordering::Equal
                    }
                })
                .then_with(|| placement_a.sort_order().cmp(&placement_b.sort_order()))
                .then_with(|| match (block_a, block_b) {
                    (
                        Block::ExcerptBoundary {
                            excerpt: excerpt_a, ..
                        },
                        Block::ExcerptBoundary {
                            excerpt: excerpt_b, ..
                        },
                    ) => Some(excerpt_a.id).cmp(&Some(excerpt_b.id)),
                    (Block::ExcerptBoundary { .. }, Block::Custom(_)) => Ordering::Less,
                    (Block::Custom(_), Block::ExcerptBoundary { .. }) => Ordering::Greater,
                    (Block::Custom(block_a), Block::Custom(block_b)) => block_a
                        .priority
                        .cmp(&block_b.priority)
                        .then_with(|| block_a.id.cmp(&block_b.id)),
                    _ => {
                        unreachable!()
                    }
                })
        });
        blocks.dedup_by(|right, left| match (left.0.clone(), right.0.clone()) {
            (BlockPlacement::Replace(range), BlockPlacement::Above(row))
            | (BlockPlacement::Replace(range), BlockPlacement::Below(row)) => range.contains(&row),
            (BlockPlacement::Replace(range_a), BlockPlacement::Replace(range_b)) => {
                if range_a.end() >= range_b.start() && range_a.start() <= range_b.end() {
                    left.0 = BlockPlacement::Replace(
                        *range_a.start()..=*range_a.end().max(range_b.end()),
                    );
                    true
                } else {
                    false
                }
            }
            _ => false,
        });
    }
}

fn push_isomorphic(tree: &mut SumTree<Transform>, rows: u32, wrap_snapshot: &WrapSnapshot) {
    if rows == 0 {
        return;
    }

    let wrap_row_start = tree.summary().input_rows;
    let wrap_row_end = wrap_row_start + rows;
    let wrap_summary = wrap_snapshot.text_summary_for_range(wrap_row_start..wrap_row_end);
    let summary = TransformSummary {
        input_rows: rows,
        output_rows: rows,
        longest_row: wrap_summary.longest_row,
        longest_row_chars: wrap_summary.longest_row_chars,
    };
    let mut merged = false;
    tree.update_last(
        |last_transform| {
            if last_transform.block.is_none() {
                last_transform.summary.add_summary(&summary, &());
                merged = true;
            }
        },
        &(),
    );
    if !merged {
        tree.push(
            Transform {
                summary,
                block: None,
            },
            &(),
        );
    }
}

impl BlockPoint {
    pub fn new(row: u32, column: u32) -> Self {
        Self(Point::new(row, column))
    }
}

impl Deref for BlockPoint {
    type Target = Point;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for BlockPoint {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Deref for BlockMapReader<'_> {
    type Target = BlockSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

impl DerefMut for BlockMapReader<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.snapshot
    }
}

impl BlockMapReader<'_> {
    pub fn row_for_block(&self, block_id: CustomBlockId) -> Option<BlockRow> {
        let block = self.blocks.iter().find(|block| block.id == block_id)?;
        let buffer_row = block
            .start()
            .to_point(self.wrap_snapshot.buffer_snapshot())
            .row;
        let wrap_row = self
            .wrap_snapshot
            .make_wrap_point(Point::new(buffer_row, 0), Bias::Left)
            .row();
        let start_wrap_row = WrapRow(
            self.wrap_snapshot
                .prev_row_boundary(WrapPoint::new(wrap_row, 0)),
        );
        let end_wrap_row = WrapRow(
            self.wrap_snapshot
                .next_row_boundary(WrapPoint::new(wrap_row, 0))
                .unwrap_or(self.wrap_snapshot.max_point().row() + 1),
        );

        let mut cursor = self.transforms.cursor::<(WrapRow, BlockRow)>(&());
        cursor.seek(&start_wrap_row, Bias::Left, &());
        while let Some(transform) = cursor.item() {
            if cursor.start().0 > end_wrap_row {
                break;
            }

            if let Some(BlockId::Custom(id)) = transform.block.as_ref().map(|block| block.id()) {
                if id == block_id {
                    return Some(cursor.start().1);
                }
            }
            cursor.next(&());
        }

        None
    }
}

impl BlockMapWriter<'_> {
    pub fn insert(
        &mut self,
        blocks: impl IntoIterator<Item = BlockProperties<Anchor>>,
    ) -> Vec<CustomBlockId> {
        let blocks = blocks.into_iter();
        let mut ids = Vec::with_capacity(blocks.size_hint().1.unwrap_or(0));
        let mut edits = Patch::default();
        let wrap_snapshot = &*self.0.wrap_snapshot.borrow();
        let buffer = wrap_snapshot.buffer_snapshot();

        let mut previous_wrap_row_range: Option<Range<u32>> = None;
        for block in blocks {
            if let BlockPlacement::Replace(_) = &block.placement {
                debug_assert!(block.height.unwrap() > 0);
            }

            let id = CustomBlockId(self.0.next_block_id.fetch_add(1, SeqCst));
            ids.push(id);

            let start = block.placement.start().to_point(buffer);
            let end = block.placement.end().to_point(buffer);
            let start_wrap_row = wrap_snapshot.make_wrap_point(start, Bias::Left).row();
            let end_wrap_row = wrap_snapshot.make_wrap_point(end, Bias::Left).row();

            let (start_row, end_row) = {
                previous_wrap_row_range.take_if(|range| {
                    !range.contains(&start_wrap_row) || !range.contains(&end_wrap_row)
                });
                let range = previous_wrap_row_range.get_or_insert_with(|| {
                    let start_row =
                        wrap_snapshot.prev_row_boundary(WrapPoint::new(start_wrap_row, 0));
                    let end_row = wrap_snapshot
                        .next_row_boundary(WrapPoint::new(end_wrap_row, 0))
                        .unwrap_or(wrap_snapshot.max_point().row() + 1);
                    start_row..end_row
                });
                (range.start, range.end)
            };
            let block_ix = match self
                .0
                .custom_blocks
                .binary_search_by(|probe| probe.placement.cmp(&block.placement, buffer))
            {
                Ok(ix) | Err(ix) => ix,
            };
            let new_block = Arc::new(CustomBlock {
                id,
                placement: block.placement,
                height: block.height,
                render: Arc::new(Mutex::new(block.render)),
                style: block.style,
                priority: block.priority,
                render_in_minimap: block.render_in_minimap,
            });
            self.0.custom_blocks.insert(block_ix, new_block.clone());
            self.0.custom_blocks_by_id.insert(id, new_block);

            edits = edits.compose([Edit {
                old: start_row..end_row,
                new: start_row..end_row,
            }]);
        }

        self.0.sync(wrap_snapshot, edits);
        ids
    }

    pub fn resize(&mut self, mut heights: HashMap<CustomBlockId, u32>) {
        let wrap_snapshot = &*self.0.wrap_snapshot.borrow();
        let buffer = wrap_snapshot.buffer_snapshot();
        let mut edits = Patch::default();
        let mut last_block_buffer_row = None;

        for block in &mut self.0.custom_blocks {
            if let Some(new_height) = heights.remove(&block.id) {
                if let BlockPlacement::Replace(_) = &block.placement {
                    debug_assert!(new_height > 0);
                }

                if block.height != Some(new_height) {
                    let new_block = CustomBlock {
                        id: block.id,
                        placement: block.placement.clone(),
                        height: Some(new_height),
                        style: block.style,
                        render: block.render.clone(),
                        priority: block.priority,
                        render_in_minimap: block.render_in_minimap,
                    };
                    let new_block = Arc::new(new_block);
                    *block = new_block.clone();
                    self.0.custom_blocks_by_id.insert(block.id, new_block);

                    let start_row = block.placement.start().to_point(buffer).row;
                    let end_row = block.placement.end().to_point(buffer).row;
                    if last_block_buffer_row != Some(end_row) {
                        last_block_buffer_row = Some(end_row);
                        let start_wrap_row = wrap_snapshot
                            .make_wrap_point(Point::new(start_row, 0), Bias::Left)
                            .row();
                        let end_wrap_row = wrap_snapshot
                            .make_wrap_point(Point::new(end_row, 0), Bias::Left)
                            .row();
                        let start =
                            wrap_snapshot.prev_row_boundary(WrapPoint::new(start_wrap_row, 0));
                        let end = wrap_snapshot
                            .next_row_boundary(WrapPoint::new(end_wrap_row, 0))
                            .unwrap_or(wrap_snapshot.max_point().row() + 1);
                        edits.push(Edit {
                            old: start..end,
                            new: start..end,
                        })
                    }
                }
            }
        }

        self.0.sync(wrap_snapshot, edits);
    }

    pub fn remove(&mut self, block_ids: HashSet<CustomBlockId>) {
        let wrap_snapshot = &*self.0.wrap_snapshot.borrow();
        let buffer = wrap_snapshot.buffer_snapshot();
        let mut edits = Patch::default();
        let mut last_block_buffer_row = None;
        let mut previous_wrap_row_range: Option<Range<u32>> = None;
        self.0.custom_blocks.retain(|block| {
            if block_ids.contains(&block.id) {
                let start = block.placement.start().to_point(buffer);
                let end = block.placement.end().to_point(buffer);
                if last_block_buffer_row != Some(end.row) {
                    last_block_buffer_row = Some(end.row);
                    let start_wrap_row = wrap_snapshot.make_wrap_point(start, Bias::Left).row();
                    let end_wrap_row = wrap_snapshot.make_wrap_point(end, Bias::Left).row();
                    let (start_row, end_row) = {
                        previous_wrap_row_range.take_if(|range| {
                            !range.contains(&start_wrap_row) || !range.contains(&end_wrap_row)
                        });
                        let range = previous_wrap_row_range.get_or_insert_with(|| {
                            let start_row =
                                wrap_snapshot.prev_row_boundary(WrapPoint::new(start_wrap_row, 0));
                            let end_row = wrap_snapshot
                                .next_row_boundary(WrapPoint::new(end_wrap_row, 0))
                                .unwrap_or(wrap_snapshot.max_point().row() + 1);
                            start_row..end_row
                        });
                        (range.start, range.end)
                    };

                    edits.push(Edit {
                        old: start_row..end_row,
                        new: start_row..end_row,
                    })
                }
                false
            } else {
                true
            }
        });
        self.0
            .custom_blocks_by_id
            .retain(|id, _| !block_ids.contains(id));
        self.0.sync(wrap_snapshot, edits);
    }

    pub fn remove_intersecting_replace_blocks<T>(
        &mut self,
        ranges: impl IntoIterator<Item = Range<T>>,
        inclusive: bool,
    ) where
        T: ToOffset,
    {
        let wrap_snapshot = self.0.wrap_snapshot.borrow();
        let mut blocks_to_remove = HashSet::default();
        for range in ranges {
            let range = range.start.to_offset(wrap_snapshot.buffer_snapshot())
                ..range.end.to_offset(wrap_snapshot.buffer_snapshot());
            for block in self.blocks_intersecting_buffer_range(range, inclusive) {
                if matches!(block.placement, BlockPlacement::Replace(_)) {
                    blocks_to_remove.insert(block.id);
                }
            }
        }
        drop(wrap_snapshot);
        self.remove(blocks_to_remove);
    }

    pub fn disable_header_for_buffer(&mut self, buffer_id: BufferId) {
        self.0.buffers_with_disabled_headers.insert(buffer_id);
    }

    pub fn fold_buffers(
        &mut self,
        buffer_ids: impl IntoIterator<Item = BufferId>,
        multi_buffer: &MultiBuffer,
        cx: &App,
    ) {
        self.fold_or_unfold_buffers(true, buffer_ids, multi_buffer, cx);
    }

    pub fn unfold_buffers(
        &mut self,
        buffer_ids: impl IntoIterator<Item = BufferId>,
        multi_buffer: &MultiBuffer,
        cx: &App,
    ) {
        self.fold_or_unfold_buffers(false, buffer_ids, multi_buffer, cx);
    }

    fn fold_or_unfold_buffers(
        &mut self,
        fold: bool,
        buffer_ids: impl IntoIterator<Item = BufferId>,
        multi_buffer: &MultiBuffer,
        cx: &App,
    ) {
        let mut ranges = Vec::new();
        for buffer_id in buffer_ids {
            if fold {
                self.0.folded_buffers.insert(buffer_id);
            } else {
                self.0.folded_buffers.remove(&buffer_id);
            }
            ranges.extend(multi_buffer.excerpt_ranges_for_buffer(buffer_id, cx));
        }
        ranges.sort_unstable_by_key(|range| range.start);

        let mut edits = Patch::default();
        let wrap_snapshot = self.0.wrap_snapshot.borrow().clone();
        for range in ranges {
            let last_edit_row = cmp::min(
                wrap_snapshot.make_wrap_point(range.end, Bias::Right).row() + 1,
                wrap_snapshot.max_point().row(),
            ) + 1;
            let range = wrap_snapshot.make_wrap_point(range.start, Bias::Left).row()..last_edit_row;
            edits.push(Edit {
                old: range.clone(),
                new: range,
            });
        }

        self.0.sync(&wrap_snapshot, edits);
    }

    fn blocks_intersecting_buffer_range(
        &self,
        range: Range<usize>,
        inclusive: bool,
    ) -> &[Arc<CustomBlock>] {
        let wrap_snapshot = self.0.wrap_snapshot.borrow();
        let buffer = wrap_snapshot.buffer_snapshot();

        let start_block_ix = match self.0.custom_blocks.binary_search_by(|block| {
            let block_end = block.end().to_offset(buffer);
            block_end.cmp(&range.start).then_with(|| {
                if inclusive || (range.is_empty() && block.start().to_offset(buffer) == block_end) {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            })
        }) {
            Ok(ix) | Err(ix) => ix,
        };
        let end_block_ix = match self.0.custom_blocks.binary_search_by(|block| {
            block
                .start()
                .to_offset(buffer)
                .cmp(&range.end)
                .then(if inclusive {
                    Ordering::Less
                } else {
                    Ordering::Greater
                })
        }) {
            Ok(ix) | Err(ix) => ix,
        };

        &self.0.custom_blocks[start_block_ix..end_block_ix]
    }
}

impl BlockSnapshot {
    pub(crate) fn chunks<'a>(
        &'a self,
        rows: Range<u32>,
        language_aware: bool,
        masked: bool,
        highlights: Highlights<'a>,
    ) -> BlockChunks<'a> {
        let max_output_row = cmp::min(rows.end, self.transforms.summary().output_rows);

        let mut cursor = self.transforms.cursor::<(BlockRow, WrapRow)>(&());
        cursor.seek(&BlockRow(rows.start), Bias::Right, &());
        let transform_output_start = cursor.start().0.0;
        let transform_input_start = cursor.start().1.0;

        let mut input_start = transform_input_start;
        let mut input_end = transform_input_start;
        if let Some(transform) = cursor.item() {
            if transform.block.is_none() {
                input_start += rows.start - transform_output_start;
                input_end += cmp::min(
                    rows.end - transform_output_start,
                    transform.summary.input_rows,
                );
            }
        }

        BlockChunks {
            input_chunks: self.wrap_snapshot.chunks(
                input_start..input_end,
                language_aware,
                highlights,
            ),
            input_chunk: Default::default(),
            transforms: cursor,
            output_row: rows.start,
            max_output_row,
            masked,
        }
    }

    pub(super) fn row_infos(&self, start_row: BlockRow) -> BlockRows {
        let mut cursor = self.transforms.cursor::<(BlockRow, WrapRow)>(&());
        cursor.seek(&start_row, Bias::Right, &());
        let (output_start, input_start) = cursor.start();
        let overshoot = if cursor
            .item()
            .map_or(false, |transform| transform.block.is_none())
        {
            start_row.0 - output_start.0
        } else {
            0
        };
        let input_start_row = input_start.0 + overshoot;
        BlockRows {
            transforms: cursor,
            input_rows: self.wrap_snapshot.row_infos(input_start_row),
            output_row: start_row,
            started: false,
        }
    }

    pub fn blocks_in_range(&self, rows: Range<u32>) -> impl Iterator<Item = (u32, &Block)> {
        let mut cursor = self.transforms.cursor::<BlockRow>(&());
        cursor.seek(&BlockRow(rows.start), Bias::Left, &());
        while cursor.start().0 < rows.start && cursor.end(&()).0 <= rows.start {
            cursor.next(&());
        }

        std::iter::from_fn(move || {
            while let Some(transform) = cursor.item() {
                let start_row = cursor.start().0;
                if start_row > rows.end
                    || (start_row == rows.end
                        && transform
                            .block
                            .as_ref()
                            .map_or(false, |block| block.height() > 0))
                {
                    break;
                }
                if let Some(block) = &transform.block {
                    cursor.next(&());
                    return Some((start_row, block));
                } else {
                    cursor.next(&());
                }
            }
            None
        })
    }

    pub fn sticky_header_excerpt(&self, position: f32) -> Option<StickyHeaderExcerpt<'_>> {
        let top_row = position as u32;
        let mut cursor = self.transforms.cursor::<BlockRow>(&());
        cursor.seek(&BlockRow(top_row), Bias::Right, &());

        while let Some(transform) = cursor.item() {
            match &transform.block {
                Some(Block::ExcerptBoundary { excerpt, .. }) => {
                    return Some(StickyHeaderExcerpt { excerpt });
                }
                Some(block) if block.is_buffer_header() => return None,
                _ => {
                    cursor.prev(&());
                    continue;
                }
            }
        }

        None
    }

    pub fn block_for_id(&self, block_id: BlockId) -> Option<Block> {
        let buffer = self.wrap_snapshot.buffer_snapshot();
        let wrap_point = match block_id {
            BlockId::Custom(custom_block_id) => {
                let custom_block = self.custom_blocks_by_id.get(&custom_block_id)?;
                return Some(Block::Custom(custom_block.clone()));
            }
            BlockId::ExcerptBoundary(next_excerpt_id) => {
                let excerpt_range = buffer.range_for_excerpt(next_excerpt_id)?;
                self.wrap_snapshot
                    .make_wrap_point(excerpt_range.start, Bias::Left)
            }
            BlockId::FoldedBuffer(excerpt_id) => self
                .wrap_snapshot
                .make_wrap_point(buffer.range_for_excerpt(excerpt_id)?.start, Bias::Left),
        };
        let wrap_row = WrapRow(wrap_point.row());

        let mut cursor = self.transforms.cursor::<WrapRow>(&());
        cursor.seek(&wrap_row, Bias::Left, &());

        while let Some(transform) = cursor.item() {
            if let Some(block) = transform.block.as_ref() {
                if block.id() == block_id {
                    return Some(block.clone());
                }
            } else if *cursor.start() > wrap_row {
                break;
            }

            cursor.next(&());
        }

        None
    }

    pub fn max_point(&self) -> BlockPoint {
        let row = self.transforms.summary().output_rows.saturating_sub(1);
        BlockPoint::new(row, self.line_len(BlockRow(row)))
    }

    pub fn longest_row(&self) -> u32 {
        self.transforms.summary().longest_row
    }

    pub fn longest_row_in_range(&self, range: Range<BlockRow>) -> BlockRow {
        let mut cursor = self.transforms.cursor::<(BlockRow, WrapRow)>(&());
        cursor.seek(&range.start, Bias::Right, &());

        let mut longest_row = range.start;
        let mut longest_row_chars = 0;
        if let Some(transform) = cursor.item() {
            if transform.block.is_none() {
                let (output_start, input_start) = cursor.start();
                let overshoot = range.start.0 - output_start.0;
                let wrap_start_row = input_start.0 + overshoot;
                let wrap_end_row = cmp::min(
                    input_start.0 + (range.end.0 - output_start.0),
                    cursor.end(&()).1.0,
                );
                let summary = self
                    .wrap_snapshot
                    .text_summary_for_range(wrap_start_row..wrap_end_row);
                longest_row = BlockRow(range.start.0 + summary.longest_row);
                longest_row_chars = summary.longest_row_chars;
            }
            cursor.next(&());
        }

        let cursor_start_row = cursor.start().0;
        if range.end > cursor_start_row {
            let summary = cursor.summary::<_, TransformSummary>(&range.end, Bias::Right, &());
            if summary.longest_row_chars > longest_row_chars {
                longest_row = BlockRow(cursor_start_row.0 + summary.longest_row);
                longest_row_chars = summary.longest_row_chars;
            }

            if let Some(transform) = cursor.item() {
                if transform.block.is_none() {
                    let (output_start, input_start) = cursor.start();
                    let overshoot = range.end.0 - output_start.0;
                    let wrap_start_row = input_start.0;
                    let wrap_end_row = input_start.0 + overshoot;
                    let summary = self
                        .wrap_snapshot
                        .text_summary_for_range(wrap_start_row..wrap_end_row);
                    if summary.longest_row_chars > longest_row_chars {
                        longest_row = BlockRow(output_start.0 + summary.longest_row);
                    }
                }
            }
        }

        longest_row
    }

    pub(super) fn line_len(&self, row: BlockRow) -> u32 {
        let mut cursor = self.transforms.cursor::<(BlockRow, WrapRow)>(&());
        cursor.seek(&BlockRow(row.0), Bias::Right, &());
        if let Some(transform) = cursor.item() {
            let (output_start, input_start) = cursor.start();
            let overshoot = row.0 - output_start.0;
            if transform.block.is_some() {
                0
            } else {
                self.wrap_snapshot.line_len(input_start.0 + overshoot)
            }
        } else if row.0 == 0 {
            0
        } else {
            panic!("row out of range");
        }
    }

    pub(super) fn is_block_line(&self, row: BlockRow) -> bool {
        let mut cursor = self.transforms.cursor::<(BlockRow, WrapRow)>(&());
        cursor.seek(&row, Bias::Right, &());
        cursor.item().map_or(false, |t| t.block.is_some())
    }

    pub(super) fn is_folded_buffer_header(&self, row: BlockRow) -> bool {
        let mut cursor = self.transforms.cursor::<(BlockRow, WrapRow)>(&());
        cursor.seek(&row, Bias::Right, &());
        let Some(transform) = cursor.item() else {
            return false;
        };
        matches!(transform.block, Some(Block::FoldedBuffer { .. }))
    }

    pub(super) fn is_line_replaced(&self, row: MultiBufferRow) -> bool {
        let wrap_point = self
            .wrap_snapshot
            .make_wrap_point(Point::new(row.0, 0), Bias::Left);
        let mut cursor = self.transforms.cursor::<(WrapRow, BlockRow)>(&());
        cursor.seek(&WrapRow(wrap_point.row()), Bias::Right, &());
        cursor.item().map_or(false, |transform| {
            transform
                .block
                .as_ref()
                .map_or(false, |block| block.is_replacement())
        })
    }

    pub fn clip_point(&self, point: BlockPoint, bias: Bias) -> BlockPoint {
        let mut cursor = self.transforms.cursor::<(BlockRow, WrapRow)>(&());
        cursor.seek(&BlockRow(point.row), Bias::Right, &());

        let max_input_row = WrapRow(self.transforms.summary().input_rows);
        let mut search_left =
            (bias == Bias::Left && cursor.start().1.0 > 0) || cursor.end(&()).1 == max_input_row;
        let mut reversed = false;

        loop {
            if let Some(transform) = cursor.item() {
                let (output_start_row, input_start_row) = cursor.start();
                let (output_end_row, input_end_row) = cursor.end(&());
                let output_start = Point::new(output_start_row.0, 0);
                let input_start = Point::new(input_start_row.0, 0);
                let input_end = Point::new(input_end_row.0, 0);

                match transform.block.as_ref() {
                    Some(block) => {
                        if block.is_replacement() {
                            if ((bias == Bias::Left || search_left) && output_start <= point.0)
                                || (!search_left && output_start >= point.0)
                            {
                                return BlockPoint(output_start);
                            }
                        }
                    }
                    None => {
                        let input_point = if point.row >= output_end_row.0 {
                            let line_len = self.wrap_snapshot.line_len(input_end_row.0 - 1);
                            self.wrap_snapshot
                                .clip_point(WrapPoint::new(input_end_row.0 - 1, line_len), bias)
                        } else {
                            let output_overshoot = point.0.saturating_sub(output_start);
                            self.wrap_snapshot
                                .clip_point(WrapPoint(input_start + output_overshoot), bias)
                        };

                        if (input_start..input_end).contains(&input_point.0) {
                            let input_overshoot = input_point.0.saturating_sub(input_start);
                            return BlockPoint(output_start + input_overshoot);
                        }
                    }
                }

                if search_left {
                    cursor.prev(&());
                } else {
                    cursor.next(&());
                }
            } else if reversed {
                return self.max_point();
            } else {
                reversed = true;
                search_left = !search_left;
                cursor.seek(&BlockRow(point.row), Bias::Right, &());
            }
        }
    }

    pub fn to_block_point(&self, wrap_point: WrapPoint) -> BlockPoint {
        let mut cursor = self.transforms.cursor::<(WrapRow, BlockRow)>(&());
        cursor.seek(&WrapRow(wrap_point.row()), Bias::Right, &());
        if let Some(transform) = cursor.item() {
            if transform.block.is_some() {
                BlockPoint::new(cursor.start().1.0, 0)
            } else {
                let (input_start_row, output_start_row) = cursor.start();
                let input_start = Point::new(input_start_row.0, 0);
                let output_start = Point::new(output_start_row.0, 0);
                let input_overshoot = wrap_point.0 - input_start;
                BlockPoint(output_start + input_overshoot)
            }
        } else {
            self.max_point()
        }
    }

    pub fn to_wrap_point(&self, block_point: BlockPoint, bias: Bias) -> WrapPoint {
        let mut cursor = self.transforms.cursor::<(BlockRow, WrapRow)>(&());
        cursor.seek(&BlockRow(block_point.row), Bias::Right, &());
        if let Some(transform) = cursor.item() {
            match transform.block.as_ref() {
                Some(block) => {
                    if block.place_below() {
                        let wrap_row = cursor.start().1.0 - 1;
                        WrapPoint::new(wrap_row, self.wrap_snapshot.line_len(wrap_row))
                    } else if block.place_above() {
                        WrapPoint::new(cursor.start().1.0, 0)
                    } else if bias == Bias::Left {
                        WrapPoint::new(cursor.start().1.0, 0)
                    } else {
                        let wrap_row = cursor.end(&()).1.0 - 1;
                        WrapPoint::new(wrap_row, self.wrap_snapshot.line_len(wrap_row))
                    }
                }
                None => {
                    let overshoot = block_point.row - cursor.start().0.0;
                    let wrap_row = cursor.start().1.0 + overshoot;
                    WrapPoint::new(wrap_row, block_point.column)
                }
            }
        } else {
            self.wrap_snapshot.max_point()
        }
    }
}

impl BlockChunks<'_> {
    /// Go to the next transform
    fn advance(&mut self) {
        self.input_chunk = Chunk::default();
        self.transforms.next(&());
        while let Some(transform) = self.transforms.item() {
            if transform
                .block
                .as_ref()
                .map_or(false, |block| block.height() == 0)
            {
                self.transforms.next(&());
            } else {
                break;
            }
        }

        if self
            .transforms
            .item()
            .map_or(false, |transform| transform.block.is_none())
        {
            let start_input_row = self.transforms.start().1.0;
            let start_output_row = self.transforms.start().0.0;
            if start_output_row < self.max_output_row {
                let end_input_row = cmp::min(
                    self.transforms.end(&()).1.0,
                    start_input_row + (self.max_output_row - start_output_row),
                );
                self.input_chunks.seek(start_input_row..end_input_row);
            }
        }
    }
}

pub struct StickyHeaderExcerpt<'a> {
    pub excerpt: &'a ExcerptInfo,
}

impl<'a> Iterator for BlockChunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.output_row >= self.max_output_row {
            return None;
        }

        let transform = self.transforms.item()?;
        if transform.block.is_some() {
            let block_start = self.transforms.start().0.0;
            let mut block_end = self.transforms.end(&()).0.0;
            self.advance();
            if self.transforms.item().is_none() {
                block_end -= 1;
            }

            let start_in_block = self.output_row - block_start;
            let end_in_block = cmp::min(self.max_output_row, block_end) - block_start;
            let line_count = end_in_block - start_in_block;
            self.output_row += line_count;

            return Some(Chunk {
                text: unsafe { std::str::from_utf8_unchecked(&NEWLINES[..line_count as usize]) },
                ..Default::default()
            });
        }

        if self.input_chunk.text.is_empty() {
            if let Some(input_chunk) = self.input_chunks.next() {
                self.input_chunk = input_chunk;
            } else {
                if self.output_row < self.max_output_row {
                    self.output_row += 1;
                    self.advance();
                    if self.transforms.item().is_some() {
                        return Some(Chunk {
                            text: "\n",
                            ..Default::default()
                        });
                    }
                }
                return None;
            }
        }

        let transform_end = self.transforms.end(&()).0.0;
        let (prefix_rows, prefix_bytes) =
            offset_for_row(self.input_chunk.text, transform_end - self.output_row);
        self.output_row += prefix_rows;

        let (mut prefix, suffix) = self.input_chunk.text.split_at(prefix_bytes);
        self.input_chunk.text = suffix;

        if self.masked {
            // Not great for multibyte text because to keep cursor math correct we
            // need to have the same number of bytes in the input as output.
            let chars = prefix.chars().count();
            let bullet_len = chars;
            prefix = &BULLETS[..bullet_len];
        }

        let chunk = Chunk {
            text: prefix,
            ..self.input_chunk.clone()
        };

        if self.output_row == transform_end {
            self.advance();
        }

        Some(chunk)
    }
}

impl Iterator for BlockRows<'_> {
    type Item = RowInfo;

    fn next(&mut self) -> Option<Self::Item> {
        if self.started {
            self.output_row.0 += 1;
        } else {
            self.started = true;
        }

        if self.output_row.0 >= self.transforms.end(&()).0.0 {
            self.transforms.next(&());
            while let Some(transform) = self.transforms.item() {
                if transform
                    .block
                    .as_ref()
                    .map_or(false, |block| block.height() == 0)
                {
                    self.transforms.next(&());
                } else {
                    break;
                }
            }

            let transform = self.transforms.item()?;
            if transform
                .block
                .as_ref()
                .map_or(true, |block| block.is_replacement())
            {
                self.input_rows.seek(self.transforms.start().1.0);
            }
        }

        let transform = self.transforms.item()?;
        if let Some(block) = transform.block.as_ref() {
            if block.is_replacement() && self.transforms.start().0 == self.output_row {
                if matches!(block, Block::FoldedBuffer { .. }) {
                    Some(RowInfo::default())
                } else {
                    Some(self.input_rows.next().unwrap())
                }
            } else {
                Some(RowInfo::default())
            }
        } else {
            Some(self.input_rows.next().unwrap())
        }
    }
}

impl sum_tree::Item for Transform {
    type Summary = TransformSummary;

    fn summary(&self, _cx: &()) -> Self::Summary {
        self.summary.clone()
    }
}

impl sum_tree::Summary for TransformSummary {
    type Context = ();

    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &Self, _: &()) {
        if summary.longest_row_chars > self.longest_row_chars {
            self.longest_row = self.output_rows + summary.longest_row;
            self.longest_row_chars = summary.longest_row_chars;
        }
        self.input_rows += summary.input_rows;
        self.output_rows += summary.output_rows;
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for WrapRow {
    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: &()) {
        self.0 += summary.input_rows;
    }
}

impl<'a> sum_tree::Dimension<'a, TransformSummary> for BlockRow {
    fn zero(_cx: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a TransformSummary, _: &()) {
        self.0 += summary.output_rows;
    }
}

impl Deref for BlockContext<'_, '_> {
    type Target = App;

    fn deref(&self) -> &Self::Target {
        self.app
    }
}

impl DerefMut for BlockContext<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.app
    }
}

impl CustomBlock {
    pub fn render(&self, cx: &mut BlockContext) -> AnyElement {
        self.render.lock()(cx)
    }

    pub fn start(&self) -> Anchor {
        *self.placement.start()
    }

    pub fn end(&self) -> Anchor {
        *self.placement.end()
    }

    pub fn style(&self) -> BlockStyle {
        self.style
    }
}

impl Debug for CustomBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Block")
            .field("id", &self.id)
            .field("placement", &self.placement)
            .field("height", &self.height)
            .field("style", &self.style)
            .field("priority", &self.priority)
            .finish_non_exhaustive()
    }
}

// Count the number of bytes prior to a target point. If the string doesn't contain the target
// point, return its total extent. Otherwise return the target point itself.
fn offset_for_row(s: &str, target: u32) -> (u32, usize) {
    let mut row = 0;
    let mut offset = 0;
    for (ix, line) in s.split('\n').enumerate() {
        if ix > 0 {
            row += 1;
            offset += 1;
        }
        if row >= target {
            break;
        }
        offset += line.len();
    }
    (row, offset)
}
