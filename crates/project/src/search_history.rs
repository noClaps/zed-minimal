use std::collections::VecDeque;

/// Determines the behavior to use when inserting a new query into the search history.
#[derive(Default, Debug, Clone, PartialEq)]
pub enum QueryInsertionBehavior {
    #[default]
    /// Always insert the query to the search history.
    AlwaysInsert,
    /// Replace the previous query in the search history, if the new query contains the previous query.
    ReplacePreviousIfContains,
}

/// A cursor that stores an index to the currently selected query in the search history.
/// This can be passed to the search history to update the selection accordingly,
/// e.g. when using the up and down arrow keys to navigate the search history.
///
/// Note: The cursor can point to the wrong query, if the maximum length of the history is exceeded
/// and the old query is overwritten.
#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub struct SearchHistoryCursor {
    selection: Option<usize>,
}

impl SearchHistoryCursor {
    /// Resets the selection to `None`.
    pub fn reset(&mut self) {
        self.selection = None;
    }
}

#[derive(Debug, Clone)]
pub struct SearchHistory {
    history: VecDeque<String>,
    max_history_len: Option<usize>,
    insertion_behavior: QueryInsertionBehavior,
}

impl SearchHistory {
    pub fn new(max_history_len: Option<usize>, insertion_behavior: QueryInsertionBehavior) -> Self {
        SearchHistory {
            max_history_len,
            insertion_behavior,
            history: VecDeque::new(),
        }
    }

    pub fn add(&mut self, cursor: &mut SearchHistoryCursor, search_string: String) {
        if let Some(selected_ix) = cursor.selection {
            if self.history.get(selected_ix) == Some(&search_string) {
                return;
            }
        }

        if self.insertion_behavior == QueryInsertionBehavior::ReplacePreviousIfContains {
            if let Some(previously_searched) = self.history.back_mut() {
                if search_string.contains(previously_searched.as_str()) {
                    *previously_searched = search_string;
                    cursor.selection = Some(self.history.len() - 1);
                    return;
                }
            }
        }

        if let Some(max_history_len) = self.max_history_len {
            if self.history.len() >= max_history_len {
                self.history.pop_front();
            }
        }
        self.history.push_back(search_string);

        cursor.selection = Some(self.history.len() - 1);
    }

    pub fn next(&mut self, cursor: &mut SearchHistoryCursor) -> Option<&str> {
        let history_size = self.history.len();
        if history_size == 0 {
            return None;
        }

        let selected = cursor.selection?;
        if selected == history_size - 1 {
            return None;
        }
        let next_index = selected + 1;
        cursor.selection = Some(next_index);
        Some(&self.history[next_index])
    }

    pub fn current(&self, cursor: &SearchHistoryCursor) -> Option<&str> {
        cursor
            .selection
            .and_then(|selected_ix| self.history.get(selected_ix).map(|s| s.as_str()))
    }

    pub fn previous(&mut self, cursor: &mut SearchHistoryCursor) -> Option<&str> {
        let history_size = self.history.len();
        if history_size == 0 {
            return None;
        }

        let prev_index = match cursor.selection {
            Some(selected_index) => {
                if selected_index == 0 {
                    return None;
                } else {
                    selected_index - 1
                }
            }
            None => history_size - 1,
        };

        cursor.selection = Some(prev_index);
        Some(&self.history[prev_index])
    }
}
