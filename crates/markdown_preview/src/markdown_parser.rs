use crate::markdown_elements::*;
use async_recursion::async_recursion;
use collections::FxHashMap;
use gpui::FontWeight;
use language::LanguageRegistry;
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use std::{ops::Range, path::PathBuf, sync::Arc, vec};

pub async fn parse_markdown(
    markdown_input: &str,
    file_location_directory: Option<PathBuf>,
    language_registry: Option<Arc<LanguageRegistry>>,
) -> ParsedMarkdown {
    let mut options = Options::all();
    options.remove(pulldown_cmark::Options::ENABLE_DEFINITION_LIST);

    let parser = Parser::new_ext(markdown_input, options);
    let parser = MarkdownParser::new(
        parser.into_offset_iter().collect(),
        file_location_directory,
        language_registry,
    );
    let renderer = parser.parse_document().await;
    ParsedMarkdown {
        children: renderer.parsed,
    }
}

struct MarkdownParser<'a> {
    tokens: Vec<(Event<'a>, Range<usize>)>,
    /// The current index in the tokens array
    cursor: usize,
    /// The blocks that we have successfully parsed so far
    parsed: Vec<ParsedMarkdownElement>,
    file_location_directory: Option<PathBuf>,
    language_registry: Option<Arc<LanguageRegistry>>,
}

struct MarkdownListItem {
    content: Vec<ParsedMarkdownElement>,
    item_type: ParsedMarkdownListItemType,
}

impl Default for MarkdownListItem {
    fn default() -> Self {
        Self {
            content: Vec::new(),
            item_type: ParsedMarkdownListItemType::Unordered,
        }
    }
}

impl<'a> MarkdownParser<'a> {
    fn new(
        tokens: Vec<(Event<'a>, Range<usize>)>,
        file_location_directory: Option<PathBuf>,
        language_registry: Option<Arc<LanguageRegistry>>,
    ) -> Self {
        Self {
            tokens,
            file_location_directory,
            language_registry,
            cursor: 0,
            parsed: vec![],
        }
    }

    fn eof(&self) -> bool {
        if self.tokens.is_empty() {
            return true;
        }
        self.cursor >= self.tokens.len() - 1
    }

    fn peek(&self, steps: usize) -> Option<&(Event, Range<usize>)> {
        if self.eof() || (steps + self.cursor) >= self.tokens.len() {
            return self.tokens.last();
        }
        return self.tokens.get(self.cursor + steps);
    }

    fn previous(&self) -> Option<&(Event, Range<usize>)> {
        if self.cursor == 0 || self.cursor > self.tokens.len() {
            return None;
        }
        return self.tokens.get(self.cursor - 1);
    }

    fn current(&self) -> Option<&(Event, Range<usize>)> {
        return self.peek(0);
    }

    fn current_event(&self) -> Option<&Event> {
        return self.current().map(|(event, _)| event);
    }

    fn is_text_like(event: &Event) -> bool {
        match event {
            Event::Text(_)
            // Represent an inline code block
            | Event::Code(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::Start(Tag::Link { .. })
            | Event::Start(Tag::Emphasis)
            | Event::Start(Tag::Strong)
            | Event::Start(Tag::Strikethrough)
            | Event::Start(Tag::Image { .. }) => {
                true
            }
            _ => false,
        }
    }

    async fn parse_document(mut self) -> Self {
        while !self.eof() {
            if let Some(block) = self.parse_block().await {
                self.parsed.extend(block);
            } else {
                self.cursor += 1;
            }
        }
        self
    }

    #[async_recursion]
    async fn parse_block(&mut self) -> Option<Vec<ParsedMarkdownElement>> {
        let (current, source_range) = self.current().unwrap();
        let source_range = source_range.clone();
        match current {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    self.cursor += 1;
                    let text = self.parse_text(false, Some(source_range));
                    Some(vec![ParsedMarkdownElement::Paragraph(text)])
                }
                Tag::Heading { level, .. } => {
                    let level = *level;
                    self.cursor += 1;
                    let heading = self.parse_heading(level);
                    Some(vec![ParsedMarkdownElement::Heading(heading)])
                }
                Tag::Table(alignment) => {
                    let alignment = alignment.clone();
                    self.cursor += 1;
                    let table = self.parse_table(alignment);
                    Some(vec![ParsedMarkdownElement::Table(table)])
                }
                Tag::List(order) => {
                    let order = *order;
                    self.cursor += 1;
                    let list = self.parse_list(order).await;
                    Some(list)
                }
                Tag::BlockQuote(_kind) => {
                    self.cursor += 1;
                    let block_quote = self.parse_block_quote().await;
                    Some(vec![ParsedMarkdownElement::BlockQuote(block_quote)])
                }
                Tag::CodeBlock(kind) => {
                    let language = match kind {
                        pulldown_cmark::CodeBlockKind::Indented => None,
                        pulldown_cmark::CodeBlockKind::Fenced(language) => {
                            if language.is_empty() {
                                None
                            } else {
                                Some(language.to_string())
                            }
                        }
                    };

                    self.cursor += 1;

                    let code_block = self.parse_code_block(language).await;
                    Some(vec![ParsedMarkdownElement::CodeBlock(code_block)])
                }
                _ => None,
            },
            Event::Rule => {
                let source_range = source_range.clone();
                self.cursor += 1;
                Some(vec![ParsedMarkdownElement::HorizontalRule(source_range)])
            }
            _ => None,
        }
    }

