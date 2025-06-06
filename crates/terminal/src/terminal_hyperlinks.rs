use alacritty_terminal::{
    Term,
    event::EventListener,
    grid::Dimensions,
    index::{Boundary, Column, Direction as AlacDirection, Line, Point as AlacPoint},
    term::search::{Match, RegexIter, RegexSearch},
};
use regex::Regex;
use std::{ops::Index, sync::LazyLock};

const URL_REGEX: &str = r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"\s{-}\^⟨⟩`]+"#;
// Optional suffix matches MSBuild diagnostic suffixes for path parsing in PathLikeWithPosition
// https://learn.microsoft.com/en-us/visualstudio/msbuild/msbuild-diagnostic-format-for-tasks
const WORD_REGEX: &str =
    r#"[\$\+\w.\[\]:/\\@\-~()]+(?:\((?:\d+|\d+,\d+)\))|[\$\+\w.\[\]:/\\@\-~()]+"#;

const PYTHON_FILE_LINE_REGEX: &str = r#"File "(?P<file>[^"]+)", line (?P<line>\d+)"#;

static PYTHON_FILE_LINE_MATCHER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(PYTHON_FILE_LINE_REGEX).unwrap());

fn python_extract_path_and_line(input: &str) -> Option<(&str, u32)> {
    if let Some(captures) = PYTHON_FILE_LINE_MATCHER.captures(input) {
        let path_part = captures.name("file")?.as_str();

        let line_number: u32 = captures.name("line")?.as_str().parse().ok()?;
        return Some((path_part, line_number));
    }
    None
}

pub(super) struct RegexSearches {
    url_regex: RegexSearch,
    word_regex: RegexSearch,
    python_file_line_regex: RegexSearch,
}

impl RegexSearches {
    pub(super) fn new() -> Self {
        Self {
            url_regex: RegexSearch::new(URL_REGEX).unwrap(),
            word_regex: RegexSearch::new(WORD_REGEX).unwrap(),
            python_file_line_regex: RegexSearch::new(PYTHON_FILE_LINE_REGEX).unwrap(),
        }
    }
}

