use crate::{OffsetUtf16, Point, PointUtf16, TextSummary, Unclipped};
use arrayvec::ArrayString;
use std::{cmp, ops::Range};
use sum_tree::Bias;
use unicode_segmentation::GraphemeCursor;

pub(crate) const MIN_BASE: usize = 64;
pub(crate) const MAX_BASE: usize = MIN_BASE * 2;

#[derive(Clone, Debug, Default)]
pub struct Chunk {
    chars: u128,
    chars_utf16: u128,
    newlines: u128,
    tabs: u128,
    pub text: ArrayString<MAX_BASE>,
}

impl Chunk {
    #[inline(always)]
    pub fn new(text: &str) -> Self {
        let mut this = Chunk::default();
        this.push_str(text);
        this
    }

    #[inline(always)]
    pub fn push_str(&mut self, text: &str) {
        for (char_ix, c) in text.char_indices() {
            let ix = self.text.len() + char_ix;
            self.chars |= 1 << ix;
            self.chars_utf16 |= 1 << ix;
            self.chars_utf16 |= (c.len_utf16() as u128) << ix;
            self.newlines |= ((c == '\n') as u128) << ix;
            self.tabs |= ((c == '\t') as u128) << ix;
        }
        self.text.push_str(text);
    }

    #[inline(always)]
    pub fn append(&mut self, slice: ChunkSlice) {
        if slice.is_empty() {
            return;
        };

        let base_ix = self.text.len();
        self.chars |= slice.chars << base_ix;
        self.chars_utf16 |= slice.chars_utf16 << base_ix;
        self.newlines |= slice.newlines << base_ix;
        self.tabs |= slice.tabs << base_ix;
        self.text.push_str(&slice.text);
    }

    #[inline(always)]
    pub fn as_slice(&self) -> ChunkSlice {
        ChunkSlice {
            chars: self.chars,
            chars_utf16: self.chars_utf16,
            newlines: self.newlines,
            tabs: self.tabs,
            text: &self.text,
        }
    }

    #[inline(always)]
    pub fn slice(&self, range: Range<usize>) -> ChunkSlice {
        self.as_slice().slice(range)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ChunkSlice<'a> {
    chars: u128,
    chars_utf16: u128,
    newlines: u128,
    tabs: u128,
    text: &'a str,
}

impl Into<Chunk> for ChunkSlice<'_> {
    fn into(self) -> Chunk {
        Chunk {
            chars: self.chars,
            chars_utf16: self.chars_utf16,
            newlines: self.newlines,
            tabs: self.tabs,
            text: self.text.try_into().unwrap(),
        }
    }
}

impl<'a> ChunkSlice<'a> {
    #[inline(always)]
    pub fn is_empty(self) -> bool {
        self.text.is_empty()
    }

    #[inline(always)]
    pub fn is_char_boundary(self, offset: usize) -> bool {
        self.text.is_char_boundary(offset)
    }

    #[inline(always)]
    pub fn split_at(self, mid: usize) -> (ChunkSlice<'a>, ChunkSlice<'a>) {
        if mid == MAX_BASE {
            let left = self;
            let right = ChunkSlice {
                chars: 0,
                chars_utf16: 0,
                newlines: 0,
                tabs: 0,
                text: "",
            };
            (left, right)
        } else {
            let mask = (1u128 << mid) - 1;
            let (left_text, right_text) = self.text.split_at(mid);
            let left = ChunkSlice {
                chars: self.chars & mask,
                chars_utf16: self.chars_utf16 & mask,
                newlines: self.newlines & mask,
                tabs: self.tabs & mask,
                text: left_text,
            };
            let right = ChunkSlice {
                chars: self.chars >> mid,
                chars_utf16: self.chars_utf16 >> mid,
                newlines: self.newlines >> mid,
                tabs: self.tabs >> mid,
                text: right_text,
            };
            (left, right)
        }
    }

    #[inline(always)]
    pub fn slice(self, range: Range<usize>) -> Self {
        let mask = if range.end == MAX_BASE {
            u128::MAX
        } else {
            (1u128 << range.end) - 1
        };
        if range.start == MAX_BASE {
            Self {
                chars: 0,
                chars_utf16: 0,
                newlines: 0,
                tabs: 0,
                text: "",
            }
        } else {
            Self {
                chars: (self.chars & mask) >> range.start,
                chars_utf16: (self.chars_utf16 & mask) >> range.start,
                newlines: (self.newlines & mask) >> range.start,
                tabs: (self.tabs & mask) >> range.start,
                text: &self.text[range],
            }
        }
    }

