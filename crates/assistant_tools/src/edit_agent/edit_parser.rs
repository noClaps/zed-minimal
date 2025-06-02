use derive_more::{Add, AddAssign};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(
    Clone, Debug, Default, PartialEq, Eq, Add, AddAssign, Serialize, Deserialize, JsonSchema,
)]
pub struct EditParserMetrics {
    pub tags: usize,
    pub mismatched_tags: usize,
}