pub(super) fn find_from_grid_point<T: EventListener>(
    term: &Term<T>,
    point: AlacPoint,
    regex_searches: &mut RegexSearches,
) -> Option<(String, bool, Match)> {
    let grid = term.grid();
    let link = grid.index(point).hyperlink();
    let found_word = if link.is_some() {
        let mut min_index = point;
        loop {
            let new_min_index = min_index.sub(term, Boundary::Cursor, 1);
            if new_min_index == min_index || grid.index(new_min_index).hyperlink() != link {
                break;
            } else {
                min_index = new_min_index
            }
        }

        let mut max_index = point;
        loop {
            let new_max_index = max_index.add(term, Boundary::Cursor, 1);
            if new_max_index == max_index || grid.index(new_max_index).hyperlink() != link {
                break;
            } else {
                max_index = new_max_index
            }
        }

        let url = link.unwrap().uri().to_owned();
        let url_match = min_index..=max_index;

        Some((url, true, url_match))
    } else if let Some(url_match) = regex_match_at(term, point, &mut regex_searches.url_regex) {
        let url = term.bounds_to_string(*url_match.start(), *url_match.end());
        Some((url, true, url_match))
    } else if let Some(python_match) =
        regex_match_at(term, point, &mut regex_searches.python_file_line_regex)
    {
        let matching_line = term.bounds_to_string(*python_match.start(), *python_match.end());
        python_extract_path_and_line(&matching_line).map(|(file_path, line_number)| {
            (format!("{file_path}:{line_number}"), false, python_match)
        })
    } else if let Some(word_match) = regex_match_at(term, point, &mut regex_searches.word_regex) {
        let file_path = term.bounds_to_string(*word_match.start(), *word_match.end());

        let (sanitized_match, sanitized_word) = 'sanitize: {
            let mut word_match = word_match;
            let mut file_path = file_path;

            if is_path_surrounded_by_common_symbols(&file_path) {
                word_match = Match::new(
                    word_match.start().add(term, Boundary::Grid, 1),
                    word_match.end().sub(term, Boundary::Grid, 1),
                );
                file_path = file_path[1..file_path.len() - 1].to_owned();
            }

            while file_path.ends_with(':') {
                file_path.pop();
                word_match = Match::new(
                    *word_match.start(),
                    word_match.end().sub(term, Boundary::Grid, 1),
                );
            }
            let mut colon_count = 0;
            for c in file_path.chars() {
                if c == ':' {
                    colon_count += 1;
                }
            }
            // strip trailing comment after colon in case of
            // file/at/path.rs:row:column:description or error message
            // so that the file path is `file/at/path.rs:row:column`
            if colon_count > 2 {
                let last_index = file_path.rfind(':').unwrap();
                let prev_is_digit = last_index > 0
                    && file_path
                        .chars()
                        .nth(last_index - 1)
                        .map_or(false, |c| c.is_ascii_digit());
                let next_is_digit = last_index < file_path.len() - 1
                    && file_path
                        .chars()
                        .nth(last_index + 1)
                        .map_or(true, |c| c.is_ascii_digit());
                if prev_is_digit && !next_is_digit {
                    let stripped_len = file_path.len() - last_index;
                    word_match = Match::new(
                        *word_match.start(),
                        word_match.end().sub(term, Boundary::Grid, stripped_len),
                    );
                    file_path = file_path[0..last_index].to_owned();
                }
            }

            break 'sanitize (word_match, file_path);
        };

        Some((sanitized_word, false, sanitized_match))
    } else {
        None
    };

    found_word.map(|(maybe_url_or_path, is_url, word_match)| {
        if is_url {
            // Treat "file://" IRIs like file paths to ensure
            // that line numbers at the end of the path are
            // handled correctly
            if let Some(path) = maybe_url_or_path.strip_prefix("file://") {
                (path.to_string(), false, word_match)
            } else {
                (maybe_url_or_path, true, word_match)
            }
        } else {
            (maybe_url_or_path, false, word_match)
        }
    })
}

fn is_path_surrounded_by_common_symbols(path: &str) -> bool {
    // Avoid detecting `[]` or `()` strings as paths, surrounded by common symbols
    path.len() > 2
        // The rest of the brackets and various quotes cannot be matched by the [`WORD_REGEX`] hence not checked for.
        && (path.starts_with('[') && path.ends_with(']')
            || path.starts_with('(') && path.ends_with(')'))
}

/// Based on alacritty/src/display/hint.rs > regex_match_at
/// Retrieve the match, if the specified point is inside the content matching the regex.
fn regex_match_at<T>(term: &Term<T>, point: AlacPoint, regex: &mut RegexSearch) -> Option<Match> {
    visible_regex_match_iter(term, regex).find(|rm| rm.contains(&point))
}

/// Copied from alacritty/src/display/hint.rs:
/// Iterate over all visible regex matches.
fn visible_regex_match_iter<'a, T>(
    term: &'a Term<T>,
    regex: &'a mut RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
    const MAX_SEARCH_LINES: usize = 100;

    let viewport_start = Line(-(term.grid().display_offset() as i32));
    let viewport_end = viewport_start + term.bottommost_line();
    let mut start = term.line_search_left(AlacPoint::new(viewport_start, Column(0)));
    let mut end = term.line_search_right(AlacPoint::new(viewport_end, Column(0)));
    start.line = start.line.max(viewport_start - MAX_SEARCH_LINES);
    end.line = end.line.min(viewport_end + MAX_SEARCH_LINES);

    RegexIter::new(start, end, AlacDirection::Right, term, regex)
        .skip_while(move |rm| rm.end().line < viewport_start)
        .take_while(move |rm| rm.start().line <= viewport_end)
}