    #[inline(always)]
    pub fn text_summary(&self) -> TextSummary {
        let mut chars = 0;
        let (longest_row, longest_row_chars) = self.longest_row(&mut chars);
        TextSummary {
            len: self.len(),
            chars,
            len_utf16: self.len_utf16(),
            lines: self.lines(),
            first_line_chars: self.first_line_chars(),
            last_line_chars: self.last_line_chars(),
            last_line_len_utf16: self.last_line_len_utf16(),
            longest_row,
            longest_row_chars,
        }
    }

    /// Get length in bytes
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// Get length in UTF-16 code units
    #[inline(always)]
    pub fn len_utf16(&self) -> OffsetUtf16 {
        OffsetUtf16(self.chars_utf16.count_ones() as usize)
    }

    /// Get point representing number of lines and length of last line
    #[inline(always)]
    pub fn lines(&self) -> Point {
        let row = self.newlines.count_ones();
        let column = self.newlines.leading_zeros() - (u128::BITS - self.text.len() as u32);
        Point::new(row, column)
    }

    /// Get number of chars in first line
    #[inline(always)]
    pub fn first_line_chars(&self) -> u32 {
        if self.newlines == 0 {
            self.chars.count_ones()
        } else {
            let mask = (1u128 << self.newlines.trailing_zeros()) - 1;
            (self.chars & mask).count_ones()
        }
    }

    /// Get number of chars in last line
    #[inline(always)]
    pub fn last_line_chars(&self) -> u32 {
        if self.newlines == 0 {
            self.chars.count_ones()
        } else {
            let mask = !(u128::MAX >> self.newlines.leading_zeros());
            (self.chars & mask).count_ones()
        }
    }

    /// Get number of UTF-16 code units in last line
    #[inline(always)]
    pub fn last_line_len_utf16(&self) -> u32 {
        if self.newlines == 0 {
            self.chars_utf16.count_ones()
        } else {
            let mask = !(u128::MAX >> self.newlines.leading_zeros());
            (self.chars_utf16 & mask).count_ones()
        }
    }

    /// Get the longest row in the chunk and its length in characters.
    /// Calculate the total number of characters in the chunk along the way.
    #[inline(always)]
    pub fn longest_row(&self, total_chars: &mut usize) -> (u32, u32) {
        let mut chars = self.chars;
        let mut newlines = self.newlines;
        *total_chars = 0;
        let mut row = 0;
        let mut longest_row = 0;
        let mut longest_row_chars = 0;
        while newlines > 0 {
            let newline_ix = newlines.trailing_zeros();
            let row_chars = (chars & ((1 << newline_ix) - 1)).count_ones() as u8;
            *total_chars += usize::from(row_chars);
            if row_chars > longest_row_chars {
                longest_row = row;
                longest_row_chars = row_chars;
            }

            newlines >>= newline_ix;
            newlines >>= 1;
            chars >>= newline_ix;
            chars >>= 1;
            row += 1;
            *total_chars += 1;
        }

        let row_chars = chars.count_ones() as u8;
        *total_chars += usize::from(row_chars);
        if row_chars > longest_row_chars {
            (row, row_chars as u32)
        } else {
            (longest_row, longest_row_chars as u32)
        }
    }

    #[inline(always)]
    pub fn offset_to_point(&self, offset: usize) -> Point {
        let mask = if offset == MAX_BASE {
            u128::MAX
        } else {
            (1u128 << offset) - 1
        };
        let row = (self.newlines & mask).count_ones();
        let newline_ix = u128::BITS - (self.newlines & mask).leading_zeros();
        let column = (offset - newline_ix as usize) as u32;
        Point::new(row, column)
    }

    #[inline(always)]
    pub fn point_to_offset(&self, point: Point) -> usize {
        if point.row > self.lines().row {
            return self.len();
        }

        let row_offset_range = self.offset_range_for_row(point.row);
        if point.column > row_offset_range.len() as u32 {
            row_offset_range.end
        } else {
            row_offset_range.start + point.column as usize
        }
    }

