use anyhow::{Context as _, Result};
use smallvec::SmallVec;
use std::{collections::BTreeMap, ops::Range};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snippet {
    pub text: String,
    pub tabstops: Vec<TabStop>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabStop {
    pub ranges: SmallVec<[Range<isize>; 2]>,
    pub choices: Option<Vec<String>>,
}

impl Snippet {
    pub fn parse(source: &str) -> Result<Self> {
        let mut text = String::with_capacity(source.len());
        let mut tabstops = BTreeMap::new();
        parse_snippet(source, false, &mut text, &mut tabstops)
            .context("failed to parse snippet")?;

        let len = text.len() as isize;
        let final_tabstop = tabstops.remove(&0);
        let mut tabstops = tabstops.into_values().collect::<Vec<_>>();

        if let Some(final_tabstop) = final_tabstop {
            tabstops.push(final_tabstop);
        } else {
            let end_tabstop = TabStop {
                ranges: [len..len].into_iter().collect(),
                choices: None,
            };

            if !tabstops.last().map_or(false, |t| *t == end_tabstop) {
                tabstops.push(end_tabstop);
            }
        }

        Ok(Snippet { text, tabstops })
    }
}

fn parse_snippet<'a>(
    mut source: &'a str,
    nested: bool,
    text: &mut String,
    tabstops: &mut BTreeMap<usize, TabStop>,
) -> Result<&'a str> {
    loop {
        match source.chars().next() {
            None => return Ok(""),
            Some('$') => {
                source = parse_tabstop(&source[1..], text, tabstops)?;
            }
            Some('\\') => {
                // As specified in the LSP spec (`Grammar` section),
                // backslashes can escape some characters:
                // https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#snippet_syntax
                source = &source[1..];
                if let Some(c) = source.chars().next() {
                    if c == '$' || c == '\\' || c == '}' {
                        text.push(c);
                        // All escapable characters are 1 byte long:
                        source = &source[1..];
                    } else {
                        text.push('\\');
                    }
                } else {
                    text.push('\\');
                }
            }
            Some('}') => {
                if nested {
                    return Ok(source);
                } else {
                    text.push('}');
                    source = &source[1..];
                }
            }
            Some(_) => {
                let chunk_end = source.find(['}', '$', '\\']).unwrap_or(source.len());
                let (chunk, rest) = source.split_at(chunk_end);
                text.push_str(chunk);
                source = rest;
            }
        }
    }
}

fn parse_tabstop<'a>(
    mut source: &'a str,
    text: &mut String,
    tabstops: &mut BTreeMap<usize, TabStop>,
) -> Result<&'a str> {
    let tabstop_start = text.len();
    let tabstop_index;
    let mut choices = None;

    if source.starts_with('{') {
        let (index, rest) = parse_int(&source[1..])?;
        tabstop_index = index;
        source = rest;

        if source.starts_with("|") {
            (source, choices) = parse_choices(&source[1..], text)?;
        }

        if source.starts_with(':') {
            source = parse_snippet(&source[1..], true, text, tabstops)?;
        }

        if source.starts_with('}') {
            source = &source[1..];
        } else {
            anyhow::bail!("expected a closing brace");
        }
    } else {
        let (index, rest) = parse_int(source)?;
        tabstop_index = index;
        source = rest;
    }

    tabstops
        .entry(tabstop_index)
        .or_insert_with(|| TabStop {
            ranges: Default::default(),
            choices,
        })
        .ranges
        .push(tabstop_start as isize..text.len() as isize);
    Ok(source)
}

fn parse_int(source: &str) -> Result<(usize, &str)> {
    let len = source
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(source.len());
    anyhow::ensure!(len > 0, "expected an integer");
    let (prefix, suffix) = source.split_at(len);
    Ok((prefix.parse()?, suffix))
}

fn parse_choices<'a>(
    mut source: &'a str,
    text: &mut String,
) -> Result<(&'a str, Option<Vec<String>>)> {
    let mut found_default_choice = false;
    let mut current_choice = String::new();
    let mut choices = Vec::new();

    loop {
        match source.chars().next() {
            None => return Ok(("", Some(choices))),
            Some('\\') => {
                source = &source[1..];

                if let Some(c) = source.chars().next() {
                    if !found_default_choice {
                        current_choice.push(c);
                        text.push(c);
                    }
                    source = &source[c.len_utf8()..];
                }
            }
            Some(',') => {
                found_default_choice = true;
                source = &source[1..];
                choices.push(current_choice);
                current_choice = String::new();
            }
            Some('|') => {
                source = &source[1..];
                choices.push(current_choice);
                return Ok((source, Some(choices)));
            }
            Some(_) => {
                let chunk_end = source.find([',', '|', '\\']);

                anyhow::ensure!(
                    chunk_end.is_some(),
                    "Placeholder choice doesn't contain closing pipe-character '|'"
                );

                let (chunk, rest) = source.split_at(chunk_end.unwrap());

                if !found_default_choice {
                    text.push_str(chunk);
                }

                current_choice.push_str(chunk);
                source = rest;
            }
        }
    }
}
