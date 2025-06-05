use std::{ops::Range, path::Path, sync::Arc};

#[derive(Debug, Clone, PartialEq)]
pub struct PathWithRange {
    pub path: Arc<Path>,
    pub range: Option<Range<LineCol>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct LineCol {
    pub line: u32,
    pub col: Option<u32>,
}

impl std::fmt::Debug for LineCol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.col {
            Some(col) => write!(f, "L{}:{}", self.line, col),
            None => write!(f, "L{}", self.line),
        }
    }
}

impl LineCol {
    pub fn new(str: impl AsRef<str>) -> Option<Self> {
        let str = str.as_ref();
        match str.split_once(':') {
            Some((line, col)) => match (line.parse::<u32>(), col.parse::<u32>()) {
                (Ok(line), Ok(col)) => Some(Self {
                    line,
                    col: Some(col),
                }),
                _ => None,
            },
            None => match str.parse::<u32>() {
                Ok(line) => Some(Self { line, col: None }),
                Err(_) => None,
            },
        }
    }
}

impl PathWithRange {
    // Note: We could try out this as an alternative, and see how it does on evals.
    //
    // The closest to a standard way of including a filename is this:
    // ```rust filename="path/to/file.rs#42:43"
    // ```
    //
    // or, alternatively,
    // ```rust filename="path/to/file.rs" lines="42:43"
    // ```
    //
    // Examples where it's used this way:
    // - https://mdxjs.com/guides/syntax-highlighting/#syntax-highlighting-with-the-meta-field
    // - https://docusaurus.io/docs/markdown-features/code-blocks
    // - https://spec.commonmark.org/0.31.2/#example-143
    pub fn new(str: impl AsRef<str>) -> Self {
        let str = str.as_ref();
        // Sometimes the model will include a language at the start,
        // e.g. "```rust zed/crates/markdown/src/markdown.rs#L1"
        // We just discard that.
        let str = match str.trim_end().rfind(' ') {
            Some(space) => &str[space + 1..],
            None => str.trim_start(),
        };

        match str.rsplit_once('#') {
            Some((path, after_hash)) => {
                // Be tolerant to the model omitting the "L" prefix, lowercasing it,
                // or including it more than once.
                let after_hash = after_hash.replace(['L', 'l'], "");

                let range = {
                    let mut iter = after_hash.split('-').flat_map(LineCol::new);
                    iter.next()
                        .map(|start| iter.next().map(|end| start..end).unwrap_or(start..start))
                };

                Self {
                    path: Path::new(path).into(),
                    range,
                }
            }
            None => Self {
                path: Path::new(str).into(),
                range: None,
            },
        }
    }
}