    #[inline(always)]
    pub fn offset_to_offset_utf16(&self, offset: usize) -> OffsetUtf16 {
        let mask = if offset == MAX_BASE {
            u128::MAX
        } else {
            (1u128 << offset) - 1
        };
        OffsetUtf16((self.chars_utf16 & mask).count_ones() as usize)
    }

    #[inline(always)]
    pub fn offset_utf16_to_offset(&self, target: OffsetUtf16) -> usize {
        if target.0 == 0 {
            0
        } else {
            let ix = nth_set_bit(self.chars_utf16, target.0) + 1;
            if ix == MAX_BASE {
                MAX_BASE
            } else {
                let utf8_additional_len = cmp::min(
                    (self.chars_utf16 >> ix).trailing_zeros() as usize,
                    self.text.len() - ix,
                );
                ix + utf8_additional_len
            }
        }
    }

    #[inline(always)]
    pub fn offset_to_point_utf16(&self, offset: usize) -> PointUtf16 {
        let mask = if offset == MAX_BASE {
            u128::MAX
        } else {
            (1u128 << offset) - 1
        };
        let row = (self.newlines & mask).count_ones();
        let newline_ix = u128::BITS - (self.newlines & mask).leading_zeros();
        let column = if newline_ix as usize == MAX_BASE {
            0
        } else {
            ((self.chars_utf16 & mask) >> newline_ix).count_ones()
        };
        PointUtf16::new(row, column)
    }

    #[inline(always)]
    pub fn point_to_point_utf16(&self, point: Point) -> PointUtf16 {
        self.offset_to_point_utf16(self.point_to_offset(point))
    }

    #[inline(always)]
    pub fn point_utf16_to_offset(&self, point: PointUtf16) -> usize {
        let lines = self.lines();
        if point.row > lines.row {
            return self.len();
        }

        let row_offset_range = self.offset_range_for_row(point.row);
        let line = self.slice(row_offset_range.clone());
        if point.column > line.last_line_len_utf16() {
            return line.len();
        }

        let mut offset = row_offset_range.start;
        if point.column > 0 {
            offset += line.offset_utf16_to_offset(OffsetUtf16(point.column as usize));
            if !self.text.is_char_boundary(offset) {
                offset -= 1;
                while !self.text.is_char_boundary(offset) {
                    offset -= 1;
                }
            }
        }
        offset
    }

    #[inline(always)]
    pub fn unclipped_point_utf16_to_point(&self, point: Unclipped<PointUtf16>) -> Point {
        let max_point = self.lines();
        if point.0.row > max_point.row {
            return max_point;
        }

        let row_offset_range = self.offset_range_for_row(point.0.row);
        let line = self.slice(row_offset_range.clone());
        if point.0.column == 0 {
            Point::new(point.0.row, 0)
        } else if point.0.column >= line.len_utf16().0 as u32 {
            Point::new(point.0.row, line.len() as u32)
        } else {
            let mut column = line.offset_utf16_to_offset(OffsetUtf16(point.0.column as usize));
            while !line.text.is_char_boundary(column) {
                column -= 1;
            }
            Point::new(point.0.row, column as u32)
        }
    }

    #[inline(always)]
    pub fn clip_point(&self, point: Point, bias: Bias) -> Point {
        let max_point = self.lines();
        if point.row > max_point.row {
            return max_point;
        }

        let line = self.slice(self.offset_range_for_row(point.row));
        if point.column == 0 {
            point
        } else if point.column >= line.len() as u32 {
            Point::new(point.row, line.len() as u32)
        } else {
            let mut column = point.column as usize;
            let bytes = line.text.as_bytes();
            if bytes[column - 1] < 128 && bytes[column] < 128 {
                return Point::new(point.row, column as u32);
            }

            let mut grapheme_cursor = GraphemeCursor::new(column, bytes.len(), true);
            loop {
                if line.is_char_boundary(column)
                    && grapheme_cursor.is_boundary(line.text, 0).unwrap_or(false)
                {
                    break;
                }

                match bias {
                    Bias::Left => column -= 1,
                    Bias::Right => column += 1,
                }
                grapheme_cursor.set_cursor(column);
            }
            Point::new(point.row, column as u32)
        }
    }

    #[inline(always)]
    pub fn clip_point_utf16(&self, point: Unclipped<PointUtf16>, bias: Bias) -> PointUtf16 {
        let max_point = self.lines();
        if point.0.row > max_point.row {
            PointUtf16::new(max_point.row, self.last_line_len_utf16())
        } else {
            let line = self.slice(self.offset_range_for_row(point.0.row));
            let column = line.clip_offset_utf16(OffsetUtf16(point.0.column as usize), bias);
            PointUtf16::new(point.0.row, column.0 as u32)
        }
    }

