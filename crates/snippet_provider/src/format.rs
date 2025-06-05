use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
pub(crate) enum ListOrDirect {
    Single(String),
    List(Vec<String>),
}

impl From<ListOrDirect> for Vec<String> {
    fn from(list: ListOrDirect) -> Self {
        match list {
            ListOrDirect::Single(entry) => vec![entry],
            ListOrDirect::List(entries) => entries,
        }
    }
}

impl std::fmt::Display for ListOrDirect {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Single(v) => v.to_owned(),
                Self::List(v) => v.join("\n"),
            }
        )
    }
}