    fn parse_text(
        &mut self,
        should_complete_on_soft_break: bool,
        source_range: Option<Range<usize>>,
    ) -> MarkdownParagraph {
        let source_range = source_range.unwrap_or_else(|| {
            self.current()
                .map(|(_, range)| range.clone())
                .unwrap_or_default()
        });

        let mut markdown_text_like = Vec::new();
        let mut text = String::new();
        let mut bold_depth = 0;
        let mut italic_depth = 0;
        let mut strikethrough_depth = 0;
        let mut link: Option<Link> = None;
        let mut image: Option<Image> = None;
        let mut region_ranges: Vec<Range<usize>> = vec![];
        let mut regions: Vec<ParsedRegion> = vec![];
        let mut highlights: Vec<(Range<usize>, MarkdownHighlight)> = vec![];
        let mut link_urls: Vec<String> = vec![];
        let mut link_ranges: Vec<Range<usize>> = vec![];

        loop {
            if self.eof() {
                break;
            }

            let (current, _) = self.current().unwrap();
            let prev_len = text.len();
            match current {
                Event::SoftBreak => {
                    if should_complete_on_soft_break {
                        break;
                    }
                    text.push(' ');
                }

                Event::HardBreak => {
                    text.push('\n');
                }

                // We want to ignore any inline HTML tags in the text but keep
                // the text between them
                Event::InlineHtml(_) => {}

                Event::Text(t) => {
                    text.push_str(t.as_ref());
                    let mut style = MarkdownHighlightStyle::default();

                    if bold_depth > 0 {
                        style.weight = FontWeight::BOLD;
                    }

                    if italic_depth > 0 {
                        style.italic = true;
                    }

                    if strikethrough_depth > 0 {
                        style.strikethrough = true;
                    }

                    let last_run_len = if let Some(link) = link.clone() {
                        region_ranges.push(prev_len..text.len());
                        regions.push(ParsedRegion {
                            code: false,
                            link: Some(link),
                        });
                        style.underline = true;
                        prev_len
                    } else {
                        // Manually scan for links
                        let mut finder = linkify::LinkFinder::new();
                        finder.kinds(&[linkify::LinkKind::Url]);
                        let mut last_link_len = prev_len;
                        for link in finder.links(t) {
                            let start = link.start();
                            let end = link.end();
                            let range = (prev_len + start)..(prev_len + end);
                            link_ranges.push(range.clone());
                            link_urls.push(link.as_str().to_string());

                            // If there is a style before we match a link, we have to add this to the highlighted ranges
                            if style != MarkdownHighlightStyle::default()
                                && last_link_len < link.start()
                            {
                                highlights.push((
                                    last_link_len..link.start(),
                                    MarkdownHighlight::Style(style.clone()),
                                ));
                            }

                            highlights.push((
                                range.clone(),
                                MarkdownHighlight::Style(MarkdownHighlightStyle {
                                    underline: true,
                                    ..style
                                }),
                            ));
                            region_ranges.push(range.clone());
                            regions.push(ParsedRegion {
                                code: false,
                                link: Some(Link::Web {
                                    url: link.as_str().to_string(),
                                }),
                            });
                            last_link_len = end;
                        }
                        last_link_len
                    };

                    if style != MarkdownHighlightStyle::default() && last_run_len < text.len() {
                        let mut new_highlight = true;
                        if let Some((last_range, last_style)) = highlights.last_mut() {
                            if last_range.end == last_run_len
                                && last_style == &MarkdownHighlight::Style(style.clone())
                            {
                                last_range.end = text.len();
                                new_highlight = false;
                            }
                        }
                        if new_highlight {
                            highlights.push((
                                last_run_len..text.len(),
                                MarkdownHighlight::Style(style.clone()),
                            ));
                        }
                    }
                }
                Event::Code(t) => {
                    text.push_str(t.as_ref());
                    region_ranges.push(prev_len..text.len());

                    if link.is_some() {
                        highlights.push((
                            prev_len..text.len(),
                            MarkdownHighlight::Style(MarkdownHighlightStyle {
                                underline: true,
                                ..Default::default()
                            }),
                        ));
                    }
                    regions.push(ParsedRegion {
                        code: true,
                        link: link.clone(),
                    });
                }
                Event::Start(tag) => match tag {
                    Tag::Emphasis => italic_depth += 1,
                    Tag::Strong => bold_depth += 1,
                    Tag::Strikethrough => strikethrough_depth += 1,
                    Tag::Link { dest_url, .. } => {
                        link = Link::identify(
                            self.file_location_directory.clone(),
                            dest_url.to_string(),
                        );
                    }
                    Tag::Image { dest_url, .. } => {
                        if !text.is_empty() {
                            let parsed_regions = MarkdownParagraphChunk::Text(ParsedMarkdownText {
                                source_range: source_range.clone(),
                                contents: text.clone(),
                                highlights: highlights.clone(),
                                region_ranges: region_ranges.clone(),
                                regions: regions.clone(),
                            });
                            text = String::new();
                            highlights = vec![];
                            region_ranges = vec![];
                            regions = vec![];
                            markdown_text_like.push(parsed_regions);
                        }
                        image = Image::identify(
                            dest_url.to_string(),
                            source_range.clone(),
                            self.file_location_directory.clone(),
                        );
                    }
                    _ => {
                        break;
                    }
                },

                Event::End(tag) => match tag {
                    TagEnd::Emphasis => italic_depth -= 1,
                    TagEnd::Strong => bold_depth -= 1,
                    TagEnd::Strikethrough => strikethrough_depth -= 1,
                    TagEnd::Link => {
                        link = None;
                    }
                    TagEnd::Image => {
                        if let Some(mut image) = image.take() {
                            if !text.is_empty() {
                                image.alt_text = Some(std::mem::take(&mut text).into());
                            }
                            markdown_text_like.push(MarkdownParagraphChunk::Image(image));
                        }
                    }
                    TagEnd::Paragraph => {
                        self.cursor += 1;
                        break;
                    }
                    _ => {
                        break;
                    }
                },
                _ => {
                    break;
                }
            }

            self.cursor += 1;
        }
        if !text.is_empty() {
            markdown_text_like.push(MarkdownParagraphChunk::Text(ParsedMarkdownText {
                source_range: source_range.clone(),
                contents: text,
                highlights,
                regions,
                region_ranges,
            }));
        }
        markdown_text_like
    }