    #[inline(always)]
    pub fn clip_offset_utf16(&self, target: OffsetUtf16, bias: Bias) -> OffsetUtf16 {
        if target == OffsetUtf16::default() {
            OffsetUtf16::default()
        } else if target >= self.len_utf16() {
            self.len_utf16()
        } else {
            let mut offset = self.offset_utf16_to_offset(target);
            while !self.text.is_char_boundary(offset) {
                if bias == Bias::Left {
                    offset -= 1;
                } else {
                    offset += 1;
                }
            }
            self.offset_to_offset_utf16(offset)
        }
    }

    #[inline(always)]
    fn offset_range_for_row(&self, row: u32) -> Range<usize> {
        let row_start = if row > 0 {
            nth_set_bit(self.newlines, row as usize) + 1
        } else {
            0
        };
        let row_len = if row_start == MAX_BASE {
            0
        } else {
            cmp::min(
                (self.newlines >> row_start).trailing_zeros(),
                (self.text.len() - row_start) as u32,
            )
        };
        row_start..row_start + row_len as usize
    }

    #[inline(always)]
    pub fn tabs(&self) -> Tabs {
        Tabs {
            tabs: self.tabs,
            chars: self.chars,
        }
    }
}

pub struct Tabs {
    tabs: u128,
    chars: u128,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TabPosition {
    pub byte_offset: usize,
    pub char_offset: usize,
}

impl Iterator for Tabs {
    type Item = TabPosition;

    fn next(&mut self) -> Option<Self::Item> {
        if self.tabs == 0 {
            return None;
        }

        let tab_offset = self.tabs.trailing_zeros() as usize;
        let chars_mask = (1 << tab_offset) - 1;
        let char_offset = (self.chars & chars_mask).count_ones() as usize;

        // Since tabs are 1 byte the tab offset is the same as the byte offset
        let position = TabPosition {
            byte_offset: tab_offset,
            char_offset: char_offset,
        };
        // Remove the tab we've just seen
        self.tabs ^= 1 << tab_offset;

        Some(position)
    }
}

/// Finds the n-th bit that is set to 1.
#[inline(always)]
fn nth_set_bit(v: u128, n: usize) -> usize {
    let low = v as u64;
    let high = (v >> 64) as u64;

    let low_count = low.count_ones() as usize;
    if n > low_count {
        64 + nth_set_bit_u64(high, (n - low_count) as u64) as usize
    } else {
        nth_set_bit_u64(low, n as u64) as usize
    }
}

#[inline(always)]
fn nth_set_bit_u64(v: u64, mut n: u64) -> u64 {
    let v = v.reverse_bits();
    let mut s: u64 = 64;

    // Parallel bit count intermediates
    let a = v - ((v >> 1) & (u64::MAX / 3));
    let b = (a & (u64::MAX / 5)) + ((a >> 2) & (u64::MAX / 5));
    let c = (b + (b >> 4)) & (u64::MAX / 0x11);
    let d = (c + (c >> 8)) & (u64::MAX / 0x101);

    // Branchless select
    let t = (d >> 32) + (d >> 48);
    s -= (t.wrapping_sub(n) & 256) >> 3;
    n -= t & (t.wrapping_sub(n) >> 8);

    let t = (d >> (s - 16)) & 0xff;
    s -= (t.wrapping_sub(n) & 256) >> 4;
    n -= t & (t.wrapping_sub(n) >> 8);

    let t = (c >> (s - 8)) & 0xf;
    s -= (t.wrapping_sub(n) & 256) >> 5;
    n -= t & (t.wrapping_sub(n) >> 8);

    let t = (b >> (s - 4)) & 0x7;
    s -= (t.wrapping_sub(n) & 256) >> 6;
    n -= t & (t.wrapping_sub(n) >> 8);

    let t = (a >> (s - 2)) & 0x3;
    s -= (t.wrapping_sub(n) & 256) >> 7;
    n -= t & (t.wrapping_sub(n) >> 8);

    let t = (v >> (s - 1)) & 0x1;
    s -= (t.wrapping_sub(n) & 256) >> 8;

    65 - s - 1
}