    fn parse_heading(&mut self, level: pulldown_cmark::HeadingLevel) -> ParsedMarkdownHeading {
        let (_event, source_range) = self.previous().unwrap();
        let source_range = source_range.clone();
        let text = self.parse_text(true, None);

        // Advance past the heading end tag
        self.cursor += 1;

        ParsedMarkdownHeading {
            source_range: source_range.clone(),
            level: match level {
                pulldown_cmark::HeadingLevel::H1 => HeadingLevel::H1,
                pulldown_cmark::HeadingLevel::H2 => HeadingLevel::H2,
                pulldown_cmark::HeadingLevel::H3 => HeadingLevel::H3,
                pulldown_cmark::HeadingLevel::H4 => HeadingLevel::H4,
                pulldown_cmark::HeadingLevel::H5 => HeadingLevel::H5,
                pulldown_cmark::HeadingLevel::H6 => HeadingLevel::H6,
            },
            contents: text,
        }
    }

    fn parse_table(&mut self, alignment: Vec<Alignment>) -> ParsedMarkdownTable {
        let (_event, source_range) = self.previous().unwrap();
        let source_range = source_range.clone();
        let mut header = ParsedMarkdownTableRow::new();
        let mut body = vec![];
        let mut current_row = vec![];
        let mut in_header = true;
        let column_alignments = alignment.iter().map(Self::convert_alignment).collect();

        loop {
            if self.eof() {
                break;
            }

            let (current, source_range) = self.current().unwrap();
            let source_range = source_range.clone();
            match current {
                Event::Start(Tag::TableHead)
                | Event::Start(Tag::TableRow)
                | Event::End(TagEnd::TableCell) => {
                    self.cursor += 1;
                }
                Event::Start(Tag::TableCell) => {
                    self.cursor += 1;
                    let cell_contents = self.parse_text(false, Some(source_range));
                    current_row.push(cell_contents);
                }
                Event::End(TagEnd::TableHead) | Event::End(TagEnd::TableRow) => {
                    self.cursor += 1;
                    let new_row = std::mem::take(&mut current_row);
                    if in_header {
                        header.children = new_row;
                        in_header = false;
                    } else {
                        let row = ParsedMarkdownTableRow::with_children(new_row);
                        body.push(row);
                    }
                }
                Event::End(TagEnd::Table) => {
                    self.cursor += 1;
                    break;
                }
                _ => {
                    break;
                }
            }
        }

        ParsedMarkdownTable {
            source_range,
            header,
            body,
            column_alignments,
        }
    }

    fn convert_alignment(alignment: &Alignment) -> ParsedMarkdownTableAlignment {
        match alignment {
            Alignment::None => ParsedMarkdownTableAlignment::None,
            Alignment::Left => ParsedMarkdownTableAlignment::Left,
            Alignment::Center => ParsedMarkdownTableAlignment::Center,
            Alignment::Right => ParsedMarkdownTableAlignment::Right,
        }
    }

    async fn parse_list(&mut self, order: Option<u64>) -> Vec<ParsedMarkdownElement> {
        let (_, list_source_range) = self.previous().unwrap();

        let mut items = Vec::new();
        let mut items_stack = vec![MarkdownListItem::default()];
        let mut depth = 1;
        let mut order = order;
        let mut order_stack = Vec::new();

        let mut insertion_indices = FxHashMap::default();
        let mut source_ranges = FxHashMap::default();
        let mut start_item_range = list_source_range.clone();

        while !self.eof() {
            let (current, source_range) = self.current().unwrap();
            match current {
                Event::Start(Tag::List(new_order)) => {
                    if items_stack.last().is_some() && !insertion_indices.contains_key(&depth) {
                        insertion_indices.insert(depth, items.len());
                    }

                    // We will use the start of the nested list as the end for the current item's range,
                    // because we don't care about the hierarchy of list items
                    if let collections::hash_map::Entry::Vacant(e) = source_ranges.entry(depth) {
                        e.insert(start_item_range.start..source_range.start);
                    }

                    order_stack.push(order);
                    order = *new_order;
                    self.cursor += 1;
                    depth += 1;
                }
                Event::End(TagEnd::List(_)) => {
                    order = order_stack.pop().flatten();
                    self.cursor += 1;
                    depth -= 1;

                    if depth == 0 {
                        break;
                    }
                }
                Event::Start(Tag::Item) => {
                    start_item_range = source_range.clone();

                    self.cursor += 1;
                    items_stack.push(MarkdownListItem::default());

                    let mut task_list = None;
                    // Check for task list marker (`- [ ]` or `- [x]`)
                    if let Some(event) = self.current_event() {
                        // If there is a linebreak in between two list items the task list marker will actually be the first element of the paragraph
                        if event == &Event::Start(Tag::Paragraph) {
                            self.cursor += 1;
                        }

                        if let Some((Event::TaskListMarker(checked), range)) = self.current() {
                            task_list = Some((*checked, range.clone()));
                            self.cursor += 1;
                        }
                    }

                    if let Some((event, range)) = self.current() {
                        // This is a plain list item.
                        // For example `- some text` or `1. [Docs](./docs.md)`
                        if MarkdownParser::is_text_like(event) {
                            let text = self.parse_text(false, Some(range.clone()));
                            let block = ParsedMarkdownElement::Paragraph(text);
                            if let Some(content) = items_stack.last_mut() {
                                let item_type = if let Some((checked, range)) = task_list {
                                    ParsedMarkdownListItemType::Task(checked, range)
                                } else if let Some(order) = order {
                                    ParsedMarkdownListItemType::Ordered(order)
                                } else {
                                    ParsedMarkdownListItemType::Unordered
                                };
                                content.item_type = item_type;
                                content.content.push(block);
                            }
                        } else {
                            let block = self.parse_block().await;
                            if let Some(block) = block {
                                if let Some(list_item) = items_stack.last_mut() {
                                    list_item.content.extend(block);
                                }
                            }
                        }
                    }

                    // If there is a linebreak in between two list items the task list marker will actually be the first element of the paragraph
                    if self.current_event() == Some(&Event::End(TagEnd::Paragraph)) {
                        self.cursor += 1;
                    }
                }
                Event::End(TagEnd::Item) => {
                    self.cursor += 1;

                    if let Some(current) = order {
                        order = Some(current + 1);
                    }

                    if let Some(list_item) = items_stack.pop() {
                        let source_range = source_ranges
                            .remove(&depth)
                            .unwrap_or(start_item_range.clone());

                        // We need to remove the last character of the source range, because it includes the newline character
                        let source_range = source_range.start..source_range.end - 1;
                        let item = ParsedMarkdownElement::ListItem(ParsedMarkdownListItem {
                            source_range,
                            content: list_item.content,
                            depth,
                            item_type: list_item.item_type,
                        });

                        if let Some(index) = insertion_indices.get(&depth) {
                            items.insert(*index, item);
                            insertion_indices.remove(&depth);
                        } else {
                            items.push(item);
                        }
                    }
                }
                _ => {
                    if depth == 0 {
                        break;
                    }
                    // This can only happen if a list item starts with more then one paragraph,
                    // or the list item contains blocks that should be rendered after the nested list items
                    let block = self.parse_block().await;
                    if let Some(block) = block {
                        if let Some(list_item) = items_stack.last_mut() {
                            // If we did not insert any nested items yet (in this case insertion index is set), we can append the block to the current list item
                            if !insertion_indices.contains_key(&depth) {
                                list_item.content.extend(block);
                                continue;
                            }
                        }

                        // Otherwise we need to insert the block after all the nested items
                        // that have been parsed so far
                        items.extend(block);
                    } else {
                        self.cursor += 1;
                    }
                }
            }
        }

        items
    }

    #[async_recursion]
    async fn parse_block_quote(&mut self) -> ParsedMarkdownBlockQuote {
        let (_event, source_range) = self.previous().unwrap();
        let source_range = source_range.clone();
        let mut nested_depth = 1;

        let mut children: Vec<ParsedMarkdownElement> = vec![];

        while !self.eof() {
            let block = self.parse_block().await;

            if let Some(block) = block {
                children.extend(block);
            } else {
                break;
            }

            if self.eof() {
                break;
            }

            let (current, _source_range) = self.current().unwrap();
            match current {
                // This is a nested block quote.
                // Record that we're in a nested block quote and continue parsing.
                // We don't need to advance the cursor since the next
                // call to `parse_block` will handle it.
                Event::Start(Tag::BlockQuote(_kind)) => {
                    nested_depth += 1;
                }
                Event::End(TagEnd::BlockQuote(_kind)) => {
                    nested_depth -= 1;
                    if nested_depth == 0 {
                        self.cursor += 1;
                        break;
                    }
                }
                _ => {}
            };
        }

        ParsedMarkdownBlockQuote {
            source_range,
            children,
        }
    }

    async fn parse_code_block(&mut self, language: Option<String>) -> ParsedMarkdownCodeBlock {
        let (_event, source_range) = self.previous().unwrap();
        let source_range = source_range.clone();
        let mut code = String::new();

        while !self.eof() {
            let (current, _source_range) = self.current().unwrap();
            match current {
                Event::Text(text) => {
                    code.push_str(text);
                    self.cursor += 1;
                }
                Event::End(TagEnd::CodeBlock) => {
                    self.cursor += 1;
                    break;
                }
                _ => {
                    break;
                }
            }
        }

        code = code.strip_suffix('\n').unwrap_or(&code).to_string();

        let highlights = if let Some(language) = &language {
            if let Some(registry) = &self.language_registry {
                let rope: language::Rope = code.as_str().into();
                registry
                    .language_for_name_or_extension(language)
                    .await
                    .map(|l| l.highlight_text(&rope, 0..code.len()))
                    .ok()
            } else {
                None
            }
        } else {
            None
        };

        ParsedMarkdownCodeBlock {
            source_range,
            contents: code.into(),
            language,
            highlights,
        }
    }
}
